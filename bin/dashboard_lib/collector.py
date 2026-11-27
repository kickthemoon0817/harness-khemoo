"""
Gathers the dashboard's data on a cadence per source and assembles one JSON-ready snapshot.
"""
import json
import logging
import os
import shlex
import socket
import subprocess
import threading
import time
from collections.abc import Callable
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import ClassVar

from dashboard_lib.buildlocks import BuildLockReader
from dashboard_lib.flow import FlowBuilder, FlowInputs
from dashboard_lib.flowtrack import FlowTracker
from dashboard_lib.github import GitHubClient, GitHubError
from dashboard_lib.models import (
    AllocatorLine,
    BuildLane,
    ClaimRecord,
    HostHealthRow,
    HostStreak,
    JsonDict,
    LeaseSlot,
    LeaseStatus,
    MachineProbe,
    ProcessInfo,
    RunQueue,
    SourceResult,
    TickProcess,
    UsageReading,
)
from dashboard_lib.parsers import StateParsers
from dashboard_lib.procs import ProcessScanner
from dashboard_lib.queue import IssueQueue, QueueIssue, QueueLabels
from dashboard_lib.redact import CommandRedactor
from dashboard_lib.runqueue import RunQueueReader


@dataclass(frozen=True)
class UsageThresholds:
    """
    Where each usage window cuts the pool (bin/usage-lib.sh defaults).
    """

    ceiling_pct: float = 90
    five_half_at: float = 60
    five_one_at: float = 70
    five_none_at: float = 80
    week_half_at: float = 80
    week_one_at: float = 85
    week_none_at: float = 88


@dataclass(frozen=True)
class DashboardConfig:
    """
    Where the dashboard reads from and writes to, how often each source refreshes, and the instance's optional
    sources; each optional source left empty keeps its panel off the page.

    `resource_status_command` is an argv printing the resource status format (`remote card|host: <h> (slots a-b)`,
    `slot N free|HELD|ORPHAN ...`, `device ...`, `operator priority: ...`, `wait ...`). `device_probe_command` is a
    shell command run on each machine whose first line is `<name>, <util %>, <memory used MiB>, <memory total MiB>`
    and whose further lines are `<pid>, <memory MiB>, <process name>`. `run_queue_glob` names run folders, each with a
    `status` and a `lease.log`, live while a process whose first arguments hold `run_queue_holder` holds them.
    `build_lock_local` and `build_lock_remote` (with `{name}` for the host's BUILD_LOCK_NAMES name) are lock file
    names under `build_lock_dir`.
    """

    harness_home: Path
    data_dir: Path
    repo: str | None
    harness_state: Path | None = None
    title: str = "Harness status"
    lane_model: str = ""
    labels: QueueLabels = field(default_factory=QueueLabels)
    usage_cache: Path = Path.home() / ".claude" / "usage-cache.json"
    usage_thresholds: UsageThresholds = field(default_factory=UsageThresholds)
    gh_binary: str = "gh"
    crontab_command: tuple[str, ...] = ("crontab", "-l")
    history_hours: float = 6.0
    github_interval_s: float = 60.0
    local_interval_s: float = 30.0
    remote_interval_s: float = 120.0
    claimable_interval_s: float = 300.0
    fast_ttl_s: float = 3.0
    local_history_keep_rows: int = 1440
    stall_lines: int = 10
    allocator_lines: int = 12
    usage_max_age_s: float = 1800.0
    model_cooldown_s: float = 1800.0
    claim_grace_s: float = 360.0
    command_timeout_s: float = 8.0
    claimable_timeout_s: float = 120.0
    ssh_connect_timeout_s: int = 5
    remote_timeout_s: float = 15.0
    host_health_stale_s: float = 540.0
    dependency_ttl_s: float = 600.0
    pid_lookup_retry_s: float = 600.0
    claim_file_max_age_s: float = 172800.0
    resource_status_command: tuple[str, ...] = ()
    device_probe_command: str = ""
    run_queue_glob: str = ""
    run_queue_holder: str = ""
    run_queue_stop_words: tuple[str, ...] = ()
    run_queue_window_s: float = 21600.0
    build_lock_names: tuple[tuple[str, str], ...] = ()
    build_lock_dir: Path = Path("/tmp")
    build_lock_local: str = ""
    build_lock_remote: str = ""
    done_window_s: float = 21600.0
    flow_animate_s: float = 20.0
    activity_ttl_s: float = 10.0

    @property
    def state_dir(self) -> Path:
        """
        Returns:
            The harness's state directory: HARNESS_STATE, else `<harness home>/state`.
        """
        return self.harness_state or self.harness_home / "state"

    @property
    def build_locks_enabled(self) -> bool:
        """
        Returns:
            True when the instance names a local or a remote build lock.
        """
        return bool(self.build_lock_local or self.build_lock_remote)


class Collector:
    """
    Keeps the last result of every source and builds snapshots from them.

    The publisher collects the resource status, the process table and local state files at the page refresh cadence.
    Local samples, SSH, GitHub and claimable.sh have independent refresh threads. Each snapshot carries
    their last results and ages. The only files written are the dashboard's own, under `data_dir`.
    """

    LOAD_PROBE: ClassVar[str] = r"""printf 'load %s %s\n' "$(cut -d' ' -f1 /proc/loadavg)" "$(nproc)"
"""
    MEMORY_PROBE: ClassVar[str] = r"""
sed -n '/^MemTotal:/p;/^MemAvailable:/p' /proc/meminfo
"""
    DEVICE_PROBE: ClassVar[str] = r"""
device=$(timeout 5 sh -c {command} 2>/dev/null)
echo '--- device'
printf '%s\n' "$device" | head -1
echo '--- processes'
apps=$(printf '%s\n' "$device" | tail -n +2)
[ -n "$apps" ] && printf '%s\n' "$apps"
echo '--- cgroup'
for p in $(printf '%s\n' "$apps" | cut -d, -f1); do printf '%s %s\n' "$p" "$(tr '\n' ' ' < /proc/$p/cgroup 2>/dev/null)"; done
"""
    CONTAINER_PROBE: ClassVar[str] = r"""
echo '--- docker'
timeout 5 docker ps --format '{{.ID}}\t{{.Names}}\t{{.Status}}\t{{.Image}}' 2>/dev/null
exit 0
"""
    LOCAL_HISTORY_COLUMNS: ClassVar[tuple[str, ...]] = (
        "time", "load1", "nproc", "mem_used_mb", "mem_total_mb", "gpu_util_pct", "gpu_mem_used_mb",
        "gpu_mem_total_mb",
    )
    TAIL_BYTES: ClassVar[int] = 65536
    PID_LOOKUPS_PER_CYCLE: ClassVar[int] = 3

    def __init__(
        self,
        config: DashboardConfig,
        github: GitHubClient | None,
        clock: Callable[[], float] = time.time,
    ) -> None:
        """
        Args:
            config: Paths, labels and cadences.
            github: The GitHub client, or None when no repo is configured.
            clock: Returns the current epoch; tests pin it.
        """
        self.config: DashboardConfig = config
        self.github: GitHubClient | None = github
        self.clock: Callable[[], float] = clock
        self.scanner: ProcessScanner = ProcessScanner(config.state_dir)
        self.queue: IssueQueue = IssueQueue(config.labels)
        self._results: dict[str, SourceResult] = {}
        self._results_lock: threading.Lock = threading.Lock()
        self._fast_lock: threading.Lock = threading.Lock()
        self._history_lock: threading.Lock = threading.Lock()
        self._stop_event: threading.Event = threading.Event()
        self._threads: list[threading.Thread] = []
        self._host_health_cache: tuple[tuple[int, int], tuple[HostHealthRow, ...]] | None = None
        self._local_history: list[tuple[float | None, ...]] = []
        self._dependency_cache: dict[int, tuple[str, float]] = {}
        self._github_issue_by_pid: dict[int, int] = {}
        self._github_outages: list[tuple[float, float | None]] = []
        self._last_github_success: float | None = None
        self._pid_lookup_attempts: dict[int, float] = {}
        self._processes: dict[int, ProcessInfo] = {}
        self.local_history_path: Path = config.data_dir / "local_history.tsv"
        self.github_cache_path: Path = config.data_dir / "github_cache.json"
        self.run_queue_reader: RunQueueReader = RunQueueReader(
            config.run_queue_glob, config.run_queue_holder, config.run_queue_stop_words, config.run_queue_window_s)
        self.build_lock_reader: BuildLockReader = BuildLockReader(
            dict(config.build_lock_names), config.build_lock_dir, config.build_lock_local, config.build_lock_remote,
            config.command_timeout_s)
        self.flow_builder: FlowBuilder = FlowBuilder(
            config.labels, dict(config.build_lock_names), config.done_window_s,
            device_lanes=bool(config.resource_status_command or config.run_queue_glob),
            build_lanes=config.build_locks_enabled)
        self.machine_probe_script: str = self.MEMORY_PROBE + (
            self.DEVICE_PROBE.replace("{command}", shlex.quote(config.device_probe_command))
            if config.device_probe_command else "") + self.CONTAINER_PROBE
        self.flow_tracker: FlowTracker = FlowTracker(config.data_dir / "flow_state.json", config.flow_animate_s)
        self._activity_lock: threading.Lock = threading.Lock()

    def start(self, publish: Callable[[], None] | None = None, publish_interval_s: float = 30.0) -> None:
        """
        Creates the data directory, owner-only, loads the saved history and GitHub cache, and starts the source
        threads.

        Args:
            publish: Collects and publishes the complete HTTP representations.
            publish_interval_s: Seconds between publications.
        """
        self.config.data_dir.mkdir(parents=True, exist_ok=True)
        os.chmod(self.config.data_dir, 0o700)
        self._load_local_history_in_place()
        self._load_github_cache_in_place()
        self.flow_tracker.load()
        periodic_sources: list[tuple[str, float, Callable[[], None]]] = [
            ("local", self.config.local_interval_s, self.refresh_local_sample_in_place),
            ("remote", self.config.remote_interval_s, self.refresh_remote_probes_in_place),
            ("github", self.config.github_interval_s, self.refresh_github_in_place),
            ("claimable_sh", self.config.claimable_interval_s, self.refresh_claimable_in_place),
        ]
        for name, interval_s, refresh in periodic_sources:
            if interval_s <= 0:
                continue
            thread: threading.Thread = threading.Thread(target=self._run_periodically,
                                                        args=(name, interval_s, refresh),
                                                        name=f"dashboard-{name}", daemon=True)
            thread.start()
            self._threads.append(thread)

        if publish is not None:
            publish()

            def publish_periodically() -> None:
                while not self._stop_event.wait(max(1.0, publish_interval_s)):
                    try:
                        publish()
                    except Exception:  # noqa: BLE001 - one bad publication must not end the publisher
                        logging.exception("Dashboard publication failed")

            publisher = threading.Thread(target=publish_periodically, name="dashboard-publisher", daemon=True)
            publisher.start()
            self._threads.append(publisher)

    def stop(self) -> None:
        """
        Asks the source threads to end and waits briefly for them.
        """
        self._stop_event.set()
        for thread in self._threads:
            thread.join(timeout=2.0)

    def snapshot(self) -> JsonDict:
        """
        Refreshes the fast sources when due and assembles the current snapshot.

        Returns:
            The JSON-ready state every panel renders from.
        """
        with self._fast_lock:
            procs_result: SourceResult | None = self._result("procs")
            if procs_result is None or procs_result.attempted_at is None \
                    or self.clock() - procs_result.attempted_at >= self.config.fast_ttl_s:
                self.refresh_fast_sources_in_place()
        return self._assemble_snapshot(self.clock())

    def refresh_fast_sources_in_place(self) -> None:
        """
        Re-reads the resource status, when the instance has a command for it, and the process table, keeping the
        last good resource read on failure.
        """
        if self.config.resource_status_command:
            self._refresh_resource_status_in_place()
        processes_started: float = self.clock()
        try:
            self._processes = self.scanner.scan_processes()
            self._store("procs", SourceResult(self.scanner.find_ticks(self._processes), processes_started,
                                              duration_s=self.clock() - processes_started,
                                              attempted_at=processes_started))
        except OSError as error:
            self._store_failure("procs", self._describe_error(error), processes_started)

    def _refresh_resource_status_in_place(self) -> None:
        """
        Runs the instance's resource status command, as an argv with no shell.
        """
        started: float = self.clock()
        try:
            completed: subprocess.CompletedProcess[str] = subprocess.run(
                list(self.config.resource_status_command), capture_output=True, text=True,
                timeout=self.config.command_timeout_s, stdin=subprocess.DEVNULL,
                env=dict(os.environ, HARNESS_HOME=str(self.config.harness_home),
                         HARNESS_STATE=str(self.config.state_dir)),
            )
            if completed.returncode != 0:
                raise RuntimeError(f"resource status exit {completed.returncode}: "
                                   f"{completed.stderr.strip()[-200:]}")
            self._store("lease", SourceResult(StateParsers.parse_lease_status(completed.stdout), started,
                                              duration_s=self.clock() - started, attempted_at=started,
                                              extra={"raw": completed.stdout.splitlines()}))
        except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
            self._store_failure("lease", self._describe_error(error), started)

    def refresh_local_sample_in_place(self) -> None:
        """
        Samples this host (load, memory, device and its processes when probed, containers), appends it to the
        history file, and re-reads whether the harness is paused.
        """
        started: float = self.clock()
        try:
            completed: subprocess.CompletedProcess[str] = subprocess.run(
                ["bash", "-c", self.LOAD_PROBE + self.machine_probe_script], capture_output=True, text=True,
                timeout=self.config.command_timeout_s * 2, stdin=subprocess.DEVNULL,
            )
            probe: MachineProbe = StateParsers.parse_machine_probe(completed.stdout)
            self._store("local", SourceResult(probe, started, duration_s=self.clock() - started,
                                              attempted_at=started))
            self._append_local_history_in_place(started, probe)
        except (OSError, subprocess.TimeoutExpired) as error:
            self._store_failure("local", self._describe_error(error), started)
        self._store("paused", SourceResult(self._read_paused_state(), started, attempted_at=started))

    def refresh_remote_probes_in_place(self) -> None:
        """
        Probes each remote host once over ssh, read-only, all hosts at once.
        """
        started: float = self.clock()
        hosts: list[str] = self._remote_hosts()
        previous_result: SourceResult | None = self._result("remote")
        previous_probes: dict[str, SourceResult] = previous_result.data if previous_result and previous_result.data \
            else {}
        latest = {row.host: row for row in self._read_host_health()}

        def failed(host: str, error: str, attempted_at: float) -> SourceResult:
            previous: SourceResult | None = previous_probes.get(host)
            return SourceResult(previous.data if previous else None, previous.fetched_at if previous else None,
                                error=error, attempted_at=attempted_at)

        def probe_host(host: str) -> SourceResult:
            host_started: float = self.clock()
            row = latest.get(host)
            script = self.machine_probe_script
            if (row is None or row.load1 is None or row.nproc is None
                    or host_started - row.epoch > self.config.host_health_stale_s):
                script = self.LOAD_PROBE + script
            try:
                completed: subprocess.CompletedProcess[str] = subprocess.run(
                    ["ssh", "-o", "BatchMode=yes", "-o", f"ConnectTimeout={self.config.ssh_connect_timeout_s}",
                     "-o", "ControlMaster=no", host, script],
                    capture_output=True, text=True, timeout=self.config.remote_timeout_s, stdin=subprocess.DEVNULL,
                )
                if completed.returncode != 0:
                    raise RuntimeError(f"ssh exit {completed.returncode}: {completed.stderr.strip()[-160:]}")
                return SourceResult(StateParsers.parse_machine_probe(completed.stdout), host_started,
                                    duration_s=self.clock() - host_started, attempted_at=host_started)
            except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
                return failed(host, self._describe_error(error), host_started)

        # Each worker appends to its own list and only finished ones are read, so a worker that outlives the
        # join never touches what is stored.
        outcomes: list[tuple[str, threading.Thread, list[SourceResult]]] = []
        for host in hosts:
            outcome: list[SourceResult] = []
            worker: threading.Thread = threading.Thread(
                target=lambda probed=host, into=outcome: into.append(probe_host(probed)), daemon=True)
            worker.start()
            outcomes.append((host, worker, outcome))
        for _, worker, _ in outcomes:
            worker.join(timeout=self.config.remote_timeout_s + 5)
        probes: dict[str, SourceResult] = {
            host: outcome[0] if outcome else failed(host, "probe gave no result in time", started)
            for host, _, outcome in outcomes
        }
        self._store("remote", SourceResult(probes, started, duration_s=self.clock() - started, attempted_at=started))

    def refresh_github_in_place(self) -> None:
        """
        Reads the open issues, their dependencies' states, the last day's merges and any unmapped tick's issue.
        """
        started: float = self.clock()
        if self.github is None:
            self._store_failure("github", "GH_REPO is not set", started)
            return
        try:
            labels: QueueLabels = self.config.labels
            issues: list[JsonDict] = self.github.fetch_open_issues(labels.all_issues)
            open_numbers: frozenset[int] = frozenset(int(issue["number"]) for issue in issues)
            first_only_issues: list[JsonDict] = [
                issue for issue in self.github.fetch_open_issues(labels.first)
                if int(issue["number"]) not in open_numbers
            ] if labels.first != labels.all_issues else []
            wanted_dependencies: list[int] = sorted(
                {dependency for issue in issues + first_only_issues
                 for dependency in IssueQueue.parse_dependencies(issue.get("body") or "")}
                - open_numbers
            )
            stale_dependencies: list[int] = [
                dependency for dependency in wanted_dependencies
                if started - self._dependency_cache.get(dependency, ("", 0.0))[1] >= self.config.dependency_ttl_s
            ]
            if stale_dependencies:
                for number, state in self.github.fetch_states(stale_dependencies).items():
                    self._dependency_cache[number] = (state, started)
            since_iso: str = self._iso(started - 86400)
            landings: list[JsonDict] = self.github.fetch_merged_pulls(since_iso)
            closed_issues: list[JsonDict] = self.github.fetch_closed_issues(
                labels.all_issues, self._iso(started - self.config.done_window_s))
            self._look_up_unmapped_ticks_in_place(started)
            dependency_states: dict[int, str] = {number: self._dependency_cache[number][0]
                                                 for number in wanted_dependencies if number in self._dependency_cache}
            payload: JsonDict = {"issues": issues, "first_only_issues": first_only_issues,
                                 "dependency_states": dependency_states, "landings": landings,
                                 "closed_issues": closed_issues}
            self._store("github", SourceResult(payload, started, duration_s=self.clock() - started,
                                               attempted_at=started))
            self._record_github_outcome_in_place(started, succeeded=True)
            self._save_github_cache(payload, started)
        except (GitHubError, KeyError, TypeError, ValueError) as error:
            self._store_failure("github", self._describe_error(error), started)
            self._record_github_outcome_in_place(started, succeeded=False)

    def refresh_claimable_in_place(self) -> None:
        """
        Runs bin/claimable.sh for the queue label, as the launcher does, and keeps its count.
        """
        started: float = self.clock()
        if self.github is None:
            self._store_failure("claimable_sh", "GH_REPO is not set", started)
            return
        labels: QueueLabels = self.config.labels
        environment: dict[str, str] = self.github.child_environment({
            "GH_REPO": self.github.repo, "HARNESS_HOME": str(self.config.harness_home),
            "ISSUE_LABEL": labels.queue, "WIP_LABEL": labels.wip, "SIGNOFF_LABEL": labels.signoff,
            "OPERATOR_LABEL": labels.operator, "READY_LABEL": labels.ready, "FIRST_LABEL": labels.first,
            "LOW_LABEL": labels.low, "LANE_LABEL": labels.lane,
        })
        try:
            completed: subprocess.CompletedProcess[str] = subprocess.run(
                [str(self.config.harness_home / "bin" / "claimable.sh")], capture_output=True, text=True,
                timeout=self.config.claimable_timeout_s, env=environment, stdin=subprocess.DEVNULL,
            )
            output: str = completed.stdout.strip()
            if completed.returncode != 0 or not output.isdigit():
                raise RuntimeError(self.github.redact(f"claimable.sh exit {completed.returncode}: "
                                                      f"{(completed.stderr or output).strip()[-200:]}"))
            self._store("claimable_sh", SourceResult(int(output), started, duration_s=self.clock() - started,
                                                     attempted_at=started))
        except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
            self._store_failure("claimable_sh", self._describe_error(error), started)

    def _assemble_snapshot(self, now: float) -> JsonDict:
        """
        Builds the snapshot from the stored results and the state files.

        Args:
            now: The current epoch.

        Returns:
            The JSON-ready snapshot.
        """
        state_dir: Path = self.config.state_dir
        lease_result: SourceResult | None = self._result("lease")
        lease_status: LeaseStatus | None = lease_result.data if lease_result else None
        lease_slots: tuple[LeaseSlot, ...] = ()
        if lease_status is not None:
            lease_slots = lease_status.slots + ((lease_status.device,) if lease_status.device else ())
        procs_result: SourceResult | None = self._result("procs")
        claims: tuple[ClaimRecord, ...] = self._read_claims(now)
        ticks: tuple[TickProcess, ...] = self.scanner.describe_ticks(
            procs_result.data if procs_result and procs_result.data else (), self._processes, claims, lease_slots,
            dict(self._github_issue_by_pid), now,
        ) if procs_result else ()
        tick_pids: frozenset[int] = frozenset(tick.pid for tick in ticks)
        tick_pid_by_issue: dict[int, int] = {tick.issue: tick.pid for tick in ticks if tick.issue is not None}
        live_claim_pid_by_issue: dict[int, int] = {
            claim.issue: claim.pid for claim in claims
            if claim.pid in self._processes and claim.issue not in tick_pid_by_issue
        }

        stalls_path: Path = state_dir / "stalls.log"
        allocator_text: str = self._read_tail(state_dir / "logs" / "allocator.log")
        allocator_lines: tuple[AllocatorLine, ...] = StateParsers.parse_allocator(allocator_text, 400)
        usage: JsonDict = self._usage_view(now)

        github_result: SourceResult | None = self._result("github")
        github_payload: JsonDict = github_result.data if github_result and github_result.data else {}
        groups: dict[str, list[QueueIssue]] = self.queue.classify(
            github_payload.get("issues", []), github_payload.get("dependency_states", {}),
            live_claim_pid_by_issue, tick_pid_by_issue, github_payload.get("first_only_issues", []),
        )
        issue_titles: dict[int, tuple[str, str]] = {
            int(issue["number"]): (issue.get("title", ""), issue.get("url", ""))
            for issue in github_payload.get("issues", [])
        }
        claimable_count: int = len(groups["claimable"])
        waiting_count: int = claimable_count
        claimable_sh_result: SourceResult | None = self._result("claimable_sh")
        claimable_sh_view: JsonDict = self._source_value_view(claimable_sh_result, now)
        claimable_sh_view["unverified"] = claimable_sh_result is not None \
            and claimable_sh_result.fetched_at is not None \
            and self._during_github_outage(claimable_sh_result.fetched_at, now)
        last_allocator: AllocatorLine | None = next(
            (line for line in reversed(allocator_lines) if "allowed" in line.fields), None)
        last_claimable_line: AllocatorLine | None = next(
            (line for line in reversed(allocator_lines) if "claimable" in line.fields), None)
        allocator_views: list[JsonDict] = [
            dict(asdict(line), text=CommandRedactor.redact(line.text), claimable_unverified="claimable" in line.fields
                 and self._during_github_outage(StateParsers.parse_iso_epoch(line.time), now))
            for line in allocator_lines[-self.config.allocator_lines:]
        ]
        github_failing: bool = github_result is None or github_result.error is not None

        lease_view: JsonDict = self._lease_view(lease_status, lease_result, tick_pids, now)
        machines: list[JsonDict] = self._machine_views(lease_status, waiting_count, now)
        run_queues, build_lanes = self._activity(ticks, now)
        flow: JsonDict = self.flow_builder.build(FlowInputs(
            groups=groups, ticks=ticks, queues=run_queues, lanes=build_lanes,
            lease_slots=lease_status.slots if lease_status else (),
            closed_issues=github_payload.get("closed_issues", []), machines=machines, titles=issue_titles, now=now,
        ))
        self.flow_tracker.update_in_place(flow, now)
        paused_result: SourceResult | None = self._result("paused")
        paused_state: JsonDict = paused_result.data if paused_result else {"paused": False, "reason": ""}
        live_standard_ticks: int = sum(1 for tick in ticks if tick.lane != "landing")
        last_allowed: int | None = int(last_allocator.fields["allowed"]) \
            if last_allocator and last_allocator.fields.get("allowed", "").isdigit() else None
        work_waiting: bool = (waiting_count > 0 and last_allowed is not None and live_standard_ticks < last_allowed
                              and not paused_state.get("paused") and usage.get("allowed_band") != "none")

        return {
            "generated_at": self._iso(now),
            "generated_epoch": now,
            "title": self.config.title,
            "harness_home": str(self.config.harness_home),
            "repo": self.config.repo,
            "history_hours": self.config.history_hours,
            "harness": {
                "paused": paused_state.get("paused", False),
                "paused_reason": paused_state.get("reason", ""),
                "tick_models": self._read_tick_models(),
                "cooling_models": self._cooling_models(now),
                "live_ticks": len(ticks),
                "last_allowed": last_allowed,
                "last_allowed_at": last_allocator.time if last_allocator else None,
                "work_waiting": work_waiting,
                "waiting_count": waiting_count,
                "waiting_count_from_failing_read": github_failing,
                "queue_label": self.config.labels.queue,
                "lane_model": self.config.lane_model,
                "labels": asdict(self.config.labels),
            },
            "usage": usage,
            "machines": machines,
            "leases": lease_view if self.config.resource_status_command else None,
            "ticks": [
                dict(asdict(tick), title=issue_titles.get(tick.issue, ("", ""))[0] if tick.issue else "",
                     url=issue_titles.get(tick.issue, ("", ""))[1] if tick.issue else "",
                     lease_slots=[lease.slot for lease in lease_slots
                                  if tick.issue is not None and lease.issue == tick.issue and lease.slot is not None],
                     choosing=tick.issue is None and tick.age_s < self.config.claim_grace_s)
                for tick in ticks
            ],
            "queue": {
                "groups": {name: [asdict(issue) for issue in issues] for name, issues in groups.items()},
                "counts": {name: len(issues) for name, issues in groups.items()},
                "claimable_count": claimable_count,
                "claimable_sh": claimable_sh_view,
                "allocator_claimable": {
                    "value": last_claimable_line.fields["claimable"],
                    "at": last_claimable_line.time,
                    "unverified": self._during_github_outage(StateParsers.parse_iso_epoch(last_claimable_line.time),
                                                             now),
                } if last_claimable_line else None,
                "github_outages": [{"from": self._iso(start), "to": self._iso(end)}
                                   for start, end in self._github_outages[-5:]],
                "fetched_at": self._iso(github_result.fetched_at) if github_result and github_result.fetched_at
                else None,
                "age_s": self._age(github_result.fetched_at if github_result else None, now),
                "error": github_result.error if github_result else "not fetched yet",
            },
            "landings": github_payload.get("landings", []),
            "health": {
                "stalls": [dict(asdict(stall), key=CommandRedactor.redact(stall.key),
                                detail=CommandRedactor.redact(stall.detail))
                           for stall in StateParsers.parse_stalls(self._read_tail(stalls_path),
                                                                  self.config.stall_lines)]
                if stalls_path.exists() else None,
                "allocator": allocator_views,
            },
            "flow": flow,
            "sources": self._sources_view(now),
        }

    def _activity(self, ticks: tuple[TickProcess, ...], now: float) -> tuple[tuple[RunQueue, ...],
                                                                               tuple[BuildLane, ...]]:
        """
        Reads the run queues and compile lanes the instance configures, at most every `activity_ttl_s`, keeping the
        last good read.

        Args:
            ticks: The live ticks, to name the issue of a lock party with no worktree in its path.
            now: The current epoch.

        Returns:
            The run queues and the compile lanes.
        """
        if not self.config.run_queue_glob and not self.config.build_locks_enabled:
            return (), ()
        with self._activity_lock:
            previous: SourceResult | None = self._result("activity")
            if previous is not None and previous.attempted_at is not None \
                    and now - previous.attempted_at < self.config.activity_ttl_s and previous.data is not None:
                return previous.data
            started: float = self.clock()
            run_queues: tuple[RunQueue, ...] = self.run_queue_reader.read_queues(self._processes, now) \
                if self.config.run_queue_glob else ()
            try:
                build_lanes: tuple[BuildLane, ...] = self.build_lock_reader.read_lanes(
                    self._remote_hosts(), self._processes,
                    {tick.pid: tick.issue for tick in ticks if tick.issue is not None}) \
                    if self.config.build_locks_enabled else ()
            except (OSError, subprocess.TimeoutExpired) as error:
                self._store_failure("activity", self._describe_error(error), started)
                kept: SourceResult | None = self._result("activity")
                return (run_queues, kept.data[1] if kept and kept.data else ())
            self._store("activity", SourceResult((run_queues, build_lanes), started,
                                                 duration_s=self.clock() - started, attempted_at=now))
            return run_queues, build_lanes

    def _machine_views(self, lease_status: LeaseStatus | None, waiting_count: int, now: float) -> list[JsonDict]:
        """
        Builds one view a machine: this host first, then each remote host.

        Args:
            lease_status: The last lease read.
            waiting_count: Claimable issues, for the idle-with-work-waiting signal.
            now: The current epoch.

        Returns:
            Machine views.
        """
        window_start: float = now - self.config.history_hours * 3600
        all_rows: tuple[HostHealthRow, ...] = self._read_host_health()
        streaks: dict[str, HostStreak] = StateParsers.parse_streaks(self._read_text(
            self.config.state_dir / "host_health.streaks"))
        lease_waits: int = len(lease_status.waits) if lease_status else 0
        slots: tuple[LeaseSlot, ...] = lease_status.slots if lease_status else ()
        machines: list[JsonDict] = []

        local_result: SourceResult | None = self._result("local")
        local_probe: MachineProbe | None = local_result.data if local_result else None
        with self._history_lock:
            local_rows: list[tuple[float | None, ...]] = [row for row in self._local_history
                                                          if row[0] is not None and row[0] >= window_start]
        local_view: JsonDict = self._probe_fields(local_probe)
        local_view.update({
            "key": "local",
            "name": socket.gethostname(),
            "kind": "local",
            "status": "ok" if local_result and local_result.error is None and local_probe else "unknown",
            "upload_mb_s": None,
            "connect_s": None,
            "streak": None,
            "card_note": "harness off (left to its owner)" if lease_status and (
                lease_status.local_card_off or lease_status.harness_only_hosts) else "",
            "slots": [asdict(slot) for slot in slots if slot.host == "local" and slot.state != "free"],
            "slot_count": sum(1 for slot in slots if slot.host == "local"),
            "history": {
                "gpu_util": [[row[0], row[5]] for row in local_rows if row[5] is not None],
                "load_per_core": [[row[0], row[1] / row[2]] for row in local_rows
                                  if row[1] is not None and row[2]],
            },
            "sampled_at": self._iso(local_result.fetched_at) if local_result and local_result.fetched_at else None,
            "age_s": self._age(local_result.fetched_at if local_result else None, now),
            "probe_error": local_result.error if local_result else None,
        })
        local_view["signals"] = self._machine_signals(local_view, waiting_count, lease_waits,
                                                      harness_uses_card=not local_view["card_note"])
        machines.append(local_view)

        remote_result: SourceResult | None = self._result("remote")
        remote_probes: dict[str, SourceResult] = remote_result.data if remote_result and remote_result.data else {}
        for host in self._remote_hosts():
            host_rows: list[HostHealthRow] = [row for row in all_rows if row.host == host]
            latest: HostHealthRow | None = host_rows[-1] if host_rows else None
            probe_result: SourceResult | None = remote_probes.get(host)
            probe: MachineProbe | None = probe_result.data if probe_result else None
            view: JsonDict = self._probe_fields(probe)
            if latest is not None and (probe_result is None or probe_result.fetched_at is None
                                       or latest.epoch >= probe_result.fetched_at):
                view.update({"load1": latest.load1, "nproc": latest.nproc})
                if view.get("gpu"):
                    view["gpu"] = dict(view["gpu"], util_pct=latest.gpu_util_pct, mem_used_mb=latest.gpu_mem_used_mb)
                else:
                    view["gpu"] = {"name": "", "util_pct": latest.gpu_util_pct, "mem_used_mb": latest.gpu_mem_used_mb,
                                   "mem_total_mb": None}
            if latest is not None and probe is not None and probe.nproc is None:
                view.update(load1=latest.load1, nproc=latest.nproc)
            streak: HostStreak | None = streaks.get(host)
            window_rows: list[HostHealthRow] = [row for row in host_rows if row.epoch >= window_start]
            view.update({
                "key": host,
                "name": host,
                "kind": "remote",
                "status": latest.status if latest else "unknown",
                "upload_mb_s": latest.upload_mb_s if latest else None,
                "upload_timed_out": latest.upload_timed_out if latest else False,
                "connect_s": latest.connect_s if latest else None,
                "probed_at": latest.time if latest else None,
                "probe_age_s": self._age(latest.epoch if latest else None, now),
                "probe_stale": latest is None or now - latest.epoch > self.config.host_health_stale_s,
                "streak": asdict(streak) if streak else None,
                "preferred_rank": (lease_status.harness_only_hosts.index(host) + 1)
                if lease_status and host in lease_status.harness_only_hosts else None,
                "card_note": "",
                "slots": [asdict(slot) for slot in slots if slot.host == host and slot.state != "free"],
                "slot_count": sum(1 for slot in slots if slot.host == host),
                "history": {
                    "gpu_util": [[row.epoch, row.gpu_util_pct] for row in window_rows if row.gpu_util_pct is not None],
                    "load_per_core": [[row.epoch, row.load1 / row.nproc] for row in window_rows
                                      if row.load1 is not None and row.nproc],
                    "upload": [[row.epoch, row.upload_mb_s] for row in window_rows if row.upload_mb_s is not None],
                    "down": [row.epoch for row in window_rows if row.status == "down"],
                },
                "sampled_at": self._iso(probe_result.fetched_at) if probe_result and probe_result.fetched_at
                else None,
                "age_s": self._age(probe_result.fetched_at if probe_result else None, now),
                "probe_error": probe_result.error if probe_result else None,
            })
            view["signals"] = self._machine_signals(view, waiting_count, lease_waits, harness_uses_card=True)
            machines.append(view)
        return machines

    def _machine_signals(self, view: JsonDict, waiting_count: int, lease_waits: int,
                         harness_uses_card: bool) -> list[JsonDict]:
        """
        States a machine's notable conditions for the page.

        Args:
            view: The machine view so far.
            waiting_count: Claimable issues.
            lease_waits: Recorded slot or device waits.
            harness_uses_card: False when the device is left to its owner.

        Returns:
            Signals, each a level (critical, serious, warning, info) and text.
        """
        signals: list[JsonDict] = []
        streak: JsonDict | None = view.get("streak")
        if streak and streak.get("state") == "demoted":
            signals.append({"level": "serious", "text": f"moved last ({streak.get('bad_probes')} bad probes)"})
        if view.get("probe_stale") and view.get("kind") == "remote":
            signals.append({"level": "warning", "text": "host probe stale"})
        if view.get("probe_error"):
            signals.append({"level": "info", "text": "live read failed"})
        gpu: JsonDict | None = view.get("gpu")
        gpu_util: float | None = gpu.get("util_pct") if gpu else None
        recent_util: list[float] = [point[1] for point in view["history"]["gpu_util"][-3:]]
        recent_load: list[float] = [point[1] for point in view["history"]["load_per_core"][-3:]]
        card_idle: bool = gpu_util == 0 and all(value == 0 for value in recent_util)
        cpu_idle: bool = bool(recent_load) and all(value < 0.25 for value in recent_load)
        if harness_uses_card and card_idle and not view["slots"] and (waiting_count > 0 or lease_waits > 0):
            signals.append({"level": "warning", "text": "idle, work waiting"})
        elif harness_uses_card and card_idle and cpu_idle and view["slots"] and len(recent_util) >= 3:
            signals.append({"level": "warning", "text": "held, device and CPU idle"})
        return signals

    def _lease_view(self, lease_status: LeaseStatus | None, lease_result: SourceResult | None,
                    tick_pids: frozenset[int], now: float) -> JsonDict:
        """
        Builds the slots-and-leases panel's data.

        Args:
            lease_status: The last lease read.
            lease_result: Its source result, for age and error.
            tick_pids: Live tick pids, to name each holder.
            now: The current epoch.

        Returns:
            Remote hosts, slots with holder kinds and ages, whole-device holds, device lease, priority and waits.
        """
        if lease_status is None:
            return {"available": False, "error": lease_result.error if lease_result else "not read yet",
                    "slots": [], "whole_card_holds": [], "waits": [], "remote_cards": [], "operator_priority": [],
                    "harness_only_hosts": [], "local_card_off": False, "device": None, "unparsed": []}

        def slot_view(slot: LeaseSlot) -> JsonDict:
            since_epoch: float | None = None
            if slot.since:
                try:
                    since_epoch = StateParsers.parse_iso_epoch(slot.since)
                except ValueError:
                    since_epoch = None
            return dict(asdict(slot), holder=CommandRedactor.redact(slot.holder) if slot.holder else slot.holder,
                        holder_kind=self.scanner.describe_holder(slot.pid, self._processes, tick_pids),
                        held_s=self._age(since_epoch, now))

        whole_card: dict[tuple[str, str | None, int | None], JsonDict] = {}
        for slot in lease_status.slots:
            if slot.state != "free" and any(tag in ("remote-all", "held-for-all", "exclusive", "reserved-for-all")
                                            for tag in slot.tags):
                hold_key: tuple[str, str | None, int | None] = (slot.host, slot.holder, slot.pid)
                hold: JsonDict = whole_card.setdefault(hold_key, dict(slot_view(slot), slots=[]))
                hold["slots"].append(slot.slot)
                hold["reserving"] = "reserved-for-all" in slot.tags
        return {
            "available": True,
            "error": lease_result.error if lease_result else None,
            "age_s": self._age(lease_result.fetched_at if lease_result else None, now),
            "remote_cards": [asdict(card) for card in lease_status.remote_cards],
            "harness_only_hosts": list(lease_status.harness_only_hosts),
            "local_card_off": lease_status.local_card_off,
            "slots": [slot_view(slot) for slot in lease_status.slots],
            "whole_card_holds": list(whole_card.values()),
            "device": slot_view(lease_status.device) if lease_status.device else None,
            "operator_priority": list(lease_status.operator_priority),
            "waits": [asdict(wait) for wait in lease_status.waits],
            "unparsed": [CommandRedactor.redact(line) for line in lease_status.unparsed_lines],
        }

    def _usage_view(self, now: float) -> JsonDict:
        """
        Reads the usage cache and places each window against the allocator's cut points.

        Args:
            now: The current epoch.

        Returns:
            Percentages, reset times, age, staleness and each window's band (full, half, one, none).
        """
        thresholds: UsageThresholds = self.config.usage_thresholds
        try:
            reading: UsageReading = StateParsers.parse_usage_cache(self.config.usage_cache.read_text())
        except (OSError, ValueError) as error:
            return {"available": False, "error": self._describe_error(error), "thresholds": asdict(thresholds),
                    "stale": True}
        age_s: int | None = self._age(reading.taken_at_epoch, now)
        stale: bool = age_s is None or age_s > self.config.usage_max_age_s

        def band(percent: float | None, half_at: float, one_at: float, none_at: float) -> str | None:
            if percent is None:
                return None
            if percent >= thresholds.ceiling_pct or percent >= none_at:
                return "none"
            if percent >= one_at:
                return "one"
            return "half" if percent >= half_at else "full"

        five_band: str | None = band(reading.five_hour_pct, thresholds.five_half_at, thresholds.five_one_at,
                                     thresholds.five_none_at)
        week_band: str | None = band(reading.weekly_pct, thresholds.week_half_at, thresholds.week_one_at,
                                     thresholds.week_none_at)
        order: tuple[str, ...] = ("full", "half", "one", "none")
        known_bands: list[str] = [value for value in (five_band, week_band) if value is not None]
        allowed_band: str = "none" if stale or not known_bands else max(known_bands, key=order.index)
        return dict(asdict(reading), available=True, error=None, age_s=age_s, stale=stale,
                    five_hour_band=five_band, weekly_band=week_band, allowed_band=allowed_band,
                    thresholds=asdict(thresholds))

    def _sources_view(self, now: float) -> JsonDict:
        """
        Lists every source's age, last error and cadence.

        Args:
            now: The current epoch.

        Returns:
            Source name to its status.
        """
        intervals: dict[str, float] = {
            "procs": self.config.fast_ttl_s, "local": self.config.local_interval_s,
            "paused": self.config.local_interval_s, "remote": self.config.remote_interval_s,
            "github": self.config.github_interval_s, "claimable_sh": self.config.claimable_interval_s,
        }
        if self.config.resource_status_command:
            intervals["lease"] = self.config.fast_ttl_s
        if self.config.run_queue_glob or self.config.build_locks_enabled:
            intervals["activity"] = self.config.activity_ttl_s
        view: JsonDict = {}
        for name, interval_s in intervals.items():
            result: SourceResult | None = self._result(name)
            view[name] = {
                "interval_s": interval_s,
                "fetched_at": self._iso(result.fetched_at) if result and result.fetched_at else None,
                "age_s": self._age(result.fetched_at if result else None, now),
                "error": result.error if result else None,
                "duration_s": round(result.duration_s, 2) if result and result.duration_s is not None else None,
            }
        host_health_path: Path = self.config.state_dir / "host_health.tsv"
        try:
            host_health_mtime: float = host_health_path.stat().st_mtime
        except OSError:
            return view
        view["host_health_tsv"] = {"interval_s": 180, "fetched_at": self._iso(host_health_mtime),
                                   "age_s": self._age(host_health_mtime, now), "error": None, "duration_s": None}
        return view

    def _source_value_view(self, result: SourceResult | None, now: float) -> JsonDict:
        """
        Shapes one source's value with its age and error.

        Args:
            result: The source result.
            now: The current epoch.

        Returns:
            Value, time, age and error.
        """
        return {
            "value": result.data if result else None,
            "fetched_at": self._iso(result.fetched_at) if result and result.fetched_at else None,
            "age_s": self._age(result.fetched_at if result else None, now),
            "error": result.error if result else None,
        }

    def _look_up_unmapped_ticks_in_place(self, now: float) -> None:
        """
        Searches GitHub claim comments for live ticks old enough to have claimed but with no claim or lease.

        Args:
            now: The current epoch.
        """
        if self.github is None:
            return
        procs_result: SourceResult | None = self._result("procs")
        lease_result: SourceResult | None = self._result("lease")
        if procs_result is None or not procs_result.data:
            return
        lease_slots: tuple[LeaseSlot, ...] = lease_result.data.slots if lease_result and lease_result.data else ()
        if lease_result and lease_result.data and lease_result.data.device:
            lease_slots += (lease_result.data.device,)
        mapped: dict[int, tuple[int, str]] = ProcessScanner.map_pids_to_issues(self._read_claims(now), lease_slots, {})
        lookups: int = 0
        live_pids: set[int] = set()
        for candidate in procs_result.data:
            pid: int = candidate.process.pid
            live_pids.add(pid)
            if pid in mapped or pid in self._github_issue_by_pid:
                continue
            if now - candidate.process.start_epoch < self.config.claim_grace_s:
                continue
            if now - self._pid_lookup_attempts.get(pid, 0.0) < self.config.pid_lookup_retry_s:
                continue
            if lookups >= self.PID_LOOKUPS_PER_CYCLE:
                break
            lookups += 1
            self._pid_lookup_attempts[pid] = now
            issue: int | None = self.github.find_issue_for_pid(pid)
            if issue is not None:
                self._github_issue_by_pid[pid] = issue
        for pid in list(self._github_issue_by_pid):
            if pid not in live_pids:
                del self._github_issue_by_pid[pid]

    def _remote_hosts(self) -> list[str]:
        """
        Lists the remote hosts: the resource status's remote hosts, else the hosts host_health.tsv names.

        Returns:
            Hosts in the harness's order.
        """
        lease_result: SourceResult | None = self._result("lease")
        if lease_result and lease_result.data and lease_result.data.remote_cards:
            return list(dict.fromkeys(card.host for card in lease_result.data.remote_cards))
        return list(dict.fromkeys(row.host for row in self._read_host_health()))

    def _read_host_health(self) -> tuple[HostHealthRow, ...]:
        """
        Parses host_health.tsv, re-reading it only when it changed.

        Returns:
            Its rows.
        """
        path: Path = self.config.state_dir / "host_health.tsv"
        try:
            stat_result: os.stat_result = path.stat()
        except OSError:
            return ()
        signature: tuple[int, int] = (stat_result.st_mtime_ns, stat_result.st_size)
        if self._host_health_cache is None or self._host_health_cache[0] != signature:
            self._host_health_cache = (signature, StateParsers.parse_host_health_tsv(self._read_text(path)))
        return self._host_health_cache[1]

    def _read_claims(self, now: float) -> tuple[ClaimRecord, ...]:
        """
        Reads the claim files written recently enough to belong to a live tick.

        Args:
            now: The current epoch.

        Returns:
            The claims.
        """
        claims: list[ClaimRecord] = []
        try:
            entries: list[os.DirEntry[str]] = list(os.scandir(self.config.state_dir / "locks"))
        except OSError:
            return ()
        for entry in entries:
            if not entry.name.startswith("claim.") or entry.name == "claim.guard":
                continue
            try:
                if now - entry.stat().st_mtime > self.config.claim_file_max_age_s:
                    continue
                claim: ClaimRecord | None = StateParsers.parse_claim(entry.name, Path(entry.path).read_text())
            except OSError:
                continue
            if claim is not None:
                claims.append(claim)
        return tuple(claims)

    def _read_paused_state(self) -> JsonDict:
        """
        Reads whether the harness is paused, by tick.sh's rule: `state/paused` exists, or no uncommented
        crontab line runs bin/tick.sh.

        Returns:
            Paused flag and reason.
        """
        if (self.config.state_dir / "paused").exists():
            return {"paused": True, "reason": "state/paused present"}
        try:
            completed: subprocess.CompletedProcess[str] = subprocess.run(
                list(self.config.crontab_command), capture_output=True, text=True,
                timeout=self.config.command_timeout_s,
                stdin=subprocess.DEVNULL,
            )
        except (OSError, subprocess.TimeoutExpired):
            return {"paused": False, "reason": "crontab unreadable"}
        for line in completed.stdout.splitlines():
            stripped: str = line.strip()
            if stripped and not stripped.startswith("#") and "bin/tick.sh" in stripped:
                return {"paused": False, "reason": ""}
        return {"paused": True, "reason": "no active bin/tick.sh cron line"}

    def _read_tick_models(self) -> list[str]:
        """
        Returns:
            `state/tick-model`'s models, best first.
        """
        return self._read_text(self.config.state_dir / "tick-model").split()

    def _cooling_models(self, now: float) -> list[JsonDict]:
        """
        Lists models still cooling down after a usage limit.

        Args:
            now: The current epoch.

        Returns:
            Model and seconds left, for each stamp younger than the cooldown.
        """
        cooling: list[JsonDict] = []
        try:
            entries: list[os.DirEntry[str]] = list(os.scandir(self.config.state_dir / "model-cooldown"))
        except OSError:
            return cooling
        for entry in entries:
            try:
                remaining_s: float = self.config.model_cooldown_s - (now - entry.stat().st_mtime)
            except OSError:
                continue
            if remaining_s > 0:
                cooling.append({"model": entry.name, "remaining_s": int(remaining_s),
                                "label": StateParsers.model_label(entry.name)})
        return cooling

    def _append_local_history_in_place(self, epoch: float, probe: MachineProbe) -> None:
        """
        Adds one local sample to the in-memory history and the history file, trimming both.

        Args:
            epoch: The sample's time.
            probe: The sample.
        """
        gpu_util: float | None = probe.gpu.util_pct if probe.gpu else None
        row: tuple[float | None, ...] = (
            epoch, probe.load1, float(probe.nproc) if probe.nproc else None, probe.mem_used_mb, probe.mem_total_mb,
            gpu_util, probe.gpu.mem_used_mb if probe.gpu else None, probe.gpu.mem_total_mb if probe.gpu else None,
        )
        keep_rows: int = self.config.local_history_keep_rows
        with self._history_lock:
            self._local_history.append(row)
            if len(self._local_history) > keep_rows:
                del self._local_history[:-keep_rows]
            try:
                self.config.data_dir.mkdir(parents=True, exist_ok=True)
                if not self.local_history_path.exists():
                    self.local_history_path.write_text("\t".join(self.LOCAL_HISTORY_COLUMNS) + "\n")
                with self.local_history_path.open("a") as history_file:
                    history_file.write(self._history_line(row))
                if self.local_history_path.stat().st_size > keep_rows * 90 * 1.25:
                    temporary_path: Path = self.local_history_path.with_suffix(".tsv.new")
                    temporary_path.write_text("\t".join(self.LOCAL_HISTORY_COLUMNS) + "\n" + "".join(
                        self._history_line(kept_row) for kept_row in self._local_history))
                    temporary_path.replace(self.local_history_path)
            except OSError as error:
                self._store_failure("local_history", self._describe_error(error), epoch)

    def _load_local_history_in_place(self) -> None:
        """
        Loads the saved local history so the sparklines survive a restart.
        """
        rows: list[tuple[float | None, ...]] = []
        for line in self._read_text(self.local_history_path).splitlines()[1:]:
            columns: list[str] = line.split("\t")
            if len(columns) != len(self.LOCAL_HISTORY_COLUMNS):
                continue
            try:
                epoch: float = StateParsers.parse_iso_epoch(columns[0])
            except ValueError:
                continue
            values: list[float | None] = [None if column in ("", "-") else float(column) for column in columns[1:]]
            rows.append((epoch, *values))
        with self._history_lock:
            self._local_history = rows[-self.config.local_history_keep_rows:]

    def _save_github_cache(self, payload: JsonDict, fetched_at: float) -> None:
        """
        Saves the last GitHub read so a restart serves it at once, marked with its age.

        Args:
            payload: Issues, dependency states and landings.
            fetched_at: When it was read.
        """
        try:
            self.config.data_dir.mkdir(parents=True, exist_ok=True)
            temporary_path: Path = self.github_cache_path.with_suffix(".json.new")
            temporary_path.write_text(json.dumps({"fetched_at": fetched_at, "payload": payload}))
            temporary_path.replace(self.github_cache_path)
        except OSError as error:
            self._store_failure("github_cache", self._describe_error(error), fetched_at)

    def _load_github_cache_in_place(self) -> None:
        """
        Serves the saved GitHub read until the first fetch replaces it.
        """
        try:
            document: JsonDict = json.loads(self.github_cache_path.read_text())
            payload: JsonDict = document["payload"]
            fetched_at: float = float(document["fetched_at"])
            payload["dependency_states"] = {int(number): state
                                            for number, state in payload.get("dependency_states", {}).items()}
        except (OSError, ValueError, KeyError, TypeError, AttributeError):
            return
        self._store("github", SourceResult(payload, fetched_at, error=None, attempted_at=fetched_at))
        self._last_github_success = fetched_at

    def _record_github_outcome_in_place(self, attempted_at: float, succeeded: bool) -> None:
        """
        Tracks when the dashboard's own GitHub read was failing.

        An outage runs from the last read that worked (or one cadence before the first failure, when none has)
        to the next read that works; while it lasts, a count another reader took from GitHub may be claimable.sh's
        fallback rather than a real count.

        Args:
            attempted_at: When the read started.
            succeeded: Whether it worked.
        """
        with self._results_lock:
            open_outage: bool = bool(self._github_outages) and self._github_outages[-1][1] is None
            if succeeded:
                if open_outage:
                    self._github_outages[-1] = (self._github_outages[-1][0], attempted_at)
                self._last_github_success = attempted_at
            elif not open_outage:
                start: float = self._last_github_success if self._last_github_success is not None \
                    else attempted_at - self.config.github_interval_s
                self._github_outages.append((start, None))
                del self._github_outages[:-20]

    def _during_github_outage(self, epoch: float, now: float) -> bool:
        """
        Args:
            epoch: A time.
            now: The current time, the end of an outage still open.

        Returns:
            True when the dashboard's own GitHub read was failing at that time.
        """
        with self._results_lock:
            outages: list[tuple[float, float | None]] = list(self._github_outages)
        return any(start < epoch < (end if end is not None else now + 1) for start, end in outages)

    def _run_periodically(self, name: str, interval_s: float, refresh: Callable[[], None]) -> None:
        """
        Calls a refresh at once and then every interval until stopped; an unexpected error is logged and stored
        as the source's failure, and the loop goes on.

        Args:
            name: The source the refresh stores.
            interval_s: Seconds between calls.
            refresh: The refresh to run.
        """
        while not self._stop_event.is_set():
            started: float = self.clock()
            try:
                refresh()
            except Exception as error:  # noqa: BLE001 - a dead thread would freeze the source without a word
                logging.exception("Dashboard refresh of %s failed", name)
                self._store_failure(name, self._describe_error(error), started)
            self._stop_event.wait(interval_s)

    def _store(self, name: str, result: SourceResult) -> None:
        """
        Records a source's result.

        Args:
            name: The source.
            result: Its result.
        """
        with self._results_lock:
            self._results[name] = result

    def _store_failure(self, name: str, error: str, attempted_at: float) -> None:
        """
        Records a failed refresh while keeping the source's last good data and its time.

        Args:
            name: The source.
            error: What went wrong.
            attempted_at: When the refresh started.
        """
        with self._results_lock:
            previous: SourceResult | None = self._results.get(name)
            self._results[name] = SourceResult(previous.data if previous else None,
                                               previous.fetched_at if previous else None, error=error,
                                               attempted_at=attempted_at,
                                               extra=previous.extra if previous else {})

    def _result(self, name: str) -> SourceResult | None:
        """
        Args:
            name: The source.

        Returns:
            Its last result, or None before the first refresh.
        """
        with self._results_lock:
            return self._results.get(name)

    def _describe_error(self, error: BaseException) -> str:
        """
        Words an error for the page, with the token removed.

        Args:
            error: The error.

        Returns:
            Its type and message, redacted.
        """
        message: str = f"{type(error).__name__}: {error}"
        return self.github.redact(message) if self.github else message

    def _read_tail(self, path: Path) -> str:
        """
        Reads the last TAIL_BYTES of a file, from the first whole line.

        Args:
            path: The file.

        Returns:
            Its tail, or an empty string when it is missing.
        """
        try:
            with path.open("rb") as tail_file:
                size: int = tail_file.seek(0, os.SEEK_END)
                tail_file.seek(max(0, size - self.TAIL_BYTES))
                data: bytes = tail_file.read()
        except OSError:
            return ""
        text: str = data.decode("utf-8", "replace")
        return text.split("\n", 1)[1] if len(data) >= self.TAIL_BYTES and "\n" in text else text

    @staticmethod
    def _probe_fields(probe: MachineProbe | None) -> JsonDict:
        """
        Copies a machine probe's fields into a view.

        Args:
            probe: The probe, or None.

        Returns:
            Load, cores, memory, device, device processes and containers (empty when no probe).
        """
        if probe is None:
            return {"load1": None, "nproc": None, "mem_used_mb": None, "mem_total_mb": None, "gpu": None,
                    "gpu_processes": [], "containers": []}
        return {
            "load1": probe.load1,
            "nproc": probe.nproc,
            "mem_used_mb": probe.mem_used_mb,
            "mem_total_mb": probe.mem_total_mb,
            "gpu": asdict(probe.gpu) if probe.gpu else None,
            "gpu_processes": [asdict(process) for process in probe.gpu_processes],
            "containers": [asdict(container) for container in probe.containers],
        }

    @staticmethod
    def _history_line(row: tuple[float | None, ...]) -> str:
        """
        Formats one local history row.

        Args:
            row: Epoch then the sampled values.

        Returns:
            The tab-separated line.
        """
        epoch: float | None = row[0]
        rendered: list[str] = [Collector._iso(epoch) if epoch is not None else "-"]
        rendered += ["-" if value is None else f"{value:.2f}".rstrip("0").rstrip(".") for value in row[1:]]
        return "\t".join(rendered) + "\n"

    @staticmethod
    def _read_text(path: Path) -> str:
        """
        Args:
            path: A file.

        Returns:
            Its text, or an empty string when it is missing or unreadable.
        """
        try:
            return path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return ""

    @staticmethod
    def _iso(epoch: float | None) -> str | None:
        """
        Args:
            epoch: Epoch seconds.

        Returns:
            ISO-8601 UTC to the second, or None.
        """
        if epoch is None:
            return None
        return datetime.fromtimestamp(epoch, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    @staticmethod
    def _age(epoch: float | None, now: float) -> int | None:
        """
        Args:
            epoch: A past time.
            now: The current time.

        Returns:
            Whole seconds since, or None.
        """
        return None if epoch is None else max(0, int(now - epoch))
