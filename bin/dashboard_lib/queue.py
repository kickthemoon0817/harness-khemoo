"""
Groups the open `ai` issues the way the harness sees them.
"""
import re
from dataclasses import dataclass
from typing import Any, ClassVar

from dashboard_lib.models import JsonDict


@dataclass(frozen=True)
class QueueLabels:
    """
    The label names the harness uses; the defaults are tick.sh's and claimable.sh's.
    """

    all_issues: str = "ai"
    queue: str = "ai"
    wip: str = "ai:wip"
    signoff: str = "ai:signoff"
    operator: str = "ai:operator"
    ready: str = "ai:ready"
    fable: str = "ai:fable"


@dataclass(frozen=True)
class QueueIssue:
    """
    One open issue, its group and why it is there.
    """

    number: int
    title: str
    url: str
    labels: tuple[str, ...]
    updated_at: str | None
    last_comment_at: str | None
    comment_count: int
    depends_on: tuple[int, ...]
    open_dependencies: tuple[int, ...]
    group: str
    reason: str
    tick_pid: int | None


class IssueQueue:
    """
    Sorts open issues into running, claimable, operator, signoff and other.

    The claimable rule is bin/claimable.sh's: an issue carrying the queue label, held by no live local claim, not
    parked for sign-off or for the operator, and whose every `Depends on` issue is closed. An `ai:wip` issue whose
    claimant is gone is claimable again. claimable.sh also reads each `ai:wip` issue's claim comments for a live
    claimant pid; here a live claimant is a live tick holding that issue's claim file, which the tick takes
    before it comments.
    """

    GROUP_ORDER: ClassVar[tuple[str, ...]] = ("running", "claimable", "operator", "signoff", "other")
    DEPENDS_LINE_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"Depends on[^:\n]*:\s*(.*)")

    def __init__(self, labels: QueueLabels) -> None:
        """
        Args:
            labels: The harness's label names.
        """
        self.labels: QueueLabels = labels

    def classify(
        self,
        raw_issues: list[JsonDict],
        dependency_states: dict[int, str],
        live_claim_pid_by_issue: dict[int, int],
        tick_pid_by_issue: dict[int, int],
    ) -> dict[str, list[QueueIssue]]:
        """
        Groups the issues.

        Args:
            raw_issues: GraphQL issue nodes (number, title, url, updatedAt, body, labels, comments).
            dependency_states: Issue or PR number to its state (OPEN, CLOSED, MERGED) for dependencies outside
                the open list.
            live_claim_pid_by_issue: Issues whose claim file names a live process, and that pid.
            tick_pid_by_issue: Issues a live tick works, and the tick's pid.

        Returns:
            Group name to its issues, each group sorted for reading.
        """
        open_numbers: frozenset[int] = frozenset(int(issue["number"]) for issue in raw_issues)
        groups: dict[str, list[QueueIssue]] = {name: [] for name in self.GROUP_ORDER}
        for raw_issue in raw_issues:
            number: int = int(raw_issue["number"])
            labels: tuple[str, ...] = tuple(label["name"] for label in raw_issue.get("labels", {}).get("nodes", []))
            depends_on: tuple[int, ...] = self.parse_dependencies(raw_issue.get("body") or "")
            open_dependencies: tuple[int, ...] = tuple(
                dependency for dependency in dict.fromkeys(depends_on)
                if dependency in open_numbers or dependency_states.get(dependency, "OPEN") not in ("CLOSED", "MERGED")
            )
            group, reason = self._group_of(number, labels, open_dependencies, live_claim_pid_by_issue,
                                           tick_pid_by_issue)
            comments: dict[str, Any] = raw_issue.get("comments") or {}
            comment_nodes: list[JsonDict] = comments.get("nodes") or []
            groups[group].append(
                QueueIssue(
                    number=number,
                    title=str(raw_issue.get("title", "")),
                    url=str(raw_issue.get("url", "")),
                    labels=labels,
                    updated_at=raw_issue.get("updatedAt"),
                    last_comment_at=comment_nodes[-1].get("createdAt") if comment_nodes else None,
                    comment_count=int(comments.get("totalCount") or 0),
                    depends_on=depends_on,
                    open_dependencies=open_dependencies,
                    group=group,
                    reason=reason,
                    tick_pid=tick_pid_by_issue.get(number),
                )
            )
        groups["claimable"].sort(key=lambda issue: ("ai:first" not in issue.labels, "ai:low" in issue.labels,
                                                    issue.number))
        for name in ("running", "operator", "signoff", "other"):
            groups[name].sort(key=lambda issue: issue.last_comment_at or issue.updated_at or "", reverse=True)
        return groups

    @staticmethod
    def parse_dependencies(body: str) -> tuple[int, ...]:
        """
        Reads every `Depends on ...: #a, #b` line of an issue body, as claimable.sh does.

        Args:
            body: The issue body.

        Returns:
            The issue numbers named, in order, repeats kept.
        """
        return tuple(
            int(number)
            for line in IssueQueue.DEPENDS_LINE_PATTERN.findall(body)
            for number in re.findall(r"#(\d+)", line)
        )

    def _group_of(
        self,
        number: int,
        labels: tuple[str, ...],
        open_dependencies: tuple[int, ...],
        live_claim_pid_by_issue: dict[int, int],
        tick_pid_by_issue: dict[int, int],
    ) -> tuple[str, str]:
        """
        Decides one issue's group.

        Args:
            number: The issue.
            labels: Its labels.
            open_dependencies: Its dependencies still open.
            live_claim_pid_by_issue: Live claim holders.
            tick_pid_by_issue: Issues live ticks work.

        Returns:
            The group and a short reason.
        """
        if number in tick_pid_by_issue:
            return "running", f"tick pid {tick_pid_by_issue[number]}"
        if number in live_claim_pid_by_issue:
            return "other", f"claimed by live pid {live_claim_pid_by_issue[number]}"
        if self.labels.operator in labels:
            return "operator", "waits on the operator"
        if self.labels.signoff in labels:
            return "signoff", "waits for sign-off"
        if self.labels.queue not in labels:
            return "other", f"not labelled {self.labels.queue}"
        if open_dependencies:
            return "other", "waits on " + ", ".join(f"#{dependency}" for dependency in open_dependencies)
        if self.labels.wip in labels:
            return "claimable", "resumable: no live claimant"
        return "claimable", "ready to claim"
