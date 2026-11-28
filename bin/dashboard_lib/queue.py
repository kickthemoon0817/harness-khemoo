"""
Groups the open `ai` issues the way the harness sees them.
"""
import re
from dataclasses import dataclass, replace
from typing import Any, ClassVar

from dashboard_lib.models import JsonDict


@dataclass(frozen=True)
class QueueLabels:
    """
    The label names the harness uses; the defaults are tick.sh's and claimable.sh's. An empty lane label turns
    the model lane off.
    """

    all_issues: str = "ai"
    queue: str = "ai"
    wip: str = "ai:wip"
    signoff: str = "ai:signoff"
    operator: str = "ai:operator"
    ready: str = "ai:ready"
    lane: str = ""
    first: str = "ai:first"
    low: str = "ai:low"


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
    Sorts open issues into running, claimable, held, operator, signoff and other.

    The claimable rule is bin/claimable.sh's. An issue is a candidate when it carries the queue label, no live
    local claim holds it, it is not parked for sign-off or for the operator, and every `Depends on` issue is
    closed; an `ai:wip` issue whose claimant is gone is a candidate again. claimable.sh also reads each `ai:wip`
    issue's claim comments for a live claimant pid; here a live claimant is a live tick holding that issue's
    claim file, which the tick takes before it comments.

    The first-before-low gate then holds candidates back. A low issue counts only when no other candidate counts
    and no first issue is claimable anywhere; a lane issue without the first label counts only while no first
    issue is claimable anywhere. "Anywhere" includes first issues outside the queue label, judged by the same
    candidate rule without it. A held-back candidate is listed in `held` with the reason.
    """

    GROUP_ORDER: ClassVar[tuple[str, ...]] = ("running", "claimable", "held", "operator", "signoff", "other")
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
        first_only_issues: list[JsonDict] | None = None,
    ) -> dict[str, list[QueueIssue]]:
        """
        Groups the issues.

        Args:
            raw_issues: GraphQL issue nodes (number, title, url, updatedAt, body, labels, comments).
            dependency_states: Issue or PR number to its state (OPEN, CLOSED, MERGED) for dependencies outside
                the open list.
            live_claim_pid_by_issue: Issues whose claim file names a live process, and that pid.
            tick_pid_by_issue: Issues a live tick works, and the tick's pid.
            first_only_issues: Open issues carrying the first label but not the all-issues label; they are not
                listed, only weighed by the gate.

        Returns:
            Group name to its issues, each group sorted for reading.
        """
        open_numbers: frozenset[int] = frozenset(int(issue["number"]) for issue in raw_issues)
        groups: dict[str, list[QueueIssue]] = {name: [] for name in self.GROUP_ORDER}
        first_claimable_outside_queue: bool = False
        listed_and_weighed: list[tuple[JsonDict, bool]] = [(raw, True) for raw in raw_issues] + \
            [(raw, False) for raw in first_only_issues or []]
        for raw_issue, is_listed in listed_and_weighed:
            issue: QueueIssue = self._issue_of(raw_issue, open_numbers, dependency_states, live_claim_pid_by_issue,
                                               tick_pid_by_issue)
            if self.labels.first in issue.labels and self.labels.queue not in issue.labels \
                    and self._is_candidate_without_queue_label(issue, live_claim_pid_by_issue, tick_pid_by_issue):
                first_claimable_outside_queue = True
            if is_listed:
                groups[issue.group].append(issue)
        groups["claimable"], groups["held"] = self._apply_first_gate(groups["claimable"],
                                                                     first_claimable_outside_queue)
        groups["claimable"].sort(key=lambda queued: (self.labels.first not in queued.labels,
                                                     self.labels.low in queued.labels, queued.number))
        for name in ("running", "held", "operator", "signoff", "other"):
            groups[name].sort(key=lambda queued: queued.last_comment_at or queued.updated_at or "", reverse=True)
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
            for number in re.findall(r"#([0-9]{1,9})\b", line)
        )

    def _apply_first_gate(self, candidates: list[QueueIssue],
                          first_claimable_outside_queue: bool) -> tuple[list[QueueIssue], list[QueueIssue]]:
        """
        Splits the candidates into those claimable.sh counts and those its first-before-low gate holds back.

        Args:
            candidates: Issues that pass the candidate rule.
            first_claimable_outside_queue: A first issue without the queue label passes the candidate rule.

        Returns:
            The counted issues and the held-back ones, each held one with its reason.
        """
        labels: QueueLabels = self.labels

        def is_new_lane_issue(issue: QueueIssue) -> bool:
            return bool(labels.lane) and labels.lane in issue.labels and labels.first not in issue.labels

        gated: list[QueueIssue] = [issue for issue in candidates
                                   if labels.low in issue.labels or is_new_lane_issue(issue)]
        first_waiting: bool = bool(gated) and (
            any(labels.first in issue.labels for issue in candidates)
            or (labels.queue != labels.first and first_claimable_outside_queue)
        )
        kept: list[QueueIssue] = [issue for issue in candidates if labels.low not in issue.labels
                                  and not (is_new_lane_issue(issue) and first_waiting)]
        if not kept and not first_waiting:
            kept = [issue for issue in candidates if labels.low in issue.labels]
        kept_numbers: frozenset[int] = frozenset(issue.number for issue in kept)
        held: list[QueueIssue] = []
        for issue in candidates:
            if issue.number in kept_numbers:
                continue
            if labels.low in issue.labels:
                reason: str = (f"held: {labels.low} waits while a {labels.first} issue is claimable" if first_waiting
                               else f"held: {labels.low} waits while other work is claimable")
            else:
                reason = f"held: a new {labels.lane} issue waits while a {labels.first} issue is claimable"
            held.append(replace(issue, group="held", reason=reason))
        return kept, held

    def _issue_of(
        self,
        raw_issue: JsonDict,
        open_numbers: frozenset[int],
        dependency_states: dict[int, str],
        live_claim_pid_by_issue: dict[int, int],
        tick_pid_by_issue: dict[int, int],
    ) -> QueueIssue:
        """
        Builds one issue record and its group before the gate.

        Args:
            raw_issue: The GraphQL issue node.
            open_numbers: Numbers of the open listed issues.
            dependency_states: States of dependencies outside the list.
            live_claim_pid_by_issue: Live claim holders.
            tick_pid_by_issue: Issues live ticks work.

        Returns:
            The issue.
        """
        number: int = int(raw_issue["number"])
        labels: tuple[str, ...] = tuple(label["name"] for label in raw_issue.get("labels", {}).get("nodes", []))
        depends_on: tuple[int, ...] = self.parse_dependencies(raw_issue.get("body") or "")
        open_dependencies: tuple[int, ...] = tuple(
            dependency for dependency in dict.fromkeys(depends_on)
            if dependency in open_numbers or dependency_states.get(dependency, "OPEN") not in ("CLOSED", "MERGED")
        )
        group, reason = self._group_of(number, labels, open_dependencies, live_claim_pid_by_issue, tick_pid_by_issue)
        comments: dict[str, Any] = raw_issue.get("comments") or {}
        comment_nodes: list[JsonDict] = comments.get("nodes") or []
        return QueueIssue(
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

    def _is_candidate_without_queue_label(self, issue: QueueIssue, live_claim_pid_by_issue: dict[int, int],
                                          tick_pid_by_issue: dict[int, int]) -> bool:
        """
        Applies the candidate rule but for the queue label, as claimable.sh does for first issues outside it.

        Args:
            issue: The issue.
            live_claim_pid_by_issue: Live claim holders.
            tick_pid_by_issue: Issues live ticks work.

        Returns:
            True when the issue would be a candidate if it carried the queue label.
        """
        return (issue.number not in tick_pid_by_issue and issue.number not in live_claim_pid_by_issue
                and self.labels.signoff not in issue.labels and self.labels.operator not in issue.labels
                and not issue.open_dependencies)

    def _group_of(
        self,
        number: int,
        labels: tuple[str, ...],
        open_dependencies: tuple[int, ...],
        live_claim_pid_by_issue: dict[int, int],
        tick_pid_by_issue: dict[int, int],
    ) -> tuple[str, str]:
        """
        Decides one issue's group before the gate.

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
