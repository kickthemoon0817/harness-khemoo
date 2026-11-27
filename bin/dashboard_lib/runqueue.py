"""
Reads the instance's run folders: whether a queue is alive, waiting for a device, or running an arm, and where.
"""
import glob
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
    Finds the run folders RUN_QUEUE_GLOB matches that were written lately and reads their state; a folder's parent
    is its packet.

    `lease.log` gives the holder (its `holder <pid> start` line) and the lease answers, stamped `<UTC> <answer>`;
    the BUSY streak is the run of stamped BUSY answers at its end. `status` gives the arm lines
    (`<arm> start <UTC> ... host=<host>` until `<arm> rc=<n> end`, a void, a skip, a failed bring-up, or a line
    starting with one of the stop words), `granted:` lines, and `RELEASED <queue> <UTC>`, always its last line once
    the queue ends.
    """

    STAMP_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z) (.*)$")
    HOLDER_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^holder (\d+) start ")
    WANT_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"\bwant=(\S+)")
    REMOTE_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"\bremote=(\S+)")
    ARM_START_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        r"^(\S+) start (\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z)\b(.*)$")
    ARM_END_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        r"^(\S+) (?:rc=\d+ end|void\d+:|skipped:|up failed|sidecar failed|no sidecar)")
    RELEASED_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^RELEASED \S+ (\S+)")
    HOST_FIELD_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"\bhost=(\S+)")
    DOCKER_HOST_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"\bDOCKER_HOST=ssh://(\S+)")
    ISSUE_ARGUMENT_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"(?:^|\s)--issue\s+(\d+)\b")

    def __init__(self, folder_glob: str, holder_name: str, stop_words: tuple[str, ...], window_s: float) -> None:
        """
        Args:
            folder_glob: RUN_QUEUE_GLOB, matching the run folders; `~` is expanded.
            holder_name: RUN_QUEUE_HOLDER, a word in a live holder's first three arguments; empty takes any live
                holder pid.
            stop_words: RUN_QUEUE_STOP_WORDS, the words that end the running arm when a `status` line starts
                with one.
            window_s: Only run folders written within this many seconds are read.
        """
        self.folder_glob: str = folder_glob
        self.holder_name: str = holder_name
        self.window_s: float = window_s
        self.stop_pattern: re.Pattern[str] | None = re.compile(
            "^(?:" + "|".join(re.escape(word) for word in stop_words) + r")(?!\w)") if stop_words else None

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
        for folder in glob.glob(os.path.expanduser(self.folder_glob)):
            runs_path: Path = Path(folder)
            if not runs_path.is_dir():
                continue
            updated: float = self._newest_mtime(runs_path)
            if now - updated > self.window_s:
                continue
            queues.append(self.read_queue(runs_path, processes, updated))
        return tuple(sorted(queues, key=lambda queue: queue.updated_epoch, reverse=True))

    def read_queue(self, runs_path: Path, processes: dict[int, ProcessInfo], updated: float) -> RunQueue:
        """
        Reads one run folder.

        Args:
            runs_path: A run folder.
            processes: The process table.
            updated: When the folder's logs were last written.

        Returns:
            The queue's state: running, waiting_device, starting, finished or stopped.
        """
        lease: LeaseLogInfo = self.parse_lease_log(self._read_text(runs_path / "lease.log"))
        status: StatusInfo = self.parse_status(self._read_text(runs_path / "status"), self.stop_pattern)
        holder: ProcessInfo | None = processes.get(lease.holder_pid) if lease.holder_pid is not None else None
        alive: bool = holder is not None and (not self.holder_name
                                              or any(self.holder_name in argument for argument in holder.argv[:3]))
        issue: int | None = StateParsers.issue_from_path(str(runs_path))
        if issue is None and holder is not None:
            issue_match: re.Match[str] | None = self.ISSUE_ARGUMENT_PATTERN.search(" ".join(holder.argv))
            issue = int(issue_match.group(1)) if issue_match else None
        if not alive:
            state: str = "finished" if status.finished_at else "stopped"
        elif status.arm is not None:
            state = "running"
        elif lease.busy_since is not None:
            state = "waiting_device"
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
    def parse_status(text: str, stop_pattern: re.Pattern[str] | None = None) -> StatusInfo:
        """
        Parses `status`.

        Args:
            text: The file's text.
            stop_pattern: Matches a line that ends the running arm, besides its own end line.

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
            if (end_match and end_match.group(1) == arm) or (stop_pattern and stop_pattern.match(line)):
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
