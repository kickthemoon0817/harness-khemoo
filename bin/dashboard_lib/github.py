"""
Read-only GitHub queries through `gh`, with the token passed in the environment and kept out of every message.
"""
import json
import os
import subprocess
from typing import Any, ClassVar

from dashboard_lib.models import JsonDict


class GitHubError(RuntimeError):
    """
    A `gh` call failed; the message is already redacted.
    """


class GitHubClient:
    """
    Runs read-only GraphQL and REST queries for one repository.

    The token reaches `gh` only as `GH_TOKEN` in the child's environment. Every error message is passed through
    `redact` before it leaves this class, and `repr` never shows the token.
    """

    ISSUES_QUERY: ClassVar[str] = """
query($owner: String!, $name: String!, $label: String!, $after: String) {
  repository(owner: $owner, name: $name) {
    issues(first: 100, after: $after, states: OPEN, labels: [$label], orderBy: {field: UPDATED_AT, direction: DESC}) {
      totalCount
      pageInfo { hasNextPage endCursor }
      nodes {
        number title url updatedAt body
        labels(first: 30) { nodes { name } }
        comments(last: 1) { totalCount nodes { createdAt } }
      }
    }
  }
}
"""
    MERGED_QUERY: ClassVar[str] = """
query($search: String!) {
  search(query: $search, type: ISSUE, first: 50) {
    issueCount
    nodes { ... on PullRequest { number title url mergedAt baseRefName author { login } } }
  }
}
"""
    MAX_ISSUE_PAGES: ClassVar[int] = 5
    STATES_PER_QUERY: ClassVar[int] = 60

    def __init__(self, repo: str, token: str | None, gh_binary: str = "gh", timeout_s: float = 30.0) -> None:
        """
        Args:
            repo: `owner/name`.
            token: The token for `GH_TOKEN`; None lets `gh` use its own login.
            gh_binary: The `gh` executable.
            timeout_s: Per-call timeout.

        Raises:
            ValueError: The repo is not `owner/name`.
        """
        owner, separator, name = repo.partition("/")
        if not separator or not owner or not name:
            raise ValueError("GH_REPO must be owner/name")
        self.repo: str = repo
        self.owner: str = owner
        self.name: str = name
        self.gh_binary: str = gh_binary
        self.timeout_s: float = timeout_s
        self._token: str | None = token or None

    def __repr__(self) -> str:
        return f"GitHubClient(repo={self.repo!r}, token={'set' if self._token else 'unset'})"

    def redact(self, text: str) -> str:
        """
        Removes the token from a message.

        Args:
            text: Any text that may have passed through `gh`.

        Returns:
            The text with every occurrence of the token replaced.
        """
        return text.replace(self._token, "[redacted]") if self._token else text

    def child_environment(self, extra: dict[str, str] | None = None) -> dict[str, str]:
        """
        Builds the environment for a `gh` (or gh-calling) child process.

        Args:
            extra: Further variables to set.

        Returns:
            The current environment with the token, non-interactive `gh` settings and `extra`.
        """
        environment: dict[str, str] = dict(os.environ)
        environment.update({"GH_PROMPT_DISABLED": "1", "GH_NO_UPDATE_NOTIFIER": "1", "NO_COLOR": "1"})
        if self._token:
            environment["GH_TOKEN"] = self._token
        if extra:
            environment.update(extra)
        return environment

    def fetch_open_issues(self, label: str) -> list[JsonDict]:
        """
        Lists the repository's open issues carrying a label.

        Args:
            label: The label.

        Returns:
            GraphQL issue nodes, most recently updated first.
        """
        issues: list[JsonDict] = []
        cursor: str | None = None
        for _ in range(self.MAX_ISSUE_PAGES):
            variables: dict[str, str] = {"owner": self.owner, "name": self.name, "label": label}
            if cursor:
                variables["after"] = cursor
            data: JsonDict = self._graphql(self.ISSUES_QUERY, variables)
            connection: JsonDict = data["repository"]["issues"]
            issues.extend(connection["nodes"])
            if not connection["pageInfo"]["hasNextPage"]:
                break
            cursor = connection["pageInfo"]["endCursor"]
        return issues

    def fetch_states(self, numbers: list[int]) -> dict[int, str]:
        """
        Reads the state of issues or PRs by number.

        Args:
            numbers: Issue or PR numbers.

        Returns:
            Number to OPEN, CLOSED or MERGED; a number GitHub does not resolve is left out.
        """
        states: dict[int, str] = {}
        for start in range(0, len(numbers), self.STATES_PER_QUERY):
            chunk: list[int] = numbers[start:start + self.STATES_PER_QUERY]
            fields: str = " ".join(
                f"n{number}: issueOrPullRequest(number: {int(number)}) "
                "{ ... on Issue { state } ... on PullRequest { state } }"
                for number in chunk
            )
            query: str = f"query($owner: String!, $name: String!) {{ repository(owner: $owner, name: $name) {{ {fields} }} }}"
            data: JsonDict = self._graphql(query, {"owner": self.owner, "name": self.name}, allow_partial=True)
            repository: JsonDict = data.get("repository") or {}
            for number in chunk:
                node: Any = repository.get(f"n{number}")
                if isinstance(node, dict) and isinstance(node.get("state"), str):
                    states[number] = node["state"]
        return states

    def fetch_merged_pulls(self, since_iso: str) -> list[JsonDict]:
        """
        Lists PRs merged since a time.

        Args:
            since_iso: ISO-8601 UTC lower bound.

        Returns:
            PR nodes (number, title, url, mergedAt, baseRefName, author), newest merge first.
        """
        data: JsonDict = self._graphql(self.MERGED_QUERY, {"search": f"repo:{self.repo} is:pr is:merged merged:>={since_iso}"})
        pulls: list[JsonDict] = [node for node in data["search"]["nodes"] if node and node.get("number")]
        return sorted(pulls, key=lambda pull: pull.get("mergedAt") or "", reverse=True)

    def find_issue_for_pid(self, pid: int) -> int | None:
        """
        Finds the issue whose claim comment names a tick pid (`... pid <N> claiming` or `resuming`).

        Args:
            pid: The tick's pid.

        Returns:
            The issue number, or None when no comment names it.
        """
        output: str = self._run(["api", "-X", "GET", "search/issues", "-f",
                                 f'q=repo:{self.repo} "pid {int(pid)}" in:comments', "-f", "per_page=5"])
        items: list[JsonDict] = json.loads(output).get("items") or []
        for item in items:
            if "pull_request" not in item:
                return int(item["number"])
        return None

    def _graphql(self, query: str, variables: dict[str, str], allow_partial: bool = False) -> JsonDict:
        """
        Runs one GraphQL query.

        Args:
            query: The query text.
            variables: String variables.
            allow_partial: Keep the data when GitHub also reports errors (an unresolvable alias).

        Returns:
            The response's `data` object.

        Raises:
            GitHubError: The call failed or GitHub returned errors.
        """
        arguments: list[str] = ["api", "graphql", "-f", f"query={query}"]
        for key, value in variables.items():
            arguments += ["-f", f"{key}={value}"]
        output: str = self._run(arguments, accept_failure_with_output=allow_partial)
        try:
            document: JsonDict = json.loads(output)
        except json.JSONDecodeError as error:
            raise GitHubError(f"gh returned non-JSON output ({error.msg})") from None
        if document.get("errors") and not (allow_partial and document.get("data")):
            messages: str = "; ".join(str(entry.get("message", "")) for entry in document["errors"])[:300]
            raise GitHubError(self.redact(f"GraphQL errors: {messages}"))
        return document.get("data") or {}

    def _run(self, arguments: list[str], accept_failure_with_output: bool = False) -> str:
        """
        Runs `gh` with the token in its environment.

        Args:
            arguments: Arguments after `gh`.
            accept_failure_with_output: Return stdout on a non-zero exit when there is any.

        Returns:
            The call's stdout.

        Raises:
            GitHubError: `gh` is missing, timed out, or exited non-zero.
        """
        try:
            completed: subprocess.CompletedProcess[str] = subprocess.run(
                [self.gh_binary] + arguments, capture_output=True, text=True, timeout=self.timeout_s,
                env=self.child_environment(), stdin=subprocess.DEVNULL,
            )
        except FileNotFoundError:
            raise GitHubError(f"{self.gh_binary} not found") from None
        except subprocess.TimeoutExpired:
            raise GitHubError(f"gh timed out after {self.timeout_s:.0f} s") from None
        if completed.returncode != 0 and not (accept_failure_with_output and completed.stdout.strip()):
            detail_lines: list[str] = (completed.stderr or completed.stdout).strip().splitlines()
            message: str = detail_lines[-1] if detail_lines else f"exit {completed.returncode}"
            raise GitHubError(self.redact(f"gh exit {completed.returncode}: {message[:300]}"))
        return completed.stdout
