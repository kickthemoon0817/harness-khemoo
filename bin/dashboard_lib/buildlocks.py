"""
Reads the compile lanes: each host's build lock, who holds it and who waits in line.
"""
import json
import os
import re
import subprocess
from pathlib import Path
from typing import ClassVar

from dashboard_lib.models import BuildLane, LockParty, ProcessInfo
from dashboard_lib.parsers import StateParsers


class BuildLockReader:
    """
    Reads `<lock dir>/worv-dev-build-remote-<name>.lock` for each remote host and `worv-dev-build.lock` for this host.

    The holder is the pid tools/dev/iter.sh writes into the lock file (`pid <n>, <tree>, <extensions>`), while it
    lives: lslocks cannot name a holder whose descriptor a child inherited. The waiters are lslocks' blocked
    entries on the file. A party's issue comes from its worktree (`ai-worktrees/<prefix>-<issue>`), read from
    the lock file for the holder and from the working directory for a waiter, else from the tick above it.
    """

    LOCK_FILE_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^pid (\d+), ([^,]*)")

    def __init__(self, lock_names: dict[str, str], lock_dir: Path, timeout_s: float) -> None:
        """
        Args:
            lock_names: Remote host to its lock name (`user@host-a` to `a`), bin/build_host.sh's
                BUILD_LOCK_NAMES.
            lock_dir: Where the locks live.
            timeout_s: lslocks timeout.
        """
        self.lock_names: dict[str, str] = lock_names
        self.lock_dir: Path = lock_dir
        self.timeout_s: float = timeout_s

    def read_lanes(self, hosts: list[str], processes: dict[int, ProcessInfo],
                   issue_by_tick_pid: dict[int, int]) -> tuple[BuildLane, ...]:
        """
        Reads the lane of this host and of each remote host.

        Args:
            hosts: The remote hosts, in the harness's order.
            processes: The process table.
            issue_by_tick_pid: Live ticks' pids to the issues they work.

        Returns:
            One lane a host, this host first.

        Raises:
            OSError: lslocks could not run.
        """
        completed: subprocess.CompletedProcess[str] = subprocess.run(
            ["lslocks", "-J", "-o", "PID,MODE,PATH"], capture_output=True, text=True, timeout=self.timeout_s,
            stdin=subprocess.DEVNULL,
        )
        waiting_pids_by_path: dict[str, list[int]] = self.parse_lslocks_waiters(completed.stdout)
        lanes: list[BuildLane] = []
        named_locks: list[tuple[str, str, Path]] = [("local", "local", self.lock_dir / "worv-dev-build.lock")]
        for host in hosts:
            name: str = self.lock_names.get(host) or re.sub(r"[^A-Za-z0-9]", "-", host.split("@", 1)[-1])
            named_locks.append((name, host, self.lock_dir / f"worv-dev-build-remote-{name}.lock"))
        for name, host, path in named_locks:
            holder: LockParty | None = None
            holder_pid, holder_tree = self.parse_lock_file(self._read_text(path))
            if holder_pid is not None and holder_pid in processes:
                holder = LockParty(holder_pid, StateParsers.issue_from_path(holder_tree or "")
                                   or self._tick_issue(holder_pid, processes, issue_by_tick_pid),
                                   os.path.basename(holder_tree.rstrip("/")) if holder_tree else None, True)
            waiters: list[LockParty] = []
            for pid in waiting_pids_by_path.get(str(path), []):
                working_directory: str = self._working_directory(pid)
                waiters.append(LockParty(pid, StateParsers.issue_from_path(working_directory)
                                         or self._tick_issue(pid, processes, issue_by_tick_pid),
                                         os.path.basename(working_directory) or None, pid in processes))
            lanes.append(BuildLane(name, host, str(path), holder, tuple(waiters)))
        return tuple(lanes)

    @staticmethod
    def parse_lslocks_waiters(text: str) -> dict[str, list[int]]:
        """
        Parses `lslocks -J -o PID,MODE,PATH` for blocked entries (a mode ending in `*`).

        Args:
            text: lslocks' JSON output.

        Returns:
            Lock path to the pids waiting on it, oldest pid first.
        """
        try:
            document: object = json.loads(text) if text.strip() else {}
        except json.JSONDecodeError:
            return {}
        entries: list[object] = document.get("locks", []) if isinstance(document, dict) else []
        waiters: dict[str, list[int]] = {}
        for entry in entries:
            if not isinstance(entry, dict) or not str(entry.get("mode", "")).endswith("*"):
                continue
            path: object = entry.get("path")
            pid: object = entry.get("pid")
            if isinstance(path, str) and isinstance(pid, int):
                waiters.setdefault(path, []).append(pid)
        return {path: sorted(set(pids)) for path, pids in waiters.items()}

    @staticmethod
    def parse_lock_file(text: str) -> tuple[int | None, str | None]:
        """
        Parses the holder line iter.sh writes into a build lock: `pid <n>, <tree>, <extensions>`.

        Args:
            text: The lock file's text.

        Returns:
            The pid and the tree, or (None, None).
        """
        match: re.Match[str] | None = BuildLockReader.LOCK_FILE_PATTERN.match(text.strip())
        return (int(match.group(1)), match.group(2).strip()) if match else (None, None)

    @staticmethod
    def _tick_issue(pid: int, processes: dict[int, ProcessInfo], issue_by_tick_pid: dict[int, int]) -> int | None:
        """
        Walks a process's parents up to a live tick.

        Args:
            pid: The process.
            processes: The process table.
            issue_by_tick_pid: Live ticks' pids to their issues.

        Returns:
            The issue of the first tick above it, or None.
        """
        current: int = pid
        for _ in range(64):
            if current in issue_by_tick_pid:
                return issue_by_tick_pid[current]
            process: ProcessInfo | None = processes.get(current)
            if process is None or process.ppid <= 1:
                return None
            current = process.ppid
        return None

    @staticmethod
    def _working_directory(pid: int) -> str:
        """
        Args:
            pid: A process.

        Returns:
            Its working directory, or an empty string when it cannot be read.
        """
        try:
            return os.readlink(f"/proc/{pid}/cwd")
        except OSError:
            return ""

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
