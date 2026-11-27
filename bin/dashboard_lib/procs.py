"""
Finds the harness's live `claude -p` ticks in /proc and the issue each one works.
"""
import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import ClassVar

from dashboard_lib.models import ClaimRecord, LeaseSlot, ProcessInfo, TickProcess
from dashboard_lib.parsers import StateParsers
from dashboard_lib.redact import CommandRedactor


@dataclass(frozen=True)
class TickCandidate:
    """
    A harness tick before its issue is known: the process and what its descriptors say.
    """

    process: ProcessInfo
    slot: int | None
    log: str | None


class ProcessScanner:
    """
    Reads the process table and recognises harness ticks.

    A tick is a process whose argv[0] base name is claude, claude-code or node and argv[1] is `-p`, the local
    ticks tick.sh runs and bin/tick-pid.sh's default TICK_AGENT_EXES, and whose stdout is a `state/logs/tick-*.log`
    of this harness home, as tick.sh's runner redirects it; a `claude -p` of another harness home or outside any
    harness is left out. Its issue comes, cheapest first, from the newest `state/locks/claim.<issue>` naming its
    pid, then from a lease it holds, then from a cached GitHub claim-comment lookup the caller supplies.
    """

    CLAUDE_BINARIES: ClassVar[frozenset[str]] = frozenset({"claude", "claude-code", "node"})
    LANDING_MARK: ClassVar[str] = "This tick is the merge slot's landing tick"
    SLOT_LOCK_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"/locks/slot([0-9]{1,9})\.lock$")
    TOOL_COMMAND_MAX_CHARS: ClassVar[int] = 160

    def __init__(self, harness_state_dir: Path, proc_root: Path = Path("/proc")) -> None:
        """
        Args:
            harness_state_dir: The harness's `state/` directory, whose `logs/` holds the tick logs.
            proc_root: The proc filesystem; tests point it elsewhere.
        """
        self.harness_state_dir: Path = harness_state_dir
        self.proc_root: Path = proc_root
        self._clock_ticks_per_second: int = os.sysconf("SC_CLK_TCK")

    def scan_processes(self) -> dict[int, ProcessInfo]:
        """
        Reads every process's parent, start time and argv.

        Returns:
            Pid to process; processes that exit mid-read are left out.
        """
        boot_epoch: float = self._boot_epoch()
        processes: dict[int, ProcessInfo] = {}
        for entry in os.scandir(self.proc_root):
            if not entry.name.isdigit():
                continue
            try:
                stat_text: str = Path(entry.path, "stat").read_text()
                argv_bytes: bytes = Path(entry.path, "cmdline").read_bytes()
            except OSError:
                continue
            fields_after_name: list[str] = stat_text.rsplit(")", 1)[-1].split()
            if len(fields_after_name) < 20:
                continue
            processes[int(entry.name)] = ProcessInfo(
                pid=int(entry.name),
                ppid=int(fields_after_name[1]),
                start_epoch=boot_epoch + int(fields_after_name[19]) / self._clock_ticks_per_second,
                argv=tuple(part.decode("utf-8", "replace") for part in argv_bytes.split(b"\0") if part),
            )
        return processes

    def find_ticks(self, processes: dict[int, ProcessInfo]) -> tuple[TickCandidate, ...]:
        """
        Picks the harness ticks out of the process table.

        Args:
            processes: The table from scan_processes.

        Returns:
            One candidate a tick, with its slot lock and log when its descriptors name them.
        """
        logs_prefix: str = str(self.harness_state_dir / "logs" / "tick-")
        candidates: list[TickCandidate] = []
        for process in processes.values():
            if len(process.argv) < 2 or process.argv[1] != "-p":
                continue
            if os.path.basename(process.argv[0]) not in self.CLAUDE_BINARIES:
                continue
            descriptor_targets: dict[str, str] = self._descriptor_targets(process.pid)
            stdout_target: str | None = descriptor_targets.get("1")
            if stdout_target is None or not stdout_target.startswith(logs_prefix):
                continue
            slot: int | None = None
            for target in descriptor_targets.values():
                slot_match: re.Match[str] | None = self.SLOT_LOCK_PATTERN.search(target)
                if slot_match and target.startswith(str(self.harness_state_dir)):
                    slot = int(slot_match.group(1))
            candidates.append(TickCandidate(process, slot, os.path.basename(stdout_target)))
        return tuple(sorted(candidates, key=lambda candidate: candidate.process.start_epoch))

    def describe_ticks(
        self,
        candidates: tuple[TickCandidate, ...],
        processes: dict[int, ProcessInfo],
        claims: tuple[ClaimRecord, ...],
        lease_slots: tuple[LeaseSlot, ...],
        github_issue_by_pid: dict[int, int],
        now_epoch: float,
    ) -> tuple[TickProcess, ...]:
        """
        Names each tick's model, lane, age, issue and current tool call.

        Args:
            candidates: The ticks from find_ticks.
            processes: The process table, to walk each tick's tool shells.
            claims: Every recent claim file.
            lease_slots: The slots and device lease from the resource status.
            github_issue_by_pid: Issues found for a pid in GitHub claim comments.
            now_epoch: The current time.

        Returns:
            One record a tick, oldest first.
        """
        issue_by_pid: dict[int, tuple[int, str]] = self.map_pids_to_issues(claims, lease_slots, github_issue_by_pid)
        children_by_parent: dict[int, list[ProcessInfo]] = {}
        for process in processes.values():
            children_by_parent.setdefault(process.ppid, []).append(process)
        ticks: list[TickProcess] = []
        for candidate in candidates:
            argv: tuple[str, ...] = candidate.process.argv
            model: str = ""
            for index, argument in enumerate(argv[:-1]):
                if argument == "--model":
                    model = argv[index + 1]
            prompt: str = argv[2] if len(argv) > 2 else ""
            if prompt.startswith(self.LANDING_MARK):
                lane: str = "landing"
            elif ", the model lane:" in prompt.split("\n", 1)[0]:
                lane = "model"
            else:
                lane = "standard"
            issue_and_source: tuple[int, str] | None = issue_by_pid.get(candidate.process.pid)
            doing, doing_age_s, tool_shells = self._current_tool_call(candidate.process.pid, children_by_parent,
                                                                      now_epoch)
            ticks.append(
                TickProcess(
                    pid=candidate.process.pid,
                    model=model,
                    model_label=StateParsers.model_label(model),
                    lane=lane,
                    age_s=max(0, int(now_epoch - candidate.process.start_epoch)),
                    slot=candidate.slot,
                    log=candidate.log,
                    issue=issue_and_source[0] if issue_and_source else None,
                    issue_source=issue_and_source[1] if issue_and_source else None,
                    doing=doing,
                    doing_age_s=doing_age_s,
                    tool_shells=tool_shells,
                )
            )
        return tuple(ticks)

    def describe_holder(self, pid: int | None, processes: dict[int, ProcessInfo], tick_pids: frozenset[int]) -> str:
        """
        Names what a lease holder pid is: a tick, a script, or gone.

        Args:
            pid: The holder pid.
            processes: The process table.
            tick_pids: The live ticks' pids.

        Returns:
            `tick`, a redacted summary of the holder's command (`run.sh --issue 2490 ...`), or `dead`.
        """
        if pid is None:
            return ""
        if pid in tick_pids:
            return "tick"
        process: ProcessInfo | None = processes.get(pid)
        if process is None:
            return "dead"
        return CommandRedactor.summarize_argv(process.argv) or "?"

    @staticmethod
    def map_pids_to_issues(
        claims: tuple[ClaimRecord, ...],
        lease_slots: tuple[LeaseSlot, ...],
        github_issue_by_pid: dict[int, int],
    ) -> dict[int, tuple[int, str]]:
        """
        Maps a pid to the issue it works, cheapest source first.

        A tick takes `claim.sh acquire <issue>` before its claim comment, so the newest claim naming a pid is
        the issue it works now. A pid with no claim falls back to a lease it holds, then to the GitHub lookup.

        Args:
            claims: Claim files.
            lease_slots: Slots and the device lease.
            github_issue_by_pid: Issues found in GitHub claim comments.

        Returns:
            Pid to (issue, source), source being `claim`, `lease` or `github`.
        """
        mapping: dict[int, tuple[int, str]] = {}
        for pid, issue in github_issue_by_pid.items():
            mapping[pid] = (issue, "github")
        for lease_slot in lease_slots:
            if lease_slot.pid is not None and lease_slot.issue is not None:
                mapping[lease_slot.pid] = (lease_slot.issue, "lease")
        newest_since_by_pid: dict[int, str] = {}
        for claim in claims:
            if claim.since >= newest_since_by_pid.get(claim.pid, ""):
                newest_since_by_pid[claim.pid] = claim.since
                mapping[claim.pid] = (claim.issue, "claim")
        return mapping

    def _descriptor_targets(self, pid: int) -> dict[str, str]:
        """
        Reads where a process's file descriptors point.

        Args:
            pid: The process.

        Returns:
            Descriptor number to target path; empty when the process is gone or unreadable.
        """
        targets: dict[str, str] = {}
        descriptor_dir: Path = self.proc_root / str(pid) / "fd"
        try:
            entries: list[os.DirEntry[str]] = list(os.scandir(descriptor_dir))
        except OSError:
            return targets
        for entry in entries:
            try:
                targets[entry.name] = os.readlink(entry.path)
            except OSError:
                continue
        return targets

    def _current_tool_call(
        self, tick_pid: int, children_by_parent: dict[int, list[ProcessInfo]], now_epoch: float
    ) -> tuple[str | None, int | None, int]:
        """
        Finds the tick's youngest tool shell and a redacted summary of the command it runs; the raw command
        line never leaves this method.

        Args:
            tick_pid: The tick.
            children_by_parent: Parent pid to its child processes.
            now_epoch: The current time.

        Returns:
            The youngest tool call's summary, its age in seconds, and how many tool shells the tick has open.
        """
        tool_shells: list[ProcessInfo] = []
        for child in children_by_parent.get(tick_pid, []):
            joined: str = " ".join(child.argv[:4])
            if "mcp" in joined:
                continue
            if len(child.argv) >= 3 and os.path.basename(child.argv[0]) in ("bash", "sh", "zsh") \
                    and child.argv[1] == "-c":
                tool_shells.append(child)
        if not tool_shells:
            return None, None, 0
        youngest: ProcessInfo = max(tool_shells, key=lambda shell: shell.start_epoch)
        command: str | None = StateParsers.tool_command(youngest.argv[2])
        summary: str = ""
        if command is not None:
            summary = CommandRedactor.summarize(command) or "shell step"
        else:
            grandchildren: list[ProcessInfo] = children_by_parent.get(youngest.pid, [])
            summary = CommandRedactor.summarize_argv(grandchildren[0].argv) if grandchildren else "shell step"
        if len(summary) > self.TOOL_COMMAND_MAX_CHARS:
            summary = summary[: self.TOOL_COMMAND_MAX_CHARS - 1] + "\u2026"
        return summary, max(0, int(now_epoch - youngest.start_epoch)), len(tool_shells)

    def _boot_epoch(self) -> float:
        """
        Reads the boot time from /proc/stat.

        Returns:
            Boot time in epoch seconds.

        Raises:
            OSError: /proc/stat has no btime line.
        """
        for line in (self.proc_root / "stat").read_text().splitlines():
            if line.startswith("btime "):
                return float(line.split()[1])
        raise OSError("no btime in /proc/stat")
