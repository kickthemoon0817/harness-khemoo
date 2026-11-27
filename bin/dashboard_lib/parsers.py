"""
Pure parsers for the harness state files and command outputs the dashboard reads.
"""
import json
import re
from datetime import datetime, timezone
from typing import ClassVar

from dashboard_lib.models import (
    AllocatorLine,
    ClaimRecord,
    ContainerInfo,
    DeviceProcess,
    DeviceReading,
    HostHealthRow,
    HostStreak,
    LeaseSlot,
    LeaseStatus,
    LeaseWait,
    MachineProbe,
    RemoteCard,
    StallLine,
    UsageReading,
)


class StateParsers:
    """
    Turns harness text (TSV rows, the resource status, logs, probe output) into records.

    Every parser takes text and returns records; none reads a file. A line that does not match its format is
    skipped (or, for the resource status, kept as an unparsed line) so one malformed row never hides the rest.
    """

    HOST_HEALTH_COLUMNS: ClassVar[int] = 9
    REMOTE_CARD_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        r"^remote (?:device|card|host): (\S+) \(slots (\d+)-(\d+)\)$")
    LOCAL_OFF_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^local (?:device|card|host): off\b")
    HARNESS_ONLY_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^harness ticks: (.*?) only\b")
    SLOT_FREE_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^slot (\d+) free$")
    SLOT_HELD_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        r"^(?:slot (\d+)|device) (HELD|ORPHAN) (\S+) (\d+) (\S+)((?: \S+)*?) \((alive|pid dead)\)$"
    )
    WAIT_PATTERN: ClassVar[re.Pattern[str]] = re.compile(
        r"^wait (whole-device|whole-card|whole-pool|single-slot) issue (\d+) for (-?\d+) min(?: \((.*)\))?$"
    )
    STALL_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^(\S+) STALL (\S+) (\S+) ?(.*)$")
    ALLOCATOR_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^(\d{8}T\d{6}Z) (.*)$")
    ALLOCATOR_FIELD_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"\b([a-z_]+)=([^\s%]+)%?")
    MODEL_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"^claude-([a-z]+)-(\d+)(?:-(\d{1,2}))?(?:-\d{8})?$")
    TOOL_EVAL_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"\beval '(.*)' < /dev/null", re.DOTALL)
    DOCKER_CGROUP_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"docker[-/]([0-9a-f]{12})")
    PACKET_ISSUE_PATTERN: ClassVar[re.Pattern[str]] = re.compile(r"(?:^|/)\d{4}-\d{2}-\d{2}-(\d+)-[^/]*(?:/|$)")

    @staticmethod
    def parse_iso_epoch(iso_text: str) -> float:
        """
        Converts an ISO-8601 UTC time (`2026-11-26T01:27:01Z`) to epoch seconds.

        Args:
            iso_text: The time; a trailing `Z` means UTC.

        Returns:
            Epoch seconds.
        """
        parsed: datetime = datetime.fromisoformat(iso_text.replace("Z", "+00:00"))
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=timezone.utc)
        return parsed.timestamp()

    @staticmethod
    def parse_host_health_tsv(text: str) -> tuple[HostHealthRow, ...]:
        """
        Parses `state/host_health.tsv`.

        Args:
            text: The file's text, header included.

        Returns:
            One row a well-formed line, in file order.
        """
        rows: list[HostHealthRow] = []
        for line in text.splitlines():
            columns: list[str] = line.split("\t")
            if len(columns) != StateParsers.HOST_HEALTH_COLUMNS or columns[0] == "time":
                continue
            try:
                epoch: float = StateParsers.parse_iso_epoch(columns[0])
            except ValueError:
                continue
            upload_text: str = columns[3]
            rows.append(
                HostHealthRow(
                    time=columns[0],
                    epoch=epoch,
                    host=columns[1],
                    connect_s=StateParsers._optional_float(columns[2]),
                    upload_mb_s=StateParsers._optional_float(upload_text),
                    upload_timed_out=upload_text == "timeout",
                    load1=StateParsers._optional_float(columns[4]),
                    nproc=StateParsers._optional_int(columns[5]),
                    gpu_util_pct=StateParsers._optional_float(columns[6]),
                    gpu_mem_used_mb=StateParsers._optional_float(columns[7]),
                    status=columns[8].strip(),
                )
            )
        return tuple(rows)

    @staticmethod
    def parse_streaks(text: str) -> dict[str, HostStreak]:
        """
        Parses `state/host_health.streaks`: `<host> <ok|demoted> <bad> <good> <since>` a line.

        Args:
            text: The file's text.

        Returns:
            Host to its streak.
        """
        streaks: dict[str, HostStreak] = {}
        for line in text.splitlines():
            parts: list[str] = line.split()
            if len(parts) < 5 or line.startswith("#"):
                continue
            host, state, bad_text, good_text, since_text = parts[:5]
            if not (bad_text.isdigit() and good_text.isdigit() and since_text.isdigit()):
                continue
            streaks[host] = HostStreak(host, state, int(bad_text), int(good_text), int(since_text))
        return streaks

    @staticmethod
    def parse_lease_status(text: str) -> LeaseStatus:
        """
        Parses the resource status format; `device`, `card` and `host` name the same thing in its `remote` and
        `local` lines, and `whole-device` and `whole-card` the same wait.

        Args:
            text: The command's stdout.

        Returns:
            The remote hosts, slots, device lease, operator priority and waits it names.
        """
        remote_cards: list[RemoteCard] = []
        harness_only_hosts: tuple[str, ...] = ()
        local_card_off: bool = False
        slots: list[LeaseSlot] = []
        device: LeaseSlot | None = None
        operator_priority: tuple[str, ...] = ()
        waits: list[LeaseWait] = []
        unparsed_lines: list[str] = []
        pending_slot_lines: list[str] = []
        for raw_line in text.splitlines():
            line: str = raw_line.strip()
            if not line:
                continue
            card_match: re.Match[str] | None = StateParsers.REMOTE_CARD_PATTERN.match(line)
            only_match: re.Match[str] | None = StateParsers.HARNESS_ONLY_PATTERN.match(line)
            wait_match: re.Match[str] | None = StateParsers.WAIT_PATTERN.match(line)
            if card_match:
                remote_cards.append(RemoteCard(card_match.group(1), int(card_match.group(2)), int(card_match.group(3))))
            elif only_match:
                harness_only_hosts = tuple(only_match.group(1).split())
            elif StateParsers.LOCAL_OFF_PATTERN.match(line):
                local_card_off = True
            elif line.startswith("operator priority:"):
                operator_priority = tuple(line.split(":", 1)[1].split())
            elif wait_match:
                kind: str = "whole-device" if wait_match.group(1) == "whole-card" else wait_match.group(1)
                waits.append(LeaseWait(kind, int(wait_match.group(2)), int(wait_match.group(3)),
                                       wait_match.group(4) or ""))
            elif line.startswith("slot ") or line.startswith("device "):
                pending_slot_lines.append(line)
            else:
                unparsed_lines.append(line)
        for line in pending_slot_lines:
            parsed_slot: LeaseSlot | None = StateParsers._parse_slot_line(line, remote_cards)
            if parsed_slot is None:
                unparsed_lines.append(line)
            elif parsed_slot.slot is None:
                device = parsed_slot
            else:
                slots.append(parsed_slot)
        return LeaseStatus(
            remote_cards=tuple(remote_cards),
            harness_only_hosts=harness_only_hosts,
            local_card_off=local_card_off,
            slots=tuple(slots),
            device=device,
            operator_priority=operator_priority,
            waits=tuple(waits),
            unparsed_lines=tuple(unparsed_lines),
        )

    @staticmethod
    def parse_stalls(text: str, limit: int) -> tuple[StallLine, ...]:
        """
        Parses the last lines of `state/stalls.log`.

        Args:
            text: The file's text (or its tail).
            limit: How many of the newest lines to keep.

        Returns:
            The newest `limit` stall lines, oldest first.
        """
        stalls: list[StallLine] = []
        for line in text.splitlines():
            match: re.Match[str] | None = StateParsers.STALL_PATTERN.match(line.strip())
            if match:
                stalls.append(StallLine(match.group(1), match.group(2), match.group(3), match.group(4)))
        return tuple(stalls[-limit:]) if limit > 0 else ()

    @staticmethod
    def parse_allocator(text: str, limit: int) -> tuple[AllocatorLine, ...]:
        """
        Parses the last decisions in `state/logs/allocator.log`.

        Args:
            text: The file's text (or its tail).
            limit: How many of the newest lines to keep.

        Returns:
            The newest `limit` decisions, oldest first; `kind` is spawn, chain, landing, cooldown, usage or other.
        """
        decisions: list[AllocatorLine] = []
        for line in text.splitlines():
            match: re.Match[str] | None = StateParsers.ALLOCATOR_PATTERN.match(line.strip())
            if match is None:
                continue
            stamp: str = match.group(1)
            rest: str = match.group(2)
            iso_time: str = f"{stamp[0:4]}-{stamp[4:6]}-{stamp[6:8]}T{stamp[9:11]}:{stamp[11:13]}:{stamp[13:15]}Z"
            slot_match: re.Match[str] | None = re.match(r"^(slot\d+) ", rest)
            if "chains tick" in rest:
                kind: str = "chain"
            elif "landing tick" in rest:
                kind = "landing"
            elif "usage limit" in rest:
                kind = "cooldown"
            elif "-> spawning" in rest:
                kind = "spawn"
            elif "usage reading" in rest:
                kind = "usage"
            else:
                kind = "other"
            fields: dict[str, str] = dict(StateParsers.ALLOCATOR_FIELD_PATTERN.findall(rest))
            spawn_match: re.Match[str] | None = re.search(r"-> spawning (\d+)", rest)
            if spawn_match:
                fields["spawning"] = spawn_match.group(1)
            decisions.append(AllocatorLine(iso_time, kind, slot_match.group(1) if slot_match else None, fields, rest))
        return tuple(decisions[-limit:]) if limit > 0 else ()

    @staticmethod
    def parse_usage_cache(text: str) -> UsageReading:
        """
        Parses `~/.claude/usage-cache.json`.

        Args:
            text: The file's JSON text.

        Returns:
            The two windows' percentages, their reset times and when the reading was taken.

        Raises:
            ValueError: The text is not a JSON object.
        """
        document: object = json.loads(text)
        if not isinstance(document, dict):
            raise ValueError("usage cache is not a JSON object")
        data: object = document.get("data")
        data_fields: dict[str, object] = data if isinstance(data, dict) else {}
        timestamp_ms: object = document.get("timestamp")
        return UsageReading(
            five_hour_pct=StateParsers._number_or_none(data_fields.get("fiveHourPercent")),
            weekly_pct=StateParsers._number_or_none(data_fields.get("weeklyPercent")),
            five_hour_resets_at=StateParsers._string_or_none(data_fields.get("fiveHourResetsAt")),
            weekly_resets_at=StateParsers._string_or_none(data_fields.get("weeklyResetsAt")),
            taken_at_epoch=(float(timestamp_ms) / 1000.0) if isinstance(timestamp_ms, (int, float)) else None,
        )

    @staticmethod
    def parse_claim(file_name: str, text: str) -> ClaimRecord | None:
        """
        Parses one `state/locks/claim.<issue>` file.

        Args:
            file_name: The file's base name, `claim.<issue>`.
            text: Its contents, `<issue> <pid> <since>`.

        Returns:
            The claim, or None when the name or contents do not match the format.
        """
        name_match: re.Match[str] | None = re.fullmatch(r"claim\.(\d+)", file_name)
        parts: list[str] = text.split()
        if name_match is None or len(parts) < 3 or not parts[1].isdigit():
            return None
        return ClaimRecord(issue=int(name_match.group(1)), pid=int(parts[1]), since=parts[2])

    @staticmethod
    def parse_machine_probe(text: str) -> MachineProbe:
        """
        Parses the output of the machine probe script (Collector.MACHINE_PROBE_SCRIPT).

        Args:
            text: The script's stdout: a `load` line, two meminfo lines, then `--- device`, `--- processes` and
                `--- cgroup` sections when a device is probed, and a `--- docker` section.

        Returns:
            Load, memory, the device, its processes and the running containers.
        """
        section: str = "head"
        load1: float | None = None
        nproc: int | None = None
        meminfo_kb: dict[str, int] = {}
        gpu: DeviceReading | None = None
        app_rows: list[tuple[int, float | None, str]] = []
        container_by_pid: dict[int, str] = {}
        containers: list[ContainerInfo] = []
        for line in text.splitlines():
            if line.startswith("--- "):
                section = line[4:].strip()
                continue
            if not line.strip():
                continue
            if section == "head":
                parts: list[str] = line.split()
                if parts[0] == "load" and len(parts) >= 3:
                    load1 = StateParsers._optional_float(parts[1])
                    nproc = StateParsers._optional_int(parts[2])
                elif parts[0] in ("MemTotal:", "MemAvailable:") and len(parts) >= 2 and parts[1].isdigit():
                    meminfo_kb[parts[0].rstrip(":")] = int(parts[1])
            elif section == "device" and gpu is None:
                gpu_columns: list[str] = [column.strip() for column in line.split(",")]
                if len(gpu_columns) >= 4:
                    gpu = DeviceReading(
                        name=gpu_columns[0],
                        util_pct=StateParsers._optional_float(gpu_columns[1]),
                        mem_used_mb=StateParsers._optional_float(gpu_columns[2]),
                        mem_total_mb=StateParsers._optional_float(gpu_columns[3]),
                    )
            elif section == "processes":
                app_columns: list[str] = [column.strip() for column in line.split(",", 2)]
                if len(app_columns) == 3 and app_columns[0].isdigit():
                    app_rows.append((int(app_columns[0]), StateParsers._optional_float(app_columns[1]),
                                     app_columns[2]))
            elif section == "cgroup":
                pid_text, _, cgroup_text = line.partition(" ")
                cgroup_match: re.Match[str] | None = StateParsers.DOCKER_CGROUP_PATTERN.search(cgroup_text)
                if pid_text.isdigit() and cgroup_match:
                    container_by_pid[int(pid_text)] = cgroup_match.group(1)
            elif section == "docker":
                docker_columns: list[str] = line.split("\t")
                if len(docker_columns) >= 4:
                    containers.append(ContainerInfo(docker_columns[0][:12], docker_columns[1], docker_columns[2],
                                                    docker_columns[3]))
        name_by_id: dict[str, str] = {container.container_id: container.name for container in containers}
        gpu_processes: tuple[DeviceProcess, ...] = tuple(
            DeviceProcess(pid, used_mb, name,
                       name_by_id.get(container_by_pid.get(pid, ""), container_by_pid.get(pid)))
            for pid, used_mb, name in app_rows
        )
        mem_total_kb: int | None = meminfo_kb.get("MemTotal")
        mem_available_kb: int | None = meminfo_kb.get("MemAvailable")
        return MachineProbe(
            load1=load1,
            nproc=nproc,
            mem_used_mb=((mem_total_kb - mem_available_kb) / 1024.0)
            if mem_total_kb is not None and mem_available_kb is not None else None,
            mem_total_mb=(mem_total_kb / 1024.0) if mem_total_kb is not None else None,
            gpu=gpu,
            gpu_processes=gpu_processes,
            containers=tuple(containers),
        )

    @staticmethod
    def issue_from_path(path: str, worktree_root: str) -> int | None:
        """
        Reads the issue a worktree or run packet belongs to from its path.

        Args:
            path: A path holding a `<worktree root>/<name>-<issue>` worktree or a `<YYYY-MM-DD>-<issue>-<slug>`
                packet.
            worktree_root: WORKTREE_ROOT; empty matches no worktree.

        Returns:
            The issue number, or None when the path names neither.
        """
        root: str = worktree_root.rstrip("/")
        worktree_match: re.Match[str] | None = re.search(
            re.escape(root) + r"/[A-Za-z][A-Za-z0-9]*-(\d+)(?:/|$)", path) if root else None
        match: re.Match[str] | None = worktree_match or StateParsers.PACKET_ISSUE_PATTERN.search(path)
        return int(match.group(1)) if match else None

    @staticmethod
    def model_label(model_id: str) -> str:
        """
        Names a model id for people: `claude-opus-5-5` reads `Opus 5.5`.

        Args:
            model_id: The `--model` value.

        Returns:
            Family and version, or the id itself when it does not follow the pattern.
        """
        match: re.Match[str] | None = StateParsers.MODEL_PATTERN.match(model_id)
        if match is None:
            return model_id or "?"
        version: str = match.group(2) + (f".{match.group(3)}" if match.group(3) else "")
        return f"{match.group(1).capitalize()} {version}"

    @staticmethod
    def tool_command(bash_argument: str) -> str | None:
        """
        Pulls the user command out of a Claude Code tool shell's `bash -c` argument.

        Args:
            bash_argument: The `-c` string, which wraps the command as `eval '<command>' < /dev/null`.

        Returns:
            The command with its quoting undone and whitespace collapsed, or None when the wrapper is not found.
        """
        match: re.Match[str] | None = StateParsers.TOOL_EVAL_PATTERN.search(bash_argument)
        if match is None:
            return None
        command: str = match.group(1).replace("'\"'\"'", "'").replace("'\\''", "'")
        return " ".join(command.split())

    @staticmethod
    def _parse_slot_line(line: str, remote_cards: list[RemoteCard]) -> LeaseSlot | None:
        """
        Parses one `slot N ...` or `device ...` line of the resource status.

        Args:
            line: The line.
            remote_cards: The remote hosts named above it, to place the slot on its host.

        Returns:
            The slot (slot None for the device lease), or None when the line does not match.
        """
        free_match: re.Match[str] | None = StateParsers.SLOT_FREE_PATTERN.match(line)
        if free_match:
            slot_number: int = int(free_match.group(1))
            return LeaseSlot(slot_number, "free", None, None, None, None, None, (),
                             StateParsers._slot_host(slot_number, remote_cards))
        held_match: re.Match[str] | None = StateParsers.SLOT_HELD_PATTERN.match(line)
        if held_match is None:
            return None
        held_slot: int | None = int(held_match.group(1)) if held_match.group(1) else None
        holder: str = held_match.group(3)
        tags: list[str] = held_match.group(6).split()
        domain: int | None = None
        for tag in list(tags):
            if tag.startswith("domain="):
                domain = StateParsers._optional_int(tag.split("=", 1)[1])
                tags.remove(tag)
        return LeaseSlot(
            slot=held_slot,
            state="held" if held_match.group(2) == "HELD" else "orphan",
            holder=holder,
            issue=int(holder) if holder.isdigit() else None,
            pid=int(held_match.group(4)),
            since=held_match.group(5),
            domain=domain,
            tags=tuple(tags),
            host=StateParsers._slot_host(held_slot, remote_cards) if held_slot is not None else "local",
        )

    @staticmethod
    def _slot_host(slot_number: int, remote_cards: list[RemoteCard]) -> str:
        """
        Names the host a slot lives on.

        Args:
            slot_number: The slot.
            remote_cards: The remote hosts and their slot ranges.

        Returns:
            The remote host whose range holds the slot, else `local`.
        """
        for card in remote_cards:
            if card.first_slot <= slot_number <= card.last_slot:
                return card.host
        return "local"

    @staticmethod
    def _optional_float(text: str) -> float | None:
        """
        Reads a number, `-` and other non-numbers reading None.

        Args:
            text: The field.

        Returns:
            The number, or None.
        """
        try:
            return float(text)
        except ValueError:
            return None

    @staticmethod
    def _optional_int(text: str) -> int | None:
        """
        Reads an integer, non-integers reading None.

        Args:
            text: The field.

        Returns:
            The integer, or None.
        """
        stripped: str = text.strip()
        return int(stripped) if stripped.isdigit() else None

    @staticmethod
    def _number_or_none(value: object) -> float | None:
        """
        Keeps a JSON number.

        Args:
            value: A decoded JSON value.

        Returns:
            The number as a float, or None for anything else.
        """
        return float(value) if isinstance(value, (int, float)) and not isinstance(value, bool) else None

    @staticmethod
    def _string_or_none(value: object) -> str | None:
        """
        Keeps a JSON string.

        Args:
            value: A decoded JSON value.

        Returns:
            The string, or None for anything else.
        """
        return value if isinstance(value, str) else None
