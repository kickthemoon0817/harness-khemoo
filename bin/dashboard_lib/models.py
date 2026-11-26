"""
Records the dashboard reads from the harness state, the process table and the hosts.
"""
from dataclasses import dataclass, field
from typing import Any, TypeAlias

JsonDict: TypeAlias = dict[str, Any]


@dataclass(frozen=True)
class HostHealthRow:
    """
    One `state/host_health.tsv` row: a remote host's probe by bin/host_health.sh.
    """

    time: str
    epoch: float
    host: str
    connect_s: float | None
    upload_mb_s: float | None
    upload_timed_out: bool
    load1: float | None
    nproc: int | None
    gpu_util_pct: float | None
    gpu_mem_used_mb: float | None
    status: str


@dataclass(frozen=True)
class HostStreak:
    """
    One `state/host_health.streaks` line: whether the probe has demoted a host, and its streaks.
    """

    host: str
    state: str
    bad_probes: int
    good_probes: int
    since_epoch: int


@dataclass(frozen=True)
class RemoteCard:
    """
    A remote card named by `lease.sh status`, with the slot numbers that live on it.
    """

    host: str
    first_slot: int
    last_slot: int


@dataclass(frozen=True)
class LeaseSlot:
    """
    One slot (or the device lease) as `lease.sh status` reports it.
    """

    slot: int | None
    state: str
    holder: str | None
    issue: int | None
    pid: int | None
    since: str | None
    domain: int | None
    tags: tuple[str, ...]
    host: str


@dataclass(frozen=True)
class LeaseWait:
    """
    A recorded wait for a slot or the whole card.
    """

    kind: str
    issue: int
    minutes: int
    tag: str


@dataclass(frozen=True)
class LeaseStatus:
    """
    The parsed output of `lease.sh status`.
    """

    remote_cards: tuple[RemoteCard, ...]
    harness_only_hosts: tuple[str, ...]
    local_card_off: bool
    slots: tuple[LeaseSlot, ...]
    device: LeaseSlot | None
    operator_priority: tuple[str, ...]
    waits: tuple[LeaseWait, ...]
    unparsed_lines: tuple[str, ...]


@dataclass(frozen=True)
class StallLine:
    """
    One `state/stalls.log` line: `<UTC> STALL <kind> <key> <detail>`.
    """

    time: str
    kind: str
    key: str
    detail: str


@dataclass(frozen=True)
class AllocatorLine:
    """
    One `state/logs/allocator.log` line, with its `key=value` fields lifted out.
    """

    time: str
    kind: str
    slot: str | None
    fields: dict[str, str]
    text: str


@dataclass(frozen=True)
class UsageReading:
    """
    The usage windows from `~/.claude/usage-cache.json`.
    """

    five_hour_pct: float | None
    weekly_pct: float | None
    five_hour_resets_at: str | None
    weekly_resets_at: str | None
    taken_at_epoch: float | None


@dataclass(frozen=True)
class ClaimRecord:
    """
    One `state/locks/claim.<issue>` file: `<issue> <pid> <since>`.
    """

    issue: int
    pid: int
    since: str


@dataclass(frozen=True)
class ProcessInfo:
    """
    One process from /proc.
    """

    pid: int
    ppid: int
    start_epoch: float
    argv: tuple[str, ...]


@dataclass(frozen=True)
class TickProcess:
    """
    A live `claude -p` tick and the issue it works.
    """

    pid: int
    model: str
    model_label: str
    lane: str
    age_s: int
    slot: int | None
    log: str | None
    issue: int | None
    issue_source: str | None
    doing: str | None
    doing_age_s: int | None
    tool_shells: int


@dataclass(frozen=True)
class GpuReading:
    """
    One card's utilisation and memory.
    """

    name: str
    util_pct: float | None
    mem_used_mb: float | None
    mem_total_mb: float | None


@dataclass(frozen=True)
class GpuProcess:
    """
    A compute process on a card, with the container it runs in when known.
    """

    pid: int
    used_mb: float | None
    name: str
    container: str | None


@dataclass(frozen=True)
class ContainerInfo:
    """
    One `docker ps` row.
    """

    container_id: str
    name: str
    status: str
    image: str


@dataclass(frozen=True)
class MachineProbe:
    """
    One read of a machine: load, memory, card, card processes and containers.
    """

    load1: float | None
    nproc: int | None
    mem_used_mb: float | None
    mem_total_mb: float | None
    gpu: GpuReading | None
    gpu_processes: tuple[GpuProcess, ...]
    containers: tuple[ContainerInfo, ...]


@dataclass(frozen=True)
class SourceResult:
    """
    The last outcome of one data source: its data, when it was fetched and any error.
    """

    data: Any
    fetched_at: float | None
    error: str | None = None
    duration_s: float | None = None
    attempted_at: float | None = None
    extra: dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class RunQueue:
    """
    One bin/arm_queue.sh run folder (`<packet>/runs*/`): who holds it, what it waits for and what it runs.
    """

    packet: str
    runs: str
    issue: int | None
    holder_pid: int | None
    alive: bool
    state: str
    want: str | None
    arm: str | None
    arm_host: str | None
    arm_started: str | None
    busy_since: str | None
    busy_reason: str | None
    granted_host: str | None
    finished_at: str | None
    updated_epoch: float


@dataclass(frozen=True)
class LockParty:
    """
    A process holding or waiting on a build lock, and the issue its worktree belongs to.
    """

    pid: int
    issue: int | None
    tree: str | None
    alive: bool


@dataclass(frozen=True)
class BuildLane:
    """
    One host's compile lane: its build lock, the holder and the waiters in line.
    """

    name: str
    host: str
    path: str
    holder: LockParty | None
    waiters: tuple[LockParty, ...]
