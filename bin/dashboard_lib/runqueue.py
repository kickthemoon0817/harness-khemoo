"""
Reads bin/arm_queue.sh run folders: whether a queue is alive, waiting for a card, or running an arm, and where.
"""
import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import ClassVar

from dashboard_lib.models import ProcessInfo, RunQueue
from dashboard_lib.parsers import StateParsers


@dataclass(frozen=True)
class LeaseLogInfo:
    """
    What a run folder's `lease.log` says about its holder and its last lease answers.
    """

    holder_pid: int | None
    want: str | None
    busy_since: str | None
    busy_reason: str | None
    last_host: str | None


@dataclass(frozen=True)
class StatusInfo:
    """
    What a run folder's `status` says: the arm running now, the granted host, and whether the queue ended.
    """

    arm: str | None
    arm_host: str | None
    arm_started: str | None
    granted_host: str | None
    finished_at: str | None


class RunQueueReader:
    """
    Finds the run folders `<evidence root>/<packet>/runs*/` written lately and reads their state.

    `lease.log` gives the holder (its `holder <pid> start` line) and the lease answers, stamped `<UTC> <answer>`;
    the BUSY streak is the run of stamped BUSY answers at its end, as bin/stall_watch.sh reads it. `status` gives
    the arm lines (`<arm> start <UTC> ... host=<host>` until `<arm> rc=<n> end`, a void, a skip or a failed
    bring-up), `granted:` lines, and `RELEASED <queue> <UTC>`, always its last line once the queue ends.
    """

    STAMP_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z) (.*)$")
    HOLDER_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^holder (\d+) start ")
    WANT_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"\bwant=(\S+)")
    REMOTE_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"\bremote=(\S+)")
    ARM_START_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        r"^(\S+) start (\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z)\b(.*)$")
    ARM_END_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        r"^(\S+) (?:rc=\d+ end|void\d+:|skipped:|up failed|sidecar failed|no sidecar)")
    QUEUE_STOP_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^(?:STOP|YIELD|NO-LEASE|DEADLINE|nucleus not ok)\b")
    RELEASED_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^RELEASED \S+ (\S+)")
    HOST_FIELD_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"\bhost=(\S+)")
    DOCKER_HOST_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"\bDOCKER_HOST=ssh://(\S+)")
    ISSUE_ARGUMENT_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"(?:^|\s)--issue\s+(\d+)\b")

    def __init__(self, evidence_root: Path, window_s: float) -> None:
        """
        Args:
            evidence_root: The folder holding the evidence packets.
            window_s: Only run folders written within this many seconds are read.
        """
        self.evidence_root: Path = evidence_root
        self.window_s: float = window_s

    def read_queues(self, processes: dict[int, ProcessInfo], now: float) -> tuple[RunQueue, ...]:
        """
        Reads every run folder written within the window.

        Args:
            processes: The process table, to tell a live holder.
            now: The current epoch.

        Returns:
            One record a run folder, newest first.
        """
        queues: list[RunQueue] = []
        try:
            packets: list[os.DirEntry[str]] = [entry for entry in os.scandir(self.evidence_root) if entry.is_dir()]
        except OSError:
            return ()
        for packet in packets:
            try:
                runs_folders: list[os.DirEntry[str]] = [entry for entry in os.scandir(packet.path)
                                                        if entry.name.startswith("runs") and entry.is_dir()]
            except OSError:
                continue
            for runs in runs_folders:
                updated: float = self._newest_mtime(Path(runs.path))
                if now - updated > self.window_s:
                    continue
                queues.append(self.read_queue(Path(runs.path), processes, updated))
        return tuple(sorted(queues, key=lambda queue: queue.updated_epoch, reverse=True))

    def read_queue(self, runs_path: Path, processes: dict[int, ProcessInfo], updated: float) -> RunQueue:
        """
        Reads one run folder.

        Args:
            runs_path: `<packet>/runs*`.
            processes: The process table.
            updated: When the folder's logs were last written.

        Returns:
            The queue's state: running, waiting_card, starting, finished or stopped.
        """
        lease: LeaseLogInfo = self.parse_lease_log(self._read_text(runs_path / "lease.log"))
        status: StatusInfo = self.parse_status(self._read_text(runs_path / "status"))
        holder: ProcessInfo | None = processes.get(lease.holder_pid) if lease.holder_pid is not None else None
        alive: bool = holder is not None and any("arm_queue.sh" in argument for argument in holder.argv[:3])
        issue: int | None = StateParsers.issue_from_path(str(runs_path))
        if issue is None and holder is not None:
            issue_match: re.Match[str] | None = self.ISSUE_ARGUMENT_PATTERN.search(" ".join(holder.argv))
            issue = int(issue_match.group(1)) if issue_match else None
        if not alive:
            state: str = "finished" if status.finished_at else "stopped"
        elif status.arm is not None:
            state = "running"
        elif lease.busy_since is not None:
            state = "waiting_card"
        else:
            state = "starting"
        return RunQueue(
            packet=runs_path.parent.name,
            runs=runs_path.name,
            issue=issue,
            holder_pid=lease.holder_pid,
            alive=alive,
            state=state,
            want=lease.want,
            arm=status.arm,
            arm_host=status.arm_host,
            arm_started=status.arm_started,
            busy_since=lease.busy_since,
            busy_reason=lease.busy_reason,
            granted_host=status.granted_host or lease.last_host,
            finished_at=status.finished_at,
            updated_epoch=updated,
        )

    @staticmethod
    def parse_lease_log(text: str) -> LeaseLogInfo:
        """
        Parses `lease.log`.

        Args:
            text: The file's text.

        Returns:
            The last holder and what it wants, the BUSY streak at the end (its first time and the last reason),
            and the host of the last grant.
        """
        holder_pid: int | None = None
        want: str | None = None
        busy_since: str | None = None
        busy_reason: str | None = None
        last_host: str | None = None
        for line in text.splitlines():
            holder_match: re.Match[str] | None = RunQueueReader.HOLDER_PATTERN.match(line)
            if holder_match:
                holder_pid = int(holder_match.group(1))
                want_match: re.Match[str] | None = RunQueueReader.WANT_PATTERN.search(line)
                want = want_match.group(1) if want_match else None
                busy_since = busy_reason = None
                continue
            stamp_match: re.Match[str] | None = RunQueueReader.STAMP_PATTERN.match(line)
            if stamp_match is None:
                continue
            answer: str = stamp_match.group(2)
            if answer.startswith("BUSY"):
                if busy_since is None:
                    busy_since = stamp_match.group(1)
                busy_reason = answer[4:].strip()
                continue
            busy_since = busy_reason = None
            if "ACQUIRED" in answer:
                remote_match: re.Match[str] | None = RunQueueReader.REMOTE_PATTERN.search(answer)
                last_host = remote_match.group(1) if remote_match else "local"
        return LeaseLogInfo(holder_pid, want, busy_since, busy_reason, last_host)

    @staticmethod
    def parse_status(text: str) -> StatusInfo:
        """
        Parses `status`.

        Args:
            text: The file's text.

        Returns:
            The arm running at its end (with its host and start), the last granted host, and when the queue
            released, if its last word is a release.
        """
        arm: str | None = None
        arm_host: str | None = None
        arm_started: str | None = None
        granted_host: str | None = None
        finished_at: str | None = None
        for line in text.splitlines():
            if line.startswith("granted: "):
                docker_match: re.Match[str] | None = RunQueueReader.DOCKER_HOST_PATTERN.search(line)
                granted_host = docker_match.group(1) if docker_match else "local"
                finished_at = None
                continue
            released_match: re.Match[str] | None = RunQueueReader.RELEASED_PATTERN.match(line)
            if released_match:
                finished_at = released_match.group(1)
                arm = arm_host = arm_started = None
                continue
            start_match: re.Match[str] | None = RunQueueReader.ARM_START_PATTERN.match(line)
            if start_match:
                arm, arm_started = start_match.group(1), start_match.group(2)
                host_match: re.Match[str] | None = RunQueueReader.HOST_FIELD_PATTERN.search(start_match.group(3))
                arm_host = host_match.group(1) if host_match else granted_host
                finished_at = None
                continue
            end_match: re.Match[str] | None = RunQueueReader.ARM_END_PATTERN.match(line)
            if (end_match and end_match.group(1) == arm) or RunQueueReader.QUEUE_STOP_PATTERN.match(line):
                arm = arm_host = arm_started = None
        return StatusInfo(arm, arm_host, arm_started, granted_host, finished_at)

    @staticmethod
    def _newest_mtime(runs_path: Path) -> float:
        """
        Args:
            runs_path: A run folder.

        Returns:
            The newest modification time of its `status` and `lease.log`, 0 when neither exists.
        """
        newest: float = 0.0
        for name in ("status", "lease.log"):
            try:
                newest = max(newest, (runs_path / name).stat().st_mtime)
            except OSError:
                continue
        return newest

    @staticmethod
    def _read_text(path: Path) -> str:
        """
        Args:
            path: A file.

        Returns:
            Its text, or an empty string when it is missing.
        """
        try:
            return path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return ""
