#!/usr/bin/env python3
"""
Exercises bin/dashboard.py: the state-file parsers, the tick-to-issue mapping, the queue grouping, and the
collector, page and HTTP server against a scratch harness home with a fake resource status command, claimable.sh
and gh, and the dashboard with none of the instance's optional sources configured.
"""
import contextlib
import gzip
import hashlib
import html
import http.client
import io
import json
import os
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from collections.abc import Callable
from typing import Any, ClassVar
from unittest.mock import patch

REPO_ROOT: Path = Path(__file__).resolve().parent.parent
FIXTURES: Path = REPO_ROOT / "tests" / "fixtures" / "dashboard"
sys.path.insert(0, str(REPO_ROOT / "bin"))

import dashboard  # noqa: E402
from dashboard_lib.collector import Collector, DashboardConfig  # noqa: E402
from dashboard_lib.github import GitHubClient  # noqa: E402
from dashboard_lib.harness_env import HarnessEnvReader  # noqa: E402
from dashboard_lib.buildlocks import BuildLockReader  # noqa: E402
from dashboard_lib.flow import FlowBuilder, FlowInputs  # noqa: E402
from dashboard_lib.flowtrack import FlowTracker  # noqa: E402
from dashboard_lib.models import (  # noqa: E402
    BuildLane, ClaimRecord, JsonDict, LeaseSlot, LockParty, ProcessInfo, RunQueue, TickProcess,
)
from dashboard_lib.parsers import StateParsers  # noqa: E402
from dashboard_lib.procs import ProcessScanner  # noqa: E402
from dashboard_lib.queue import IssueQueue, QueueLabels  # noqa: E402
from dashboard_lib.redact import CommandRedactor  # noqa: E402
from dashboard_lib.runqueue import RunQueueReader  # noqa: E402
from dashboard_lib.render import HtmlRenderer  # noqa: E402

linux_only = unittest.skipUnless(sys.platform.startswith("linux"), "needs /proc, flock, setsid and lslocks")


class FixtureHarness:
    """
    A scratch harness home: state files from the fixtures, a fake resource status command, claimable.sh and gh,
    an instance harness.env holding a fake token and every optional source, and a fake tick process.
    """

    FAKE_TOKEN: ClassVar[str] = "ghp_FAKEtokenFAKEtokenFAKEtoken0123456789"
    PINNED_EPOCH: ClassVar[float] = 1795657200.0  # 2026-11-26T01:40:00Z

    def __init__(self) -> None:
        self.temporary_directory: tempfile.TemporaryDirectory[str] = tempfile.TemporaryDirectory()
        self.home: Path = Path(self.temporary_directory.name).resolve() / "harness"
        self.state: Path = self.home / "state"
        self.calls_log: Path = self.home / "calls.log"
        self.tick_processes: list[subprocess.Popen[bytes]] = []
        self._slot_lock_files: list[Any] = []
        self.helper_processes: list[subprocess.Popen[bytes]] = []
        for directory in (self.home / "bin", self.home / "instance", self.state / "locks", self.state / "logs",
                          self.state / "model-cooldown", self.home / "evidence", self.home / "buildlocks"):
            directory.mkdir(parents=True)
        shutil.copy(FIXTURES / "host_health.tsv", self.state / "host_health.tsv")
        shutil.copy(FIXTURES / "host_health.streaks", self.state / "host_health.streaks")
        shutil.copy(FIXTURES / "stalls.log", self.state / "stalls.log")
        shutil.copy(FIXTURES / "allocator.log", self.state / "logs" / "allocator.log")
        shutil.copy(FIXTURES / "usage-cache.json", self.home / "usage-cache.json")
        (self.state / "tick-model").write_text("claude-opus-5-5 claude-sonnet-5-2\n")
        (self.state / "model-cooldown" / "claude-sonnet-5-2").touch()
        (self.state / "locks" / "claim.2501").write_text(f"2501 {os.getpid()} 2026-11-26T01:10:00Z\n")
        (self.home / "instance" / "harness.env").write_text(textwrap.dedent(f"""\
            # fixture
            TARGET_REPO=/nowhere
            WORKTREE_ROOT={self.home / 'trees'}
            export GH_TOKEN="{self.FAKE_TOKEN}"
            GH_REPO=example/repo   # the target
            ISSUE_LABEL=ai:plan
            FIRST_LABEL=ai:first
            LOW_LABEL="ai:low"
            LANE_LABEL=ai:lane
            LANE_MODEL=claude-sonnet-5-2
            DASHBOARD_TITLE="Fixture <b>harness</b>"
            RESOURCE_STATUS_CMD="'{self.home / 'bin' / 'resource-status'}' status"
            DEVICE_PROBE_CMD="cat '{FIXTURES / 'device_probe.txt'}'"
            RUN_QUEUE_GLOB={self.home / 'evidence'}/*/runs*
            RUN_QUEUE_HOLDER=run_queue.sh
            RUN_QUEUE_STOP_WORDS="STOP YIELD NO-LEASE"
            BUILD_LOCK_DIR={self.home / 'buildlocks'}
            BUILD_LOCK_LOCAL=dev-build.lock
            BUILD_LOCK_REMOTE=dev-build-remote-{{name}}.lock
            BUILD_LOCK_NAMES="ci@worker-a.local=wa ci@192.0.2.6=laptop"
            OTHER_SECRET=do-not-read
        """))
        self._write_script("resource-status", f"""\
            #!/usr/bin/env bash
            echo "resource-status $*" >> "{self.calls_log}"
            if [ -s "{self.home / 'lease_override.txt'}" ]; then cat "{self.home / 'lease_override.txt'}"
            else cat "{FIXTURES / 'lease_status.txt'}"; fi
        """)
        self._write_script("claimable.sh", f"""\
            #!/usr/bin/env bash
            echo "claimable.sh label=$ISSUE_LABEL token=${{GH_TOKEN:+set}} repo=$GH_REPO" >> "{self.calls_log}"
            [ -n "${{FAKE_CLAIMABLE_FAIL:-}}" ] && {{ echo "boom $GH_TOKEN" >&2; exit 3; }}
            echo 2
        """)
        self._write_script("crontab", f"""\
            #!/usr/bin/env bash
            [ -e "{self.home / 'cron-off'}" ] && {{ echo "# */2 * * * * {self.home}/bin/tick.sh"; exit 0; }}
            echo "*/2 * * * * {self.home}/bin/tick.sh"
        """)
        self._write_script("gh", f"""\
            #!/usr/bin/env python3
            import json, os, re, sys
            fixtures = "{FIXTURES}"
            arguments = sys.argv[1:]
            with open("{self.calls_log}", "a") as log:
                log.write("gh " + json.dumps(arguments) + "\\n")
            if os.environ.get("GH_TOKEN") != "{self.FAKE_TOKEN}":
                sys.stderr.write("no token\\n"); sys.exit(4)
            if os.path.exists("{self.home / 'gh-fail'}"):
                sys.stderr.write("HTTP 401: Bad credentials for token " + os.environ["GH_TOKEN"] + "\\n"); sys.exit(1)
            query = next((a[len("query="):] for a in arguments if a.startswith("query=")), "")
            if "search/issues" in arguments:
                print(json.dumps({{"items": [{{"number": 2490}}]}}))
            elif "issues(" in query:
                name = "/issues_first.json" if "label=ai:first" in arguments else "/issues.json"
                nodes = json.load(open(fixtures + name))
                print(json.dumps({{"data": {{"repository": {{"issues": {{"totalCount": len(nodes),
                    "pageInfo": {{"hasNextPage": False, "endCursor": None}}, "nodes": nodes}}}}}}}}))
            elif "search(" in query and any("is:issue is:closed" in a for a in arguments):
                nodes = json.load(open(fixtures + "/closed.json"))
                print(json.dumps({{"data": {{"search": {{"issueCount": len(nodes), "nodes": nodes}}}}}}))
            elif "search(" in query:
                nodes = json.load(open(fixtures + "/merged.json"))
                print(json.dumps({{"data": {{"search": {{"issueCount": len(nodes), "nodes": nodes}}}}}}))
            elif "issueOrPullRequest" in query:
                states = json.load(open(fixtures + "/states.json"))
                numbers = re.findall(r"n(\\d+): issueOrPullRequest", query)
                print(json.dumps({{"data": {{"repository": {{"n" + n: ({{"state": states[n]}} if n in states else None)
                                                           for n in numbers}}}}}}))
            else:
                sys.stderr.write("unexpected call\\n"); sys.exit(2)
        """)

    def start_fake_tick(self, issue: int, slot: int, tool_command: str = "sleep 30") -> int:
        """
        Starts a process that looks like a harness tick: argv[0] `claude`, argv[1] `-p`, stdout to a tick log,
        a slot lock open, and one tool shell child wrapping its command as Claude Code does. The tool shell sleeps
        before its `eval`, so the command is only carried in its argv, never run: it may be any text, quoting
        that does not parse included.

        Args:
            issue: The issue its claim file names.
            slot: The slot lock it holds; one tick a slot.
            tool_command: The command its tool shell carries.

        Returns:
            Its pid.
        """
        escaped_command: str = tool_command.replace("'", "'\"'\"'")
        tool_argument: Path = self.home / f"tool_argument_{slot}.txt"
        tool_argument.write_text(f"true && sleep 30 && eval '{escaped_command}' < /dev/null && pwd -P >| /dev/null")
        script: Path = self.home / f"fake_tick_{slot}.sh"
        script.write_text(f'bash -c "$(cat \'{tool_argument}\')" &\nwait\n')
        slot_lock_file = (self.state / "locks" / f"slot{slot}.lock").open("w")
        self._slot_lock_files.append(slot_lock_file)
        log_file = (self.state / "logs" / f"tick-20261126T013000Z-slot{slot}.log").open("w")
        tick_process = subprocess.Popen(
            ["bash", "-c", f'exec -a claude bash -p "{script}" "This tick runs on claude-opus-5-5: never claim" '
                           "--model claude-opus-5-5"],
            stdout=log_file, stderr=subprocess.DEVNULL, pass_fds=(slot_lock_file.fileno(),),
            start_new_session=True,
        )
        log_file.close()
        self.tick_processes.append(tick_process)
        pid: int = tick_process.pid
        (self.state / "locks" / f"claim.{issue}").write_text(f"{issue} {pid} 2026-11-26T01:30:00Z\n")
        (self.state / "locks" / "claim.2001").write_text(f"2001 {pid} 2026-11-25T01:00:00Z\n")
        deadline: float = time.time() + 5
        while time.time() < deadline:
            children: str = subprocess.run(["ps", "-o", "args=", "--ppid", str(pid)], capture_output=True,
                                           text=True).stdout
            if "eval '" in children:
                break
            time.sleep(0.05)
        return pid

    def start_fake_queue(self, issue: int, runs: str, status: str, lease_log: str) -> int:
        """
        Writes a run folder and starts a process that looks like its run_queue.sh holder.

        Args:
            issue: The issue its packet is named for.
            runs: The run folder's name.
            status: The `status` text.
            lease_log: The queue log's text; `{pid}` stands for the holder's pid.

        Returns:
            The holder's pid.
        """
        script: Path = self.home / "bin" / "run_queue.sh"
        script.write_text("sleep 30\n")
        holder = subprocess.Popen(["bash", str(script)], start_new_session=True)
        self.helper_processes.append(holder)
        folder: Path = self.home / "evidence" / f"2026-11-26-{issue}-fixture" / runs
        folder.mkdir(parents=True)
        (folder / "status").write_text(status)
        (folder / "queue.log").write_text(lease_log.replace("{pid}", str(holder.pid)))
        return holder.pid

    def hold_build_lock(self, name: str, holder_issue: int, waiter_issue: int) -> tuple[int, int]:
        """
        Takes a build lock as a build does (pid and tree written into it) and starts a waiter in another tree.

        Args:
            name: The lock name, `dev-build-remote-<name>.lock`.
            holder_issue: The issue of the holder's worktree.
            waiter_issue: The issue of the waiter's worktree, its working directory.

        Returns:
            The holder's and the waiter's pids.
        """
        lock: Path = self.home / "buildlocks" / f"dev-build-remote-{name}.lock"
        holder = subprocess.Popen(["bash", "-c", 'exec 8>"$1"; flock 8; sleep 30', "_", str(lock)],
                                  start_new_session=True)
        self.helper_processes.append(holder)
        deadline: float = time.time() + 5
        while time.time() < deadline and not lock.exists():
            time.sleep(0.05)
        time.sleep(0.2)
        lock.write_text(f"pid {holder.pid}, {self.home}/trees/app-{holder_issue}, app\n")
        tree: Path = self.home / "trees" / f"app-{waiter_issue}"
        tree.mkdir(parents=True, exist_ok=True)
        waiter = subprocess.Popen(["flock", str(lock), "sleep", "30"], cwd=tree, start_new_session=True)
        self.helper_processes.append(waiter)
        while time.time() < deadline:
            listing: str = subprocess.run(["lslocks", "-n", "-o", "PID,MODE,PATH"], capture_output=True,
                                          text=True).stdout
            if any(line.split()[:1] == [str(waiter.pid)] and "*" in line for line in listing.splitlines()):
                break
            time.sleep(0.05)
        return holder.pid, waiter.pid

    def config(self, **overrides: Any) -> DashboardConfig:
        """
        Args:
            **overrides: DashboardConfig fields to change.

        Returns:
            A config reading this home, with its data dir inside it.
        """
        fields: dict[str, Any] = dict(
            harness_home=self.home, data_dir=self.home / "dashboard-data", repo="example/repo",
            labels=QueueLabels(queue="ai:plan", lane="ai:lane"), usage_cache=self.home / "usage-cache.json",
            gh_binary=str(self.home / "bin" / "gh"), remote_interval_s=0,
            crontab_command=(str(self.home / "bin" / "crontab"),),
            resource_status_command=(str(self.home / "bin" / "resource-status"), "status"),
            device_probe_command=("cat", str(FIXTURES / "device_probe.txt")),
            worktree_root=str(self.home / "trees"),
            run_queue_glob=str(self.home / "evidence" / "*" / "runs*"), run_queue_holder="run_queue.sh",
            run_queue_stop_words=("STOP", "YIELD", "NO-LEASE"),
            build_lock_dir=self.home / "buildlocks", build_lock_local="dev-build.lock",
            build_lock_remote="dev-build-remote-{name}.lock",
            build_lock_names=(("ci@worker-a.local", "wa"), ("ci@192.0.2.6", "laptop")),
        )
        fields.update(overrides)
        return DashboardConfig(**fields)

    def collector(self, clock: Callable[[], float] | None = None, **overrides: Any) -> Collector:
        """
        Args:
            clock: The collector's clock; pinned to PINNED_EPOCH when None.
            **overrides: DashboardConfig fields to change.

        Returns:
            A collector on this home.
        """
        config: DashboardConfig = self.config(**overrides)
        github: GitHubClient = GitHubClient("example/repo", self.FAKE_TOKEN, config.gh_binary)
        return Collector(config, github, clock=clock or (lambda: self.PINNED_EPOCH))

    def calls(self) -> list[str]:
        """
        Returns:
            Every fake command call so far.
        """
        return self.calls_log.read_text().splitlines() if self.calls_log.exists() else []

    def close(self) -> None:
        """
        Stops the fake ticks and removes the home.
        """
        for tick_process in self.tick_processes:
            try:
                os.killpg(tick_process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            tick_process.wait(timeout=5)
        for slot_lock_file in self._slot_lock_files:
            slot_lock_file.close()
        for helper in self.helper_processes:
            try:
                os.killpg(helper.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            helper.wait(timeout=5)
        self.temporary_directory.cleanup()

    def _write_script(self, name: str, text: str) -> None:
        path: Path = self.home / "bin" / name
        path.write_text(textwrap.dedent(text))
        path.chmod(0o755)


class HarnessEnvReaderTest(unittest.TestCase):
    def test_reads_only_the_wanted_keys_unquoted(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            env_path: Path = Path(directory) / "harness.env"
            env_path.write_text("GH_REPO=first/repo\nexport GH_TOKEN='abc 123'\nGH_REPO=owner/name  # comment\n"
                                "SECRET=zzz\n# GH_TOKEN=commented\n")
            values: dict[str, str] = HarnessEnvReader.read_keys(env_path, frozenset({"GH_TOKEN", "GH_REPO"}))
        self.assertEqual(values, {"GH_TOKEN": "abc 123", "GH_REPO": "owner/name"})

    def test_missing_file_reads_nothing(self) -> None:
        self.assertEqual(HarnessEnvReader.read_keys(Path("/nonexistent/harness.env"), frozenset({"GH_TOKEN"})), {})


class ParserTest(unittest.TestCase):
    def test_host_health_rows(self) -> None:
        rows = StateParsers.parse_host_health_tsv((FIXTURES / "host_health.tsv").read_text())
        self.assertEqual(len(rows), 6)
        self.assertEqual(rows[0].host, "ci@worker-a.local")
        self.assertEqual(rows[0].upload_mb_s, 65.68)
        self.assertEqual(rows[2].gpu_util_pct, 32.0)
        self.assertTrue(rows[3].upload_timed_out)
        self.assertIsNone(rows[3].upload_mb_s)
        self.assertEqual(rows[3].status, "slow")
        self.assertEqual(rows[5].status, "down")
        self.assertIsNone(rows[5].load1)
        self.assertEqual(rows[5].epoch, 1795656961.0)

    def test_streaks(self) -> None:
        streaks = StateParsers.parse_streaks((FIXTURES / "host_health.streaks").read_text())
        self.assertEqual(streaks["ci@192.0.2.6"].state, "demoted")
        self.assertEqual(streaks["ci@192.0.2.6"].bad_probes, 3)
        self.assertEqual(streaks["ci@worker-a.local"].good_probes, 275)

    def test_lease_status(self) -> None:
        status = StateParsers.parse_lease_status((FIXTURES / "lease_status.txt").read_text())
        self.assertEqual([card.host for card in status.remote_cards], ["ci@worker-a.local", "ci@192.0.2.6"])
        self.assertEqual(status.remote_cards[1].first_slot, 6)
        self.assertEqual(status.harness_only_hosts, ("ci@192.0.2.6", "ci@worker-a.local"))
        self.assertTrue(status.local_card_off)
        by_slot: dict[int | None, LeaseSlot] = {slot.slot: slot for slot in status.slots}
        self.assertEqual(len(status.slots), 7)
        self.assertEqual(by_slot[1].state, "free")
        self.assertEqual(by_slot[1].host, "local")
        self.assertEqual((by_slot[2].holder, by_slot[2].issue, by_slot[2].pid), ("owner", None, 4242))
        self.assertEqual(by_slot[3].state, "orphan")
        self.assertEqual(by_slot[4].issue, 2490)
        self.assertEqual(by_slot[4].tags, ("remote-all", "render"))
        self.assertEqual(by_slot[4].domain, 80)
        self.assertEqual(by_slot[4].host, "ci@worker-a.local")
        self.assertEqual(by_slot[6].host, "ci@192.0.2.6")
        self.assertIsNotNone(status.device)
        self.assertEqual((status.device.issue, status.device.tags), (2466, ("device",)))
        self.assertEqual(status.operator_priority, ("2490", "2488"))
        self.assertEqual([(wait.kind, wait.issue, wait.minutes, wait.tag) for wait in status.waits],
                         [("whole-device", 2467, 35, "priority"), ("single-slot", 2470, 12, "first in line")])
        self.assertEqual(status.unparsed_lines, ("something unexpected",))

    def test_lease_status_accepts_card_and_host_for_device(self) -> None:
        status = StateParsers.parse_lease_status(
            "remote card: ci@a (slots 1-2)\nremote host: ci@b (slots 3-3)\nlocal card: off\n"
            "wait whole-card issue 5 for 2 min\nwait whole-pool issue 6 for 3 min\n")
        self.assertEqual([(card.host, card.first_slot) for card in status.remote_cards], [("ci@a", 1), ("ci@b", 3)])
        self.assertTrue(status.local_card_off)
        self.assertEqual([(wait.kind, wait.issue) for wait in status.waits], [("whole-device", 5), ("whole-pool", 6)])
        self.assertEqual(status.unparsed_lines, ())

    def test_stalls_keep_the_newest(self) -> None:
        stalls = StateParsers.parse_stalls((FIXTURES / "stalls.log").read_text(), 2)
        self.assertEqual([stall.kind for stall in stalls], ["lock_wait", "device_idle"])
        self.assertEqual(stalls[0].key, "/tmp/dev-build-remote-dev-a.lock")
        self.assertTrue(stalls[1].detail.startswith("device 0 %"))

    def test_allocator_lines(self) -> None:
        lines = StateParsers.parse_allocator((FIXTURES / "allocator.log").read_text(), 10)
        self.assertEqual([line.kind for line in lines], ["chain", "spawn", "cooldown", "usage", "landing", "chain"])
        self.assertEqual(lines[0].time, "2026-11-26T01:00:13Z")
        self.assertEqual(lines[0].slot, "slot2")
        self.assertEqual(lines[1].fields["week"], "50")
        self.assertEqual(lines[1].fields["spawning"], "1")
        self.assertEqual(lines[4].fields["ready"], "1")
        self.assertEqual(lines[5].fields["claimable"], "2")

    def test_usage_cache(self) -> None:
        reading = StateParsers.parse_usage_cache((FIXTURES / "usage-cache.json").read_text())
        self.assertEqual((reading.five_hour_pct, reading.weekly_pct), (64.0, 52.0))
        self.assertEqual(reading.taken_at_epoch, 1795657080.0)
        with self.assertRaises(ValueError):
            StateParsers.parse_usage_cache("[]")

    def test_claim_file(self) -> None:
        self.assertEqual(StateParsers.parse_claim("claim.2464", "2464 18907 2026-11-26T01:00:51Z\n"),
                         ClaimRecord(2464, 18907, "2026-11-26T01:00:51Z"))
        self.assertIsNone(StateParsers.parse_claim("claim.guard", ""))
        self.assertIsNone(StateParsers.parse_claim("claim.12", "garbage"))

    def test_machine_probe(self) -> None:
        probe = StateParsers.parse_machine_probe((FIXTURES / "machine_probe.txt").read_text())
        self.assertEqual((probe.load1, probe.nproc), (8.12, 24))
        self.assertAlmostEqual(probe.mem_total_mb, 65481572 / 1024)
        self.assertAlmostEqual(probe.mem_used_mb, (65481572 - 18771028) / 1024)
        self.assertEqual(probe.gpu.name, "NVIDIA GeForce RTX 5080")
        self.assertEqual((probe.gpu.util_pct, probe.gpu.mem_used_mb, probe.gpu.mem_total_mb), (63.0, 5940.0, 16303.0))
        self.assertEqual([(process.pid, process.container) for process in probe.gpu_processes],
                         [(3557139, "dev-run-4"), (4100000, None)])
        self.assertEqual([container.name for container in probe.containers], ["dev-run-4", "sim_dev"])

    def test_machine_probe_without_device(self) -> None:
        probe = StateParsers.parse_machine_probe("load 0.5 4\n--- docker\n")
        self.assertIsNone(probe.gpu)
        self.assertEqual(probe.gpu_processes, ())

    def test_model_label(self) -> None:
        self.assertEqual(StateParsers.model_label("claude-opus-5-5"), "Opus 5.5")
        self.assertEqual(StateParsers.model_label("claude-sonnet-5-2"), "Sonnet 5.2")
        self.assertEqual(StateParsers.model_label("claude-sonnet-4-5-20250929"), "Sonnet 4.5")
        self.assertEqual(StateParsers.model_label("custom"), "custom")

    def test_tool_command(self) -> None:
        wrapped: str = ("source /x/snapshot.sh 2>/dev/null || true && eval 'timeout 60 bash -c '\"'\"'until grep -q "
                        "x f; do sleep 10; done'\"'\"'; date -u' < /dev/null && pwd -P >| /tmp/claude-cwd")
        self.assertEqual(StateParsers.tool_command(wrapped),
                         "timeout 60 bash -c 'until grep -q x f; do sleep 10; done'; date -u")
        self.assertIsNone(StateParsers.tool_command("plain command"))


class RedactionTest(unittest.TestCase):
    SHAPES: ClassVar[list[JsonDict]] = json.loads((FIXTURES / "secret_shapes.json").read_text())
    KEPT: ClassVar[list[JsonDict]] = json.loads((FIXTURES / "kept_shapes.json").read_text())
    BYPASS: ClassVar[list[JsonDict]] = json.loads((FIXTURES / "bypass_shapes.json").read_text())

    def test_every_secret_shape_is_masked(self) -> None:
        for shape in self.SHAPES:
            with self.subTest(shape["shape"]):
                masked: str = CommandRedactor.redact(f"tool {shape['text']} --next value")
                self.assertNotIn(shape["secret"], masked)
                self.assertIn(CommandRedactor.MASK, masked)
                self.assertIn("--next value", masked)

    def test_every_secret_shape_is_masked_in_a_summary(self) -> None:
        for shape in self.SHAPES:
            with self.subTest(shape["shape"]):
                summary: str = CommandRedactor.summarize(f"cd /w && tool {shape['text']}", max_arguments=8,
                                                         max_argument_chars=200)
                self.assertTrue(summary.startswith("tool "), summary)
                self.assertNotIn(shape["secret"], summary)
                self.assertIn(CommandRedactor.MASK, summary)

    def test_quoting_bypass_shapes_are_masked_in_the_summary(self) -> None:
        for shape in self.BYPASS:
            for max_arguments in (4, 12):
                with self.subTest(shape["shape"], max_arguments=max_arguments):
                    summary: str = CommandRedactor.summarize(shape["command"], max_arguments=max_arguments,
                                                             max_argument_chars=200)
                    masked_words: str = " ".join(CommandRedactor.redact_command(shape["command"]))
                    self.assertIn(CommandRedactor.MASK, masked_words)
                    for secret in shape["secrets"]:
                        self.assertNotIn(secret, summary)
                        self.assertNotIn(secret, masked_words)

    def test_quoting_bypass_shapes_are_masked_in_an_argv_summary(self) -> None:
        for shape in self.BYPASS:
            if shape["argv"] is None:
                continue
            with self.subTest(shape["shape"]):
                summary: str = CommandRedactor.summarize_argv(shape["argv"], max_arguments=12)
                masked_words: str = " ".join(CommandRedactor.redact_command(shlex.join(shape["argv"])))
                self.assertIn(CommandRedactor.MASK, masked_words)
                for secret in shape["secrets"]:
                    self.assertNotIn(secret, summary)
                    self.assertNotIn(secret, masked_words)

    def test_every_word_after_a_quoting_failure_is_masked(self) -> None:
        self.assertEqual(CommandRedactor.redact_command('tool --name "a b'), ["tool", "--name", CommandRedactor.MASK])
        self.assertEqual(CommandRedactor.redact_command("tool 'a"), ["tool", CommandRedactor.MASK])
        self.assertEqual(CommandRedactor.redact_command("tool --token=x --password y z --next w"),
                         ["tool", "--token=[redacted]", "--password", "[redacted]", "[redacted]", "--next", "w"])

    def test_scripts_nested_past_the_depth_limit_are_masked_whole(self) -> None:
        script: str = "tool --name plain"
        for _ in range(CommandRedactor.MAX_SCRIPT_DEPTH + 1):
            script = shlex.join(["bash", "-c", script])
        self.assertNotIn("plain", " ".join(CommandRedactor.redact_command(script)))

    def test_plain_values_are_kept(self) -> None:
        for kept in self.KEPT:
            with self.subTest(kept["text"]):
                self.assertIn(kept["kept"], CommandRedactor.redact(kept["text"]))

    def test_summary_names_the_working_command_briefly(self) -> None:
        self.assertEqual(CommandRedactor.summarize(
            "cd /w/app-2494 && ls env/service.env 2>/dev/null; DOCKER_HOST=ssh://ci@h timeout 1500 "
            "tools/dev/build.sh app > /tmp/p/build1.log 2>&1; echo rc=$?"),
            "timeout 1500 tools/dev/build.sh app +1 more")
        self.assertEqual(CommandRedactor.summarize(
            "P=/a/b; for i in $(seq 1 9); do sleep 60; done; grep -E ' rc=' $P/runs/status"), "sleep 60 +1 more")
        self.assertEqual(CommandRedactor.summarize(
            "timeout 560 bash -c \"until grep -q 'RS6 start' $P/runs/status; do sleep 20; done\"; date -u"),
            "timeout 560 bash -c 'grep -q RS6 \u2026 +1 more' +1 more")
        self.assertEqual(CommandRedactor.summarize("date -u; echo done"), "date -u +1 more")
        self.assertEqual(CommandRedactor.summarize("cat /very/long/path/to/some/evidence/folder/with/a/file.log"),
                         "cat \u2026/file.log")
        self.assertEqual(CommandRedactor.summarize(""), "")
        self.assertEqual(CommandRedactor.summarize_argv(("bash", "/h/bin/run_queue.sh", "--issue", "2490", "--packet",
                                                         "/p")), "run_queue.sh --issue 2490 \u2026")
        self.assertEqual(CommandRedactor.summarize_argv(("python3", "/h/bin/x.py", "--token", "abc")),
                         "x.py --token [redacted]")


class SecretSurfaceTest(unittest.TestCase):
    """
    Puts every secret shape where the dashboard looks, a tick's tool call, a lease holder's command line and the
    stall log, and requires none to reach the JSON or the page.
    """

    @linux_only
    def test_no_secret_reaches_the_json_or_the_page(self) -> None:
        shapes: list[JsonDict] = json.loads((FIXTURES / "secret_shapes.json").read_text())
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        tool_command: str = "GH_TOKEN=ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789 bash -c 'sleep 30' x " + " ".join(
            shape["text"] for shape in shapes)
        tick_pid: int = harness.start_fake_tick(issue=2464, slot=3, tool_command=tool_command)
        holder_script: Path = harness.home / "holder.sh"
        holder_script.write_text("sleep 30\n")
        holder = subprocess.Popen(["bash", str(holder_script)] + [word for shape in shapes for word in
                                                                  shlex.split(shape["text"])],
                                  start_new_session=True)
        self.addCleanup(lambda: (os.killpg(holder.pid, signal.SIGKILL), holder.wait()))
        holder_with_flag = subprocess.Popen(["bash", str(holder_script), "--token", "flag-value-2", "--issue", "7"],
                                            start_new_session=True)
        self.addCleanup(lambda: (os.killpg(holder_with_flag.pid, signal.SIGKILL), holder_with_flag.wait()))
        lease_text: str = (FIXTURES / "lease_status.txt").read_text()
        lease_text = lease_text.replace("slot 4 HELD 2490 432188", f"slot 4 HELD 2490 {holder.pid}")
        lease_text = lease_text.replace("slot 5 HELD 2490 432188", f"slot 5 HELD 2490 {holder_with_flag.pid}")
        (harness.home / "lease_override.txt").write_text(lease_text)
        time.sleep(0.2)

        collector: Collector = harness.collector()
        collector.refresh_github_in_place()
        snapshot = collector.snapshot()
        snapshot_json: str = json.dumps(snapshot, default=str)
        page: str = HtmlRenderer().render(snapshot, 30, "access")

        tick = next(tick for tick in snapshot["ticks"] if tick["pid"] == tick_pid)
        self.assertEqual(tick["doing"], "bash -c 'sleep 30' x GH_TOKEN=[redacted] \u2026")
        slots = {slot["slot"]: slot for slot in snapshot["leases"]["slots"]}
        self.assertEqual(slots[5]["holder_kind"], "holder.sh --token [redacted] \u2026")
        self.assertTrue(slots[4]["holder_kind"].startswith("holder.sh "))
        stall = snapshot["health"]["stalls"][0]
        self.assertIn("https://[redacted]@example.com/r.git", stall["detail"])
        self.assertIn("GH_TOKEN=[redacted]", stall["detail"])
        secrets_list: list[str] = [shape["secret"] for shape in shapes] + [
            "ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789", "stall-pass-1", "stall-token-1", "flag-value-2"]
        for secret in secrets_list:
            with self.subTest(secret=secret):
                self.assertNotIn(secret, snapshot_json)
                self.assertNotIn(html.escape(secret, quote=True), page)
                self.assertNotIn(secret, page)


    @linux_only
    def test_no_quoting_bypass_reaches_the_json_or_the_page(self) -> None:
        shapes: list[JsonDict] = json.loads((FIXTURES / "bypass_shapes.json").read_text())
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        pid_by_shape: dict[str, int] = {
            shape["shape"]: harness.start_fake_tick(issue=3000 + index, slot=index + 1, tool_command=shape["command"])
            for index, shape in enumerate(shapes)
        }
        kept = json.loads((FIXTURES / "kept_shapes.json").read_text())
        with (harness.state / "stalls.log").open("a") as stalls:
            for item in kept:
                stalls.write(f"2026-11-26T01:30:00Z STALL note local {item['text']}\n")
        collector: Collector = harness.collector()
        snapshot = collector.snapshot()
        snapshot_json: str = json.dumps(snapshot, default=str)
        page: str = HtmlRenderer().render(snapshot, 30, "access")
        server = dashboard.DashboardHttpServer(0, collector, HtmlRenderer(), 30, (harness.FAKE_TOKEN,), "access")
        self.addCleanup(server.server_close)
        with patch.object(collector, "snapshot", return_value=snapshot):
            server.publish_in_place()
        manifest = server.routes["/api/sections"].plain.decode()
        fragments = [server.routes["/api/sections/" + digest].plain.decode() for digest in
                     json.loads(manifest).values()]
        surface = html.unescape(manifest + "".join(fragments))
        self.assertIn(CommandRedactor.MASK, surface)
        for item in kept:
            self.assertIn(item["kept"], surface)
        doing_by_pid: dict[int, str] = {tick["pid"]: tick["doing"] for tick in snapshot["ticks"]}
        for shape in shapes:
            with self.subTest(shape["shape"]):
                doing: str = doing_by_pid[pid_by_shape[shape["shape"]]]
                self.assertTrue(doing)
                for secret in shape["secrets"]:
                    self.assertNotIn(secret, doing)
                    self.assertNotIn(secret, snapshot_json)
                    self.assertNotIn(secret, page)
                    self.assertNotIn(secret, surface)


class TickMappingTest(unittest.TestCase):
    def test_newest_claim_wins_then_lease_then_github(self) -> None:
        claims = (ClaimRecord(2001, 10, "2026-11-25T01:00:00Z"), ClaimRecord(2464, 10, "2026-11-26T01:00:00Z"),
                  ClaimRecord(2300, 30, "2026-11-26T00:00:00Z"))
        leases = (LeaseSlot(4, "held", "2490", 2490, 10, "x", 80, (), "h"),
                  LeaseSlot(5, "held", "2490", 2490, 20, "x", 81, (), "h"))
        mapping = ProcessScanner.map_pids_to_issues(claims, leases, {20: 9999, 40: 2600})
        self.assertEqual(mapping[10], (2464, "claim"))
        self.assertEqual(mapping[20], (2490, "lease"))
        self.assertEqual(mapping[30], (2300, "claim"))
        self.assertEqual(mapping[40], (2600, "github"))

    @linux_only
    def test_finds_a_live_tick_with_slot_log_issue_and_tool_call(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        pid: int = harness.start_fake_tick(issue=2464, slot=3)
        scanner: ProcessScanner = ProcessScanner(harness.state)
        processes = scanner.scan_processes()
        candidates = scanner.find_ticks(processes)
        self.assertEqual([candidate.process.pid for candidate in candidates], [pid])
        self.assertEqual(candidates[0].slot, 3)
        self.assertEqual(candidates[0].log, "tick-20261126T013000Z-slot3.log")
        claims = (StateParsers.parse_claim("claim.2464", (harness.state / "locks" / "claim.2464").read_text()),
                  StateParsers.parse_claim("claim.2001", (harness.state / "locks" / "claim.2001").read_text()))
        ticks = scanner.describe_ticks(candidates, processes, claims, (), {}, time.time())
        self.assertEqual(len(ticks), 1)
        tick = ticks[0]
        self.assertEqual((tick.issue, tick.issue_source), (2464, "claim"))
        self.assertEqual((tick.model, tick.model_label, tick.lane), ("claude-opus-5-5", "Opus 5.5", "standard"))
        self.assertEqual(tick.doing, "sleep 30")
        self.assertEqual(tick.tool_shells, 1)
        self.assertGreaterEqual(tick.age_s, 0)

    @linux_only
    def test_ignores_claude_p_outside_the_harness(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        process = subprocess.Popen(["bash", "-c", "exec -a claude bash -p -c 'sleep 30; :' x other --model m"],
                                   stdout=subprocess.DEVNULL, start_new_session=True)
        self.addCleanup(lambda: (os.killpg(process.pid, signal.SIGKILL), process.wait()))
        time.sleep(0.2)
        scanner: ProcessScanner = ProcessScanner(harness.state)
        self.assertEqual(scanner.find_ticks(scanner.scan_processes()), ())


class QueueTest(unittest.TestCase):
    def test_groups(self) -> None:
        issues = json.loads((FIXTURES / "issues.json").read_text())
        groups = IssueQueue(QueueLabels(queue="ai:plan", lane="ai:lane")).classify(
            issues, {2380: "CLOSED", 2499: "MERGED", 1229: "OPEN"}, {2501: 4242}, {2464: 18907})
        numbers = {name: [issue.number for issue in members] for name, members in groups.items()}
        self.assertEqual(numbers["running"], [2464])
        self.assertEqual(sorted(numbers["claimable"]), [2490, 2495, 2502])
        self.assertEqual(numbers["claimable"][0], 2495)
        self.assertEqual(sorted(numbers["held"]), [2503, 2504])
        self.assertEqual(sorted(numbers["operator"]), [2063, 2467])
        self.assertEqual(numbers["signoff"], [2200])
        self.assertEqual(sorted(numbers["other"]), [1240, 2388, 2501])
        reasons = {issue.number: issue.reason for members in groups.values() for issue in members}
        self.assertEqual(reasons[2388], "waits on #2063")
        self.assertEqual(reasons[1240], "not labelled ai:plan")
        self.assertEqual(reasons[2501], "claimed by live pid 4242")
        self.assertEqual(reasons[2490], "resumable: no live claimant")
        self.assertEqual(reasons[2464], "tick pid 18907")
        self.assertEqual(reasons[2503], "held: ai:low waits while a ai:first issue is claimable")
        self.assertEqual(reasons[2504], "held: a new ai:lane issue waits while a ai:first issue is claimable")
        last_comment = {issue.number: issue.last_comment_at for members in groups.values() for issue in members}
        self.assertEqual(last_comment[2464], "2026-11-26T01:27:15Z")
        self.assertIsNone(last_comment[2495])

    def test_first_gate(self) -> None:
        def issue(number: int, *labels: str, body: str = "") -> JsonDict:
            return {"number": number, "title": f"t{number}", "body": body,
                    "labels": {"nodes": [{"name": name} for name in ("ai", "ai:plan", *labels)]}}

        def split(issues: list[JsonDict], first_only: list[JsonDict] | None = None) -> tuple[list[int], list[int]]:
            groups = IssueQueue(QueueLabels(queue="ai:plan", lane="ai:lane")).classify(issues, {}, {}, {}, first_only)
            return (sorted(member.number for member in groups["claimable"]),
                    sorted(member.number for member in groups["held"]))

        self.assertEqual(split([issue(1), issue(2, "ai:low")]), ([1], [2]))
        self.assertEqual(split([issue(2, "ai:low"), issue(3, "ai:low")]), ([2, 3], []))
        self.assertEqual(split([issue(4, "ai:lane"), issue(2, "ai:low")]), ([4], [2]))
        self.assertEqual(split([issue(4, "ai:lane"), issue(5, "ai:lane", "ai:first")]), ([5], [4]))
        outside_first = {"number": 9, "title": "t9", "body": "", "labels": {"nodes": [{"name": "ai:first"}]}}
        self.assertEqual(split([issue(4, "ai:lane"), issue(2, "ai:low")], [outside_first]), ([], [2, 4]))
        self.assertEqual(split([issue(4, "ai:lane")], [outside_first]), ([], [4]))
        parked_first = {"number": 9, "title": "t9", "body": "",
                        "labels": {"nodes": [{"name": "ai:first"}, {"name": "ai:operator"}]}}
        self.assertEqual(split([issue(4, "ai:lane")], [parked_first]), ([4], []))
        listed_outside_queue = {"number": 8, "title": "t8", "body": "",
                                "labels": {"nodes": [{"name": "ai"}, {"name": "ai:first"}]}}
        self.assertEqual(split([issue(4, "ai:lane"), listed_outside_queue]), ([], [4]))
        self.assertEqual(split([issue(2, "ai:low"), issue(1, body="Depends on: #77")]), ([2], []))

    def test_unknown_dependency_counts_as_open(self) -> None:
        issues = [{"number": 1, "title": "t", "body": "Depends on: #77", "labels": {"nodes": [{"name": "ai"}]}}]
        groups = IssueQueue(QueueLabels()).classify(issues, {}, {}, {})
        self.assertEqual(groups["other"][0].reason, "waits on #77")

    def test_parse_dependencies_reads_every_line(self) -> None:
        self.assertEqual(IssueQueue.parse_dependencies("Depends on: #1, #2\ntext\nDepends on (later): #3"),
                         (1, 2, 3))


class ClaimableParityTest(unittest.TestCase):
    """
    Runs the real bin/claimable.sh, behind a fake gh, on the same issues the queue panel groups, and requires the
    same claimable count.
    """

    FAKE_GH: ClassVar[str] = textwrap.dedent("""\
        #!/usr/bin/env python3
        import json, os, sys
        arguments = sys.argv[1:]
        fixtures = os.environ["PARITY_FIXTURES"]
        if arguments[:2] == ["issue", "list"]:
            label = arguments[arguments.index("--label") + 1]
            path = os.path.join(fixtures, label.replace(":", "_") + ".json")
            print(open(path).read() if os.path.exists(path) else "[]")
        elif arguments[:2] == ["issue", "view"] and "state" in arguments:
            print("CLOSED" if os.path.exists(os.path.join(fixtures, "closed." + arguments[2])) else "OPEN")
    """)
    SCENARIOS: ClassVar[list[tuple[str, list[tuple[int, tuple[str, ...], str]], tuple[int, ...]]]] = [
        ("plain then low", [(1, ("ai", "ai:plan"), ""), (2, ("ai", "ai:plan", "ai:low"), "")], ()),
        ("only low", [(2, ("ai", "ai:plan", "ai:low"), ""), (3, ("ai", "ai:plan", "ai:low"), "")], ()),
        ("new round and low", [(4, ("ai", "ai:plan", "ai:lane"), ""), (2, ("ai", "ai:plan", "ai:low"), "")], ()),
        ("new round behind first", [(4, ("ai", "ai:plan", "ai:lane"), ""),
                                    (5, ("ai", "ai:plan", "ai:lane", "ai:first"), "")], ()),
        ("first outside every ai issue", [(4, ("ai", "ai:plan", "ai:lane"), ""), (2, ("ai", "ai:plan", "ai:low"), ""),
                                          (9, ("ai:first",), "")], ()),
        ("parked first outside", [(4, ("ai", "ai:plan", "ai:lane"), ""), (9, ("ai:first", "ai:operator"), "")], ()),
        ("listed first outside the queue", [(4, ("ai", "ai:plan", "ai:lane"), ""), (8, ("ai", "ai:first"), "")], ()),
        ("blocked plain leaves low", [(2, ("ai", "ai:plan", "ai:low"), ""),
                                      (1, ("ai", "ai:plan"), "Depends on: #77")], ()),
        ("met dependency holds low", [(2, ("ai", "ai:plan", "ai:low"), ""),
                                      (1, ("ai", "ai:plan"), "Depends on: #78")], (78,)),
        ("first round and low", [(5, ("ai", "ai:plan", "ai:lane", "ai:first"), ""),
                                 (2, ("ai", "ai:plan", "ai:low"), "")], ()),
        ("blocked first outside", [(4, ("ai", "ai:plan", "ai:lane"), ""),
                                   (9, ("ai:first",), "Depends on: #77")], ()),
    ]

    def test_dashboard_count_matches_claimable_sh(self) -> None:
        for name, issues, closed in self.SCENARIOS:
            with self.subTest(name), tempfile.TemporaryDirectory() as directory:
                root: Path = Path(directory)
                (root / "fake").mkdir()
                (root / "fx").mkdir()
                (root / "state" / "locks").mkdir(parents=True)
                fake_gh: Path = root / "fake" / "gh"
                fake_gh.write_text(self.FAKE_GH)
                fake_gh.chmod(0o755)
                listed: dict[str, list[JsonDict]] = {}
                graphql_nodes: list[JsonDict] = []
                first_only: list[JsonDict] = []
                for number, labels, body in issues:
                    for label in labels:
                        listed.setdefault(label, []).append(
                            {"number": number, "body": body, "labels": [{"name": label} for label in labels]})
                    node: JsonDict = {"number": number, "title": f"t{number}", "body": body,
                                      "labels": {"nodes": [{"name": label} for label in labels]}}
                    if "ai" in labels:
                        graphql_nodes.append(node)
                    elif "ai:first" in labels:
                        first_only.append(node)
                for label, members in listed.items():
                    (root / "fx" / (label.replace(":", "_") + ".json")).write_text(json.dumps(members))
                for number in closed:
                    (root / "fx" / f"closed.{number}").touch()
                completed = subprocess.run(
                    [str(REPO_ROOT / "bin" / "claimable.sh")], capture_output=True, text=True, timeout=60,
                    env=dict(os.environ, PATH=f"{root / 'fake'}:{os.environ['PATH']}", HARNESS_HOME=str(root),
                             PARITY_FIXTURES=str(root / "fx"), GH_REPO="o/r", ISSUE_LABEL="ai:plan",
                             LANE_LABEL="ai:lane"),
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                groups = IssueQueue(QueueLabels(queue="ai:plan", lane="ai:lane")).classify(
                    graphql_nodes, {number: "CLOSED" for number in closed}, {}, {}, first_only)
                self.assertEqual(len(groups["claimable"]), int(completed.stdout.strip()))


class CollectorAndPageTest(unittest.TestCase):
    def setUp(self) -> None:
        self.harness: FixtureHarness = FixtureHarness()
        self.addCleanup(self.harness.close)
        self.tick_pid: int = self.harness.start_fake_tick(issue=2464, slot=3)
        self.collector: Collector = self.harness.collector()
        self.collector.refresh_local_sample_in_place()
        self.collector.refresh_github_in_place()
        self.collector.refresh_claimable_in_place()
        self.snapshot: dict[str, Any] = self.collector.snapshot()

    def test_publisher_keeps_its_own_cadence_whatever_the_source_intervals(self) -> None:
        collector: Collector = self.harness.collector(local_interval_s=0, github_interval_s=0,
                                                      claimable_interval_s=3600)
        published: list[float] = []
        collector.start(lambda: published.append(time.monotonic()), 1.0)
        self.addCleanup(collector.stop)
        deadline: float = time.monotonic() + 5.0
        while len(published) < 3 and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertGreaterEqual(len(published), 3)

    def test_a_refresh_that_raises_is_stored_as_the_failure_and_the_loop_goes_on(self) -> None:
        calls: list[int] = []

        def refresh() -> None:
            calls.append(1)
            if len(calls) >= 3:
                self.collector._stop_event.set()
            raise ZeroDivisionError("boom")

        with self.assertLogs(level="ERROR"):
            self.collector._run_periodically("github", 0.01, refresh)
        self.assertEqual(len(calls), 3)
        github = self.collector.snapshot()["sources"]["github"]
        self.assertEqual(github["error"], "ZeroDivisionError: boom")
        self.assertIsNotNone(github["fetched_at"])

    @linux_only
    def test_every_panel_has_data(self) -> None:
        snapshot = self.snapshot
        self.assertEqual([machine["key"] for machine in snapshot["machines"]],
                         ["local", "ci@worker-a.local", "ci@192.0.2.6"])
        worker = snapshot["machines"][1]
        self.assertEqual((worker["status"], worker["upload_mb_s"], worker["load1"]), ("ok", 64.1, 5.5))
        self.assertEqual(worker["gpu"]["util_pct"], 63.0)
        self.assertEqual(len(worker["history"]["gpu_util"]), 3)
        self.assertEqual([slot["slot"] for slot in worker["slots"]], [4, 5])
        demoted = snapshot["machines"][2]
        self.assertEqual(demoted["status"], "down")
        self.assertIn("moved last (3 bad probes)", [signal["text"] for signal in demoted["signals"]])
        self.assertEqual(demoted["history"]["down"], [1795656961.0])
        self.assertEqual(snapshot["machines"][0]["card_note"], "harness off (left to its owner)")
        self.assertEqual(snapshot["machines"][0]["gpu"]["name"], "Example Device")
        self.assertEqual([process["pid"] for process in snapshot["machines"][0]["gpu_processes"]], [4242])

        leases = snapshot["leases"]
        self.assertTrue(leases["available"])
        self.assertEqual(len(leases["slots"]), 7)
        self.assertEqual([(hold["issue"], hold["slots"]) for hold in leases["whole_card_holds"]], [(2490, [4, 5])])
        self.assertEqual(leases["operator_priority"], ["2490", "2488"])
        self.assertEqual(leases["device"]["issue"], 2466)
        self.assertEqual(len(leases["waits"]), 2)

        self.assertEqual(len(snapshot["ticks"]), 1)
        tick = snapshot["ticks"][0]
        self.assertEqual((tick["pid"], tick["issue"], tick["issue_source"], tick["slot"]), (self.tick_pid, 2464,
                                                                                        "claim", 3))
        self.assertEqual(tick["title"], "feat: give each region of material a persistent rest state")

        queue = snapshot["queue"]
        self.assertEqual(queue["counts"]["running"], 1)
        self.assertEqual(queue["claimable_count"], 3)
        self.assertEqual(sorted(issue["number"] for issue in queue["groups"]["held"]), [2503, 2504])
        listed = [issue["number"] for members in queue["groups"].values() for issue in members]
        self.assertNotIn(2600, listed)
        self.assertEqual((queue["claimable_sh"]["value"], queue["claimable_sh"]["unverified"]), (2, False))
        self.assertEqual(queue["allocator_claimable"],
                         {"value": "2", "at": "2026-11-26T01:32:06Z", "unverified": False})
        self.assertEqual(snapshot["harness"]["waiting_count"], 3)
        self.assertFalse(snapshot["harness"]["waiting_count_from_failing_read"])
        self.assertIsNone(queue["error"])
        self.assertEqual([pull["number"] for pull in snapshot["landings"]], [2496, 2499])

        self.assertEqual(len(snapshot["health"]["stalls"]), 4)
        self.assertEqual(snapshot["health"]["allocator"][-1]["kind"], "chain")
        usage = snapshot["usage"]
        self.assertEqual((usage["five_hour_pct"], usage["five_hour_band"], usage["allowed_band"]), (64.0, "half", "half"))
        self.assertEqual(usage["age_s"], 120)
        harness = snapshot["harness"]
        self.assertEqual(harness["last_allowed"], 10)
        self.assertEqual(harness["tick_models"], ["claude-opus-5-5", "claude-sonnet-5-2"])
        self.assertEqual([model["model"] for model in harness["cooling_models"]], ["claude-sonnet-5-2"])
        self.assertTrue(harness["work_waiting"])

    def test_calls_are_read_only(self) -> None:
        calls: list[str] = self.harness.calls()
        self.assertTrue(calls)
        for call in calls:
            if call.startswith("resource-status"):
                self.assertEqual(call, "resource-status status")
            elif call.startswith("gh "):
                arguments: list[str] = json.loads(call[3:])
                self.assertEqual(arguments[0], "api")
                query: str = next((a for a in arguments if a.startswith("query=")), "")
                self.assertNotIn("mutation", query)
                if arguments[1] != "graphql":
                    self.assertEqual(arguments[1:3], ["-X", "GET"])
            else:
                self.assertEqual(call, "claimable.sh label=ai:plan token=set repo=example/repo")
        self.assertFalse(any(path.name.startswith("dashboard") for path in (self.harness.state / "locks").iterdir()))
        self.assertTrue((self.harness.home / "dashboard-data" / "local_history.tsv").exists())

    def test_page_renders_every_panel_and_escapes(self) -> None:
        page: str = HtmlRenderer().render(self.snapshot, 30)
        for expected in ("Harness status", '<meta http-equiv="refresh" content="30;url=/', "ci@worker-a.local",
                         "moved last (3 bad probes)", "Resource slots and leases", "whole device", "operator priority",
                         "Running ticks", "#2464", "Opus 5.5", "Claimable", "from this read (ai:plan)",
                         "claimable.sh 2", "held back by the first-before-low gate: 2", "#2503",
                         "Waiting on the operator", "Waiting for sign-off", "#2200", "Landings", "#2499",
                         "Stalls", "lock_wait", "Allocator", "spawn", "usage", "<svg", "polyline",
                         "prefers-color-scheme:dark", "work waiting"):
            self.assertIn(expected, page)
        self.assertNotIn("<script>alert(1)</script>", page)
        self.assertIn("&lt;script&gt;alert(1)&lt;/script&gt;", page)
        self.assertEqual(page.count("<script"), 1)
        self.assertIn('<script defer src="/dashboard.js', page)

    def test_no_token_in_page_or_json(self) -> None:
        token: str = FixtureHarness.FAKE_TOKEN
        self.assertNotIn(token, json.dumps(self.snapshot, default=str))
        self.assertNotIn(token, HtmlRenderer().render(self.snapshot, 30))
        self.assertNotIn(token, repr(self.collector.github))
        self.assertNotIn(token, repr(self.collector.config))

    @linux_only
    def test_gh_failure_is_redacted_and_keeps_the_last_read(self) -> None:
        (self.harness.home / "gh-fail").touch()
        self.collector.refresh_github_in_place()
        os.environ["FAKE_CLAIMABLE_FAIL"] = "1"
        self.addCleanup(os.environ.pop, "FAKE_CLAIMABLE_FAIL", None)
        self.collector.refresh_claimable_in_place()
        snapshot = self.collector.snapshot()
        token: str = FixtureHarness.FAKE_TOKEN
        self.assertIn("Bad credentials for token [redacted]", snapshot["queue"]["error"])
        self.assertIn("boom [redacted]", snapshot["queue"]["claimable_sh"]["error"])
        self.assertEqual(snapshot["queue"]["counts"]["running"], 1)
        self.assertEqual(snapshot["queue"]["claimable_sh"]["value"], 2)
        self.assertNotIn(token, json.dumps(snapshot, default=str))
        self.assertNotIn(token, HtmlRenderer().render(snapshot, 30))

    def test_paused_by_file_or_commented_cron_line_stops_the_work_waiting_signal(self) -> None:
        (self.harness.home / "cron-off").touch()
        self.collector.refresh_local_sample_in_place()
        harness = self.collector.snapshot()["harness"]
        self.assertEqual((harness["paused"], harness["paused_reason"]), (True, "no active bin/tick.sh cron line"))
        self.assertFalse(harness["work_waiting"])
        (self.harness.home / "cron-off").unlink()
        (self.harness.state / "paused").touch()
        self.collector.refresh_local_sample_in_place()
        harness = self.collector.snapshot()["harness"]
        self.assertEqual((harness["paused"], harness["paused_reason"]), (True, "state/paused present"))

    @linux_only
    def test_counts_taken_while_the_own_read_fails_are_unverified(self) -> None:
        clock_epoch: list[float] = [1795656600.0]
        collector: Collector = self.harness.collector(clock=lambda: clock_epoch[0])
        collector.refresh_github_in_place()
        clock_epoch[0] = 1795656900.0
        (self.harness.home / "gh-fail").touch()
        collector.refresh_github_in_place()
        clock_epoch[0] = 1795657000.0
        collector.refresh_claimable_in_place()
        snapshot = collector.snapshot()
        queue = snapshot["queue"]
        self.assertTrue(queue["allocator_claimable"]["unverified"])
        self.assertTrue(queue["claimable_sh"]["unverified"])
        self.assertEqual(snapshot["harness"]["waiting_count"], 3)
        self.assertTrue(snapshot["harness"]["waiting_count_from_failing_read"])
        unverified_by_time = {line["time"]: line["claimable_unverified"] for line in snapshot["health"]["allocator"]}
        self.assertTrue(unverified_by_time["2026-11-26T01:32:06Z"])
        self.assertFalse(unverified_by_time["2026-11-26T01:00:13Z"])
        self.assertEqual(queue["github_outages"], [{"from": "2026-11-26T01:30:00Z", "to": None}])
        page: str = HtmlRenderer().render(snapshot, 30)
        self.assertIn("claimable unverified", page)
        self.assertIn("at the last good read", page)

        (self.harness.home / "gh-fail").unlink()
        clock_epoch[0] = 1795657200.0
        collector.refresh_github_in_place()
        clock_epoch[0] = 1795657260.0
        collector.refresh_claimable_in_place()
        snapshot = collector.snapshot()
        self.assertTrue(snapshot["queue"]["allocator_claimable"]["unverified"])
        self.assertFalse(snapshot["queue"]["claimable_sh"]["unverified"])
        self.assertFalse(snapshot["harness"]["waiting_count_from_failing_read"])
        self.assertEqual(snapshot["queue"]["github_outages"],
                         [{"from": "2026-11-26T01:30:00Z", "to": "2026-11-26T01:40:00Z"}])

    @linux_only
    def test_restart_serves_the_saved_github_read(self) -> None:
        restarted: Collector = self.harness.collector()
        restarted._load_github_cache_in_place()
        snapshot = restarted.snapshot()
        self.assertEqual(snapshot["queue"]["counts"]["claimable"], 3)
        self.assertEqual(snapshot["queue"]["age_s"], 0)

    def test_local_history_survives_a_restart(self) -> None:
        restarted: Collector = self.harness.collector()
        restarted._load_local_history_in_place()
        self.assertEqual(len(restarted._local_history), 1)
        self.assertEqual(restarted._local_history[0][0], FixtureHarness.PINNED_EPOCH)


class HttpServerTest(unittest.TestCase):
    def serve(self, harness: FixtureHarness) -> tuple[dashboard.DashboardHttpServer, int]:
        """
        Starts the dashboard's server on a free port for one test.

        Args:
            harness: The scratch harness home.

        Returns:
            The server and its port.
        """
        options = dashboard.DashboardApp.parse_options([
            "--harness-home", str(harness.home), "--data-dir", str(harness.home / "data"), "--port", "0",
            "--gh", str(harness.home / "bin" / "gh"),
            "--usage-cache", str(harness.home / "usage-cache.json"), "--remote-interval", "0",
        ])
        app = dashboard.DashboardApp(options)
        self.assertEqual(app.config.repo, "example/repo")
        app.collector.refresh_github_in_place()
        server = dashboard.DashboardHttpServer(0, app.collector, HtmlRenderer(), 30, (app.github_token,),
                                               self.ACCESS_TOKEN)
        self.addCleanup(server.server_close)
        server.publish_in_place()
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.shutdown)
        return server, server.server_address[1]

    @staticmethod
    def request(port: int, path: str, host: str | None, headers: dict[str, str] | None = None
                ) -> tuple[int, dict[str, str], str]:
        """
        Sends one GET with a chosen Host header, or none.

        Args:
            port: The server's port.
            path: Path and query.
            host: The Host header; None sends none.
            headers: Additional request headers.

        Returns:
            Status, headers and body.
        """
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
        try:
            connection.putrequest("GET", path, skip_host=True, skip_accept_encoding=True)
            if host is not None:
                connection.putheader("Host", host)
            for name, value in (headers or {}).items():
                connection.putheader(name, value)
            connection.endheaders()
            response = connection.getresponse()
            body = response.read()
            if body and response.getheader("Content-Encoding") == "gzip":
                body = gzip.decompress(body)
            return response.status, {name.lower(): value for name, value in response.getheaders()}, body.decode()
        finally:
            connection.close()

    ACCESS_TOKEN: ClassVar[str] = "test-access-token-0123456789"

    @linux_only
    def test_serves_page_json_and_health_on_loopback_without_the_github_token(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        server, port = self.serve(harness)
        self.assertEqual(server.server_address[0], "127.0.0.1")
        self.assertEqual(server.url, f"http://127.0.0.1:{port}/?t={self.ACCESS_TOKEN}")
        host: str = f"127.0.0.1:{port}"
        status, _, body = self.request(port, f"/healthz?t={self.ACCESS_TOKEN}", host)
        self.assertEqual((status, body), (200, "ok\n"))
        status, headers, page = self.request(port, f"/?t={self.ACCESS_TOKEN}", host)
        self.assertEqual((status, headers["content-type"]), (200, "text/html; charset=utf-8"))
        status, _, body = self.request(port, f"/api/state?t={self.ACCESS_TOKEN}", f"localhost:{port}")
        self.assertEqual(status, 200)
        state = json.loads(body)
        status, _, _ = self.request(port, f"/instance/harness.env?t={self.ACCESS_TOKEN}", host)
        self.assertEqual(status, 404)
        self.assertIn("<title>Fixture &lt;b&gt;harness&lt;/b&gt;</title>", page)
        self.assertIn("<h1>Fixture &lt;b&gt;harness&lt;/b&gt;</h1>", page)
        self.assertIn("lane: Sonnet 5.2 for ai:lane", page)
        self.assertIn(f'content="30;url=/?t={self.ACCESS_TOKEN}"', page)
        self.assertIn(f'href="/api/state?t={self.ACCESS_TOKEN}"', page)
        self.assertNotIn(self.ACCESS_TOKEN, body)
        self.assertEqual(state["repo"], "example/repo")
        self.assertEqual(state["queue"]["counts"]["running"] + state["queue"]["counts"]["claimable"], 4)
        self.assertNotIn(FixtureHarness.FAKE_TOKEN, page)
        self.assertNotIn(FixtureHarness.FAKE_TOKEN, body)
        self.assertEqual(server.scrub(f"x {FixtureHarness.FAKE_TOKEN} y"), "x [redacted] y")

    def test_refuses_a_foreign_host_and_a_missing_or_wrong_access_token(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        _, port = self.serve(harness)
        good: str = f"?t={self.ACCESS_TOKEN}"
        for host in ("attacker.example", f"attacker.example:{port}", f"127.0.0.1:{port + 1}", "127.0.0.1",
                     f"0.0.0.0:{port}", "", None):
            for path in ("/healthz", "/" + good, "/api/state" + good):
                with self.subTest(host=host, path=path):
                    status, _, body = self.request(port, path, host)
                    self.assertEqual(status, 421)
                    self.assertNotIn("Harness status", body)
        for path in ("/", "/api/state", "/?t=", "/api/state?t=wrong", "/?x=" + self.ACCESS_TOKEN,
                     f"/api/state?t={self.ACCESS_TOKEN}x"):
            with self.subTest(path=path):
                status, _, body = self.request(port, path, f"127.0.0.1:{port}")
                self.assertEqual(status, 403)
                self.assertNotIn("repo", body)
        status, _, _ = self.request(port, "/" + good, f"LOCALHOST:{port}")
        self.assertEqual(status, 200)

    def test_every_response_carries_the_security_headers(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        _, port = self.serve(harness)
        for path, host, expected_status in (("/?t=" + self.ACCESS_TOKEN, f"127.0.0.1:{port}", 200),
                                            ("/api/state?t=" + self.ACCESS_TOKEN, f"127.0.0.1:{port}", 200),
                                            ("/", f"127.0.0.1:{port}", 403),
                                            ("/", "evil.example", 421),
                                            ("/missing?t=" + self.ACCESS_TOKEN, f"127.0.0.1:{port}", 404)):
            with self.subTest(path=path, host=host):
                status, headers, _ = self.request(port, path, host)
                self.assertEqual(status, expected_status)
                self.assertEqual(headers["cross-origin-resource-policy"], "same-origin")
                self.assertEqual(headers["x-content-type-options"], "nosniff")
                self.assertEqual(headers["referrer-policy"], "no-referrer")
                policy: str = headers["content-security-policy"]
                self.assertIn("default-src 'none'", policy)
                self.assertIn("frame-ancestors 'none'", policy)
                self.assertNotIn("http", policy)
                self.assertNotIn("*", policy)

    def test_conditional_get_and_gzip_variants(self) -> None:
        harness = FixtureHarness()
        self.addCleanup(harness.close)
        server, port = self.serve(harness)
        host = f"127.0.0.1:{port}"
        query = "?t=" + self.ACCESS_TOKEN
        with patch.object(server.collector, "snapshot", side_effect=AssertionError("collection during GET")):
            for path in ("/", "/api/state", "/api/sections", "/dashboard.js"):
                with self.subTest(path=path):
                    status, plain, body = self.request(port, path + query, host)
                    self.assertEqual(status, 200)
                    self.assertEqual(plain["etag"], f'"{hashlib.sha256(body.encode()).hexdigest()}"')
                    status, zipped, decoded = self.request(port, path + query, host,
                                                           {"Accept-Encoding": "br, gzip;q=0.8",
                                                            "If-None-Match": plain["etag"]})
                    self.assertEqual((status, decoded, zipped["content-encoding"]), (200, body, "gzip"))
                    self.assertNotEqual(plain["etag"], zipped["etag"])
                    self.assertLess(int(zipped["content-length"]), len(body.encode()))
                    status, headers, empty = self.request(port, path + query, host, {"Accept-Encoding": "gzip",
                                                            "If-None-Match": '"old", W/' + zipped["etag"]})
                    self.assertEqual((status, empty, headers["etag"]), (304, "", zipped["etag"]))
                    self.assertEqual(headers["cache-control"], "private, no-cache")
                    self.assertEqual(headers["vary"], "Accept-Encoding")
            for accepted in ("br", "gzip;q=0, *;q=1", "gzip;q=0.4, identity;q=1"):
                status, headers, _ = self.request(port, "/api/state" + query, host, {"Accept-Encoding": accepted})
                self.assertEqual(status, 200)
                self.assertNotIn("content-encoding", headers)

    def test_sections_track_content_and_require_the_token(self) -> None:
        harness = FixtureHarness()
        self.addCleanup(harness.close)
        server, port = self.serve(harness)
        host = f"127.0.0.1:{port}"
        query = "?t=" + self.ACCESS_TOKEN
        _, headers, index = self.request(port, "/api/sections" + query, host)
        before = json.loads(index)
        fragment = "/api/sections/" + before["ticks"]
        for path in ("/healthz", "/dashboard.js", "/api/sections", fragment):
            for credential in ("", "?t=wrong"):
                self.assertEqual(self.request(port, path + credential, host, {"If-None-Match": "*"})[0], 403)
            self.assertEqual(self.request(port, path + query, "foreign.example")[0], 421)
        _, _, page = self.request(port, "/" + query, host)
        self.assertIn(f'<noscript><meta http-equiv="refresh" content="30;url=/{query}"></noscript>', page)
        self.assertIn(f'data-section="ticks" data-version="{before["ticks"]}"', page)
        status, _, body = self.request(port, fragment + query, host)
        self.assertEqual(status, 200)
        self.assertIn("Running ticks", body)
        self.assertNotIn("<style", body)
        self.assertEqual(hashlib.sha256(body.encode()).hexdigest(), before["ticks"])

        _, _, state = self.request(port, "/api/state" + query, host)
        snapshot = json.loads(state)
        snapshot["repo"] = "different/repo " + harness.FAKE_TOKEN
        with patch.object(server.collector, "snapshot", return_value=snapshot):
            server.publish_in_place()
        status, _, index = self.request(port, "/api/sections" + query, host, {"If-None-Match": headers["etag"]})
        after = json.loads(index)
        self.assertEqual(status, 200)
        self.assertEqual([name for name in before if before[name] != after[name]], ["header"])
        fragment_body = self.request(port, "/api/sections/" + after["header"] + query, host)[2]
        self.assertIn("different/repo [redacted]", fragment_body)
        self.assertNotIn(harness.FAKE_TOKEN, fragment_body)

    def test_request_log_never_holds_the_access_token(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        _, port = self.serve(harness)
        captured = io.StringIO()
        with contextlib.redirect_stderr(captured):
            self.request(port, f"/api/state?t={self.ACCESS_TOKEN}", f"127.0.0.1:{port}")
        self.assertIn("/api/state?t=[token]", captured.getvalue())
        self.assertNotIn(self.ACCESS_TOKEN, captured.getvalue())

    def test_private_file_is_owner_only_and_never_follows_a_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path: Path = Path(directory) / "dashboard.url"
            path.write_text("old")
            path.chmod(0o644)
            dashboard.DashboardApp.write_private_file(path, "http://127.0.0.1:1/?t=x\n")
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertEqual(path.read_text(), "http://127.0.0.1:1/?t=x\n")
            target: Path = Path(directory) / "elsewhere"
            target.write_text("keep")
            link: Path = Path(directory) / "planted.url"
            link.symlink_to(target)
            with self.assertRaises(OSError):
                dashboard.DashboardApp.write_private_file(link, "secret")
            self.assertEqual(target.read_text(), "keep")

    def test_labels_come_from_the_env_file_and_the_flag_overrides_the_queue_label(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        base: list[str] = ["--harness-home", str(harness.home), "--data-dir", str(harness.home / "data")]
        labels = dashboard.DashboardApp(dashboard.DashboardApp.parse_options(base)).config.labels
        self.assertEqual((labels.queue, labels.first, labels.low, labels.wip, labels.lane),
                         ("ai:plan", "ai:first", "ai:low", "ai:wip", "ai:lane"))
        config = dashboard.DashboardApp(dashboard.DashboardApp.parse_options(base)).config
        self.assertEqual((config.title, config.lane_model), ("Fixture <b>harness</b>", "claude-sonnet-5-2"))
        self.assertEqual(config.resource_status_command, (str(harness.home / "bin" / "resource-status"), "status"))
        self.assertEqual(config.run_queue_stop_words, ("STOP", "YIELD", "NO-LEASE"))
        self.assertEqual((config.build_lock_dir, config.build_lock_local, config.build_lock_remote),
                         (harness.home / "buildlocks", "dev-build.lock", "dev-build-remote-{name}.lock"))
        self.assertEqual(config.build_lock_names, (("ci@worker-a.local", "wa"), ("ci@192.0.2.6", "laptop")))
        overridden = dashboard.DashboardApp(dashboard.DashboardApp.parse_options(base + ["--queue-label", "ai"]))
        self.assertEqual(overridden.config.labels.queue, "ai")
        self.assertEqual(dashboard.DashboardApp.ENV_KEYS, frozenset({
            "GH_TOKEN", "GH_REPO", "ISSUE_LABEL", "WIP_LABEL", "SIGNOFF_LABEL", "OPERATOR_LABEL", "READY_LABEL",
            "LANE_LABEL", "FIRST_LABEL", "LOW_LABEL", "LANE_MODEL", "DASHBOARD_TITLE", "RESOURCE_STATUS_CMD",
            "DEVICE_PROBE_CMD", "QUEUE_DEPTH_CMD", "HARNESS_STATE", "USAGE_CACHE", "WORKTREE_ROOT",
            "RUN_QUEUE_GLOB", "RUN_QUEUE_HOLDER", "RUN_QUEUE_STOP_WORDS", "RUN_QUEUE_LOG", "BUILD_LOCK_DIR",
            "BUILD_LOCK_LOCAL", "BUILD_LOCK_REMOTE", "BUILD_LOCK_NAMES"}))

    @linux_only
    def test_start_script_runs_it_detached_and_stops_it(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port: int = probe.getsockname()[1]
        environment: dict[str, str] = dict(os.environ, HARNESS_HOME=str(harness.home),
                                           DASHBOARD_DATA_DIR=str(harness.home / "data"))
        script: str = str(REPO_ROOT / "bin" / "dashboard.sh")
        options: list[str] = ["--port", str(port), "--gh", str(harness.home / "bin" / "gh"),
                              "--usage-cache", str(harness.home / "usage-cache.json"),
                              "--remote-interval", "0", "--claimable-interval", "0"]
        self.addCleanup(subprocess.run, [script, "stop"], env=environment, capture_output=True)
        started = subprocess.run([script, *options], env=environment, capture_output=True, text=True, timeout=30)
        self.assertEqual(started.returncode, 0, started.stderr)
        url: str = started.stdout.strip()
        self.assertRegex(url, rf"^http://127\.0\.0\.1:{port}/\?t=[A-Za-z0-9_-]{{20,}}$")
        access_token: str = url.split("?t=", 1)[1]
        data: Path = harness.home / "data"
        self.assertEqual(data.stat().st_mode & 0o777, 0o700)
        for name in ("dashboard.url", "dashboard.log", "dashboard.pid"):
            self.assertEqual((data / name).stat().st_mode & 0o077, 0, name)
        self.assertEqual((data / "dashboard.url").read_text().strip(), url)
        again = subprocess.run([script, *options], env=environment, capture_output=True, text=True, timeout=30)
        self.assertIn("already running", again.stdout)
        self.assertIn(url, again.stdout)
        with urllib.request.urlopen(url.replace("/?t=", "/api/state?t="), timeout=30) as response:
            self.assertEqual(json.loads(response.read())["repo"], "example/repo")
        with self.assertRaises(urllib.error.HTTPError) as refused:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/api/state", timeout=30)
        self.assertEqual(refused.exception.code, 403)
        status = subprocess.run([script, "status"], env=environment, capture_output=True, text=True, timeout=30)
        self.assertEqual(status.returncode, 0)
        self.assertIn(url, status.stdout)
        stopped = subprocess.run([script, "stop"], env=environment, capture_output=True, text=True, timeout=30)
        self.assertIn("stopped pid", stopped.stdout)
        self.assertFalse((data / "dashboard.url").exists())
        status = subprocess.run([script, "status"], env=environment, capture_output=True, text=True, timeout=30)
        self.assertEqual(status.returncode, 1)
        log: str = (data / "dashboard.log").read_text()
        self.assertNotIn(FixtureHarness.FAKE_TOKEN, log)
        self.assertIn(url, log)
        self.assertNotIn(f"?t={access_token} HTTP", log)

    def test_once_prints_a_snapshot_without_the_token(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        completed = subprocess.run(
            [sys.executable, str(REPO_ROOT / "bin" / "dashboard.py"), "--harness-home", str(harness.home),
             "--data-dir", str(harness.home / "data"), "--gh",
             str(harness.home / "bin" / "gh"), "--usage-cache", str(harness.home / "usage-cache.json"),
             "--remote-interval", "0", "--once"],
            capture_output=True, text=True, timeout=60,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        state = json.loads(completed.stdout)
        self.assertEqual(state["queue"]["claimable_sh"]["value"], 2)
        self.assertNotIn(FixtureHarness.FAKE_TOKEN, completed.stdout + completed.stderr)


class RunQueueTest(unittest.TestCase):
    QUEUES: ClassVar[Path] = FIXTURES / "runqueues"

    @staticmethod
    def holder(pid: int) -> dict[int, ProcessInfo]:
        return {pid: ProcessInfo(pid, 1, 0.0, ("bash", "/h/bin/run_queue.sh", "--issue", "2502"))}

    def test_a_queue_running_a_job(self) -> None:
        lease = RunQueueReader.parse_queue_log((self.QUEUES / "running" / "queue.log").read_text())
        self.assertEqual((lease.holder_pid, lease.want, lease.busy_since, lease.last_host),
                         (4101, "any", None, "ci@192.0.2.60"))
        status = RunQueueReader.parse_status((self.QUEUES / "running" / "status").read_text())
        self.assertEqual((status.job, status.job_host, status.job_started, status.finished_at),
                         ("RS6-on-s1323-render", "ci@192.0.2.60", "2026-11-26T03:03:10Z", None))

    def test_a_queue_waiting_for_a_device_keeps_only_the_last_holders_busy_streak(self) -> None:
        lease = RunQueueReader.parse_queue_log((self.QUEUES / "waiting" / "queue.log").read_text())
        self.assertEqual((lease.holder_pid, lease.want), (4201, "ci@192.0.2.60"))
        self.assertEqual((lease.busy_since, lease.busy_reason),
                         ("2026-11-26T02:40:00Z", "no remote card is free, alone and quiet"))
        self.assertIsNone(RunQueueReader.parse_status((self.QUEUES / "waiting" / "status").read_text()).job)

    def test_a_stop_word_ends_the_running_job(self) -> None:
        text: str = "RS1 start 2026-11-26T02:47:25Z host=h\nNO-LEASE: lost the slot\n"
        self.assertEqual(RunQueueReader.parse_status(text).job, "RS1")
        stop = RunQueueReader("", "", ("STOP", "NO-LEASE"), 60).stop_pattern
        self.assertIsNone(RunQueueReader.parse_status(text, stop).job)
        self.assertEqual(RunQueueReader.parse_status("RS1 start 2026-11-26T02:47:25Z\nSTOPPED x\n", stop).job, "RS1")

    def test_a_released_queue_is_finished(self) -> None:
        status = RunQueueReader.parse_status((self.QUEUES / "finished" / "status").read_text())
        self.assertEqual((status.job, status.finished_at, status.granted_host),
                         (None, "2026-11-26T02:51:16Z", "ci@worker-b.local"))

    def test_read_queues_names_state_issue_and_liveness(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root: Path = Path(directory)
            now = time.time()
            for name, packet in (("running", "2026-11-26-2502-push"), ("waiting", "2026-11-26-2464-rest"),
                                 ("finished", "2026-11-26-2490-sub")):
                runs = root / packet / "runs"
                shutil.copytree(self.QUEUES / name, runs)
                for path in runs.iterdir():
                    os.utime(path, (now, now))
            old: Path = root / "2026-11-20-2000-old" / "runs"
            shutil.copytree(self.QUEUES / "finished", old)
            for path in old.iterdir():
                os.utime(path, (now - 86400, now - 86400))
            processes = {**self.holder(4101), **self.holder(4201)}
            queues = RunQueueReader(str(root / "*" / "runs*"), "run_queue.sh", (), 6 * 3600).read_queues(
                processes, now)
        by_issue = {queue.issue: queue for queue in queues}
        self.assertEqual(sorted(by_issue), [2464, 2490, 2502])
        self.assertEqual((by_issue[2502].state, by_issue[2502].job), ("running", "RS6-on-s1323-render"))
        self.assertEqual(by_issue[2464].state, "waiting_device")
        self.assertEqual((by_issue[2490].state, by_issue[2490].alive), ("finished", False))


class BuildLockTest(unittest.TestCase):
    def test_parses_lslocks_waiters_and_the_lock_file(self) -> None:
        waiters = BuildLockReader.parse_lslocks_waiters((FIXTURES / "lslocks.json").read_text())
        self.assertEqual(waiters, {"/tmp/dev-build-remote-gpu-a.lock": [2131760, 2387677],
                                   "/tmp/dev-build-remote-dev-a.lock": [99]})
        self.assertEqual(BuildLockReader.parse_lslocks_waiters("not json"), {})
        self.assertEqual(BuildLockReader.parse_lock_file((FIXTURES / "build_lock.txt").read_text()),
                         (2088784, "/home/x/trees/app-2464"))
        self.assertEqual(BuildLockReader.parse_lock_file(""), (None, None))

    @linux_only
    def test_reads_a_live_holder_and_waiter(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        holder_pid, waiter_pid = harness.hold_build_lock("wa", holder_issue=2495, waiter_issue=2388)
        scanner: ProcessScanner = ProcessScanner(harness.state)
        lanes = BuildLockReader({"ci@worker-a.local": "wa"}, harness.home / "buildlocks", "dev-build.lock",
                                "dev-build-remote-{name}.lock", 5, str(harness.home / "trees")).read_lanes(
            ["ci@worker-a.local", "ci@192.0.2.6"], scanner.scan_processes(), {})
        by_name = {lane.name: lane for lane in lanes}
        self.assertEqual(sorted(by_name), ["192-0-2-6", "local", "wa"])
        self.assertEqual((by_name["wa"].holder.pid, by_name["wa"].holder.issue), (holder_pid, 2495))
        self.assertEqual([(waiter.pid, waiter.issue) for waiter in by_name["wa"].waiters], [(waiter_pid, 2388)])
        self.assertIsNone(by_name["local"].holder)


class FlowBuilderTest(unittest.TestCase):
    NOW: ClassVar[float] = 1795657200.0

    def inputs(self, **overrides: Any) -> FlowInputs:
        issues = json.loads((FIXTURES / "issues.json").read_text())
        groups = IssueQueue(QueueLabels(queue="ai:plan", lane="ai:lane")).classify(
            issues, {2380: "CLOSED", 2499: "MERGED"}, {}, {})
        tick = TickProcess(11, "claude-opus-5-5", "Opus 5.5", "standard", 600, 1, None, 2464, "claim", "sleep 30",
                           5, 1)
        landing = TickProcess(12, "claude-opus-5-5", "Opus 5.5", "landing", 120, 2, None, 2201, "claim", None,
                              None, 0)
        running = RunQueue("2026-11-26-2502-x", "runs", 2502, 41, True, "running", "any", "RS6-p1",
                           "ci@192.0.2.6", "2026-11-26T01:30:00Z", None, None, "ci@192.0.2.6", None, self.NOW)
        waiting = RunQueue("2026-11-26-2490-x", "runs", 2490, 42, True, "waiting_device", "ci@192.0.2.6", None, None,
                           None, "2026-11-26T01:28:00Z", "slot limit GH_TOKEN=ghp_AbCdEfGhIjKlMnOpQrStUvWxYz012345",
                           None, None, self.NOW)
        lanes = (BuildLane("wa", "ci@worker-a.local", "/l/wa", LockParty(51, 2495, "app-2495", True),
                           (LockParty(52, 2388, "app-2388", True),)),)
        slots = (LeaseSlot(4, "held", "2488", 2488, 61, "2026-11-26T01:00:00Z", 80, ("remote-all", "no-cost"),
                           "ci@worker-a.local"),)
        machines = [
            {"key": "local", "name": "box", "status": "ok", "gpu": {"name": "RTX", "util_pct": 5.0}, "load1": 1.0,
             "nproc": 8},
            {"key": "ci@worker-a.local", "name": "ci@worker-a.local", "status": "ok",
             "gpu": {"util_pct": 40.0, "mem_used_mb": 5000.0, "mem_total_mb": 16000.0}, "load1": 4.0, "nproc": 16},
            {"key": "ci@192.0.2.6", "name": "ci@192.0.2.6", "status": "ok", "gpu": {"util_pct": 0.0},
             "load1": 2.0, "nproc": 24},
        ]
        fields: dict[str, Any] = dict(
            groups=groups, ticks=(tick, landing), queues=(running, waiting), lanes=lanes, lease_slots=slots,
            closed_issues=json.loads((FIXTURES / "closed.json").read_text()), machines=machines,
            titles={int(issue["number"]): (issue["title"], issue["url"]) for issue in issues}, now=self.NOW)
        fields.update(overrides)
        return FlowInputs(**fields)

    def builder(self) -> FlowBuilder:
        return FlowBuilder(QueueLabels(queue="ai:plan", lane="ai:lane"),
                           {"ci@worker-a.local": "wa", "ci@192.0.2.6": "laptop"}, 6 * 3600, device_lanes=True,
                           build_lanes=True)

    def test_each_issue_lands_in_its_column(self) -> None:
        flow = self.builder().build(self.inputs())
        cards = {card["issue"]: card for card in flow["cards"]}
        expected: dict[int, tuple[str, str]] = {
            2502: ("running", "running RS6-p1 on laptop"),
            2488: ("running", "holding wa"),
            2490: ("waiting_device", "waiting for a device"),
            2495: ("compiling", "compiling on wa"),
            2388: ("compiling", "waiting for the wa compile lane"),
            2201: ("landing", "landing"),
            2464: ("claimed", "claimed"),
            2504: ("waiting", "held back by the gate"),
            2503: ("waiting", "held back by the gate"),
            2467: ("held", "on the operator"),
            2200: ("held", "waiting for sign-off"),
            2492: ("done", "done"),
        }
        for issue, (column, label) in expected.items():
            with self.subTest(issue=issue):
                self.assertEqual((cards[issue]["column"], cards[issue]["label"]), (column, label))
        self.assertNotIn(1240, cards)
        self.assertNotIn(2310, cards)
        self.assertEqual(cards[2388]["detail"], "1 in line for wa")
        self.assertIn("[redacted]", cards[2490]["detail"])
        self.assertNotIn("ghp_", json.dumps(flow))
        self.assertTrue(cards[2490]["detail"].startswith("BUSY 12 min: slot limit"))

    def test_edges_run_to_the_lanes_held_and_waited_for(self) -> None:
        flow = self.builder().build(self.inputs())
        edges = sorted((edge["issue"], edge["device"], edge["lane"], edge["kind"]) for edge in flow["edges"])
        self.assertEqual(edges, [
            (2388, "ci@worker-a.local", "compile", "waits"),
            (2488, "ci@worker-a.local", "device", "holds"),
            (2490, "ci@192.0.2.6", "device", "waits"),
            (2495, "ci@worker-a.local", "compile", "holds"),
            (2502, "ci@192.0.2.6", "device", "holds"),
        ])
        device = {item["key"]: item for item in flow["devices"]}
        self.assertTrue(device["ci@192.0.2.6"]["idle_with_queue"])
        self.assertEqual(device["ci@192.0.2.6"]["idle_flag"], "device idle, queue waiting")
        held = self.builder().build(self.inputs(lease_slots=(LeaseSlot(
            6, "held", "2502", 2502, 62, "2026-11-26T01:00:00Z", 82, ("remote-all",), "ci@192.0.2.6"),)))
        self.assertEqual({item["key"]: item for item in held["devices"]}["ci@192.0.2.6"]["idle_flag"],
                         "0 % while held, queue waiting")
        self.assertFalse(device["ci@worker-a.local"]["idle_with_queue"])
        self.assertEqual(device["ci@worker-a.local"]["holders"], ["#2488 slot 4 remote-all no-cost"])
        self.assertEqual(device["ci@worker-a.local"]["compile"], "compile: #2495, 1 waiting")
        for edge in flow["edges"]:
            self.assertEqual(edge["y2"], device[edge["device"]]["y"] + device[edge["device"]]["h"])

    def test_columns_hold_at_most_their_cap(self) -> None:
        flow = self.builder().build(self.inputs())
        for column in flow["columns"]:
            shown = [card for card in flow["cards"] if card["column"] == column["key"]]
            self.assertEqual(len(shown), column["count"] - column["hidden"])
            self.assertLessEqual(len(shown), FlowBuilder.MAX_CARDS)


class FlowTrackerTest(unittest.TestCase):
    @staticmethod
    def flow(cards: list[tuple[int, int, int, str]]) -> JsonDict:
        return {"cards": [{"issue": issue, "x": x, "y": y, "label": label} for issue, x, y, label in cards],
                "edges": [{"issue": issue, "device": "d", "lane": "device", "kind": "holds"}
                          for issue, _, _, label in cards if label.startswith("running")]}

    def test_moves_fresh_cards_transitions_and_restart(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path: Path = Path(directory) / "flow_state.json"
            tracker: FlowTracker = FlowTracker(path, animate_s=20)
            first = self.flow([(1, 10, 100, "claimed"), (2, 10, 160, "claimed")])
            tracker.update_in_place(first, 1000.0)
            self.assertFalse(any(card["fresh"] or card["moved"] for card in first["cards"]))
            self.assertEqual(first["transitions"], [])

            second = self.flow([(1, 300, 100, "running RS6-p1 on laptop"), (2, 10, 100, "claimed"),
                                (3, 10, 160, "claimed")])
            tracker.update_in_place(second, 1030.0)
            cards = {card["issue"]: card for card in second["cards"]}
            self.assertEqual((cards[1]["moved"], cards[1]["from_x"], cards[1]["from_y"]), (True, 10, 100))
            self.assertEqual((cards[2]["moved"], cards[2]["from_y"]), (True, 160))
            self.assertTrue(cards[3]["fresh"])
            self.assertTrue(second["edges"][0]["fresh"])
            self.assertEqual(second["transitions"][0]["issue"], 1)
            self.assertEqual((second["transitions"][0]["from"], second["transitions"][0]["to"]),
                             ("claimed", "running RS6-p1 on laptop"))

            within = self.flow([(1, 300, 100, "running RS6-p1 on laptop"), (2, 10, 100, "claimed"),
                                (3, 10, 160, "claimed")])
            tracker.update_in_place(within, 1045.0)
            self.assertTrue({card["issue"]: card for card in within["cards"]}[1]["moved"])
            later = self.flow([(1, 300, 100, "running RS6-p1 on laptop"), (3, 10, 160, "claimed")])
            tracker.update_in_place(later, 1060.0)
            self.assertFalse(any(card["moved"] for card in later["cards"]))
            self.assertEqual((later["transitions"][0]["issue"], later["transitions"][0]["to"]), (2, "off the board"))

            crowded = self.flow([(1, 300, 100, "running RS6-p1 on laptop")])
            crowded["overflow"] = [{"issue": 3, "column": "claimed", "label": "claimed"}]
            tracker.update_in_place(crowded, 1070.0)
            self.assertEqual(len(crowded["transitions"]), 2)
            shown_again = self.flow([(1, 300, 100, "running RS6-p1 on laptop"), (3, 10, 160, "claimed")])
            tracker.update_in_place(shown_again, 1080.0)
            self.assertTrue({card["issue"]: card for card in shown_again["cards"]}[3]["fresh"])
            self.assertEqual(len(shown_again["transitions"]), 2)

            restarted: FlowTracker = FlowTracker(path, animate_s=20)
            restarted.load()
            again = self.flow([(1, 300, 100, "running RS6-p1 on laptop"), (3, 10, 160, "claimed")])
            restarted.update_in_place(again, 1100.0)
            self.assertFalse(any(card["fresh"] for card in again["cards"]))
            self.assertEqual(len(again["transitions"]), 2)


class MachineCardTest(unittest.TestCase):
    def test_containers_fold_into_a_details_list(self) -> None:
        snapshot: JsonDict = {"generated_epoch": 0, "machines": [{
            "key": "local", "name": "box", "kind": "local", "status": "ok", "history": {}, "slots": [],
            "containers": [{"name": "dev-run-4", "status": "Up 8 minutes", "image": "sim:1"},
                           {"name": "sim_dev", "status": "Up 3 weeks", "image": "sim:2"}],
        }]}
        page: str = HtmlRenderer().render(snapshot, 30)
        self.assertIn("<details><summary>containers <span class=\"sub\">2</span></summary>", page)
        self.assertIn("<li><span class=\"line\" title=\"sim:1\">dev-run-4</span>", page)
        self.assertNotIn("dev-run-4 <span class=\"sub\">Up 8 minutes</span>; ", page)


class FlowPageTest(unittest.TestCase):
    @linux_only
    def test_the_collector_builds_a_live_flow_and_the_page_draws_it(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        tick_pid: int = harness.start_fake_tick(issue=2464, slot=3)
        harness.start_fake_queue(2502, "runs-render", (FIXTURES / "runqueues" / "running" / "status").read_text()
                                 .replace("ci@192.0.2.60", "ci@192.0.2.6"),
                                 "holder {pid} start 2026-11-26T01:20:00Z queue=/q want=any deadline=1\n")
        harness.start_fake_queue(2504, "runs", "", "holder {pid} start 2026-11-26T01:20:00Z queue=/q "
                                 "want=ci@192.0.2.6 deadline=1\n2026-11-26T01:30:00Z BUSY slot limit 2/2\n")
        harness.hold_build_lock("wa", holder_issue=2495, waiter_issue=2388)
        clock_epoch: list[float] = [FixtureHarness.PINNED_EPOCH]
        collector: Collector = harness.collector(clock=lambda: clock_epoch[0])
        collector.refresh_github_in_place()
        first = collector.snapshot()
        cards = {card["issue"]: card for card in first["flow"]["cards"]}
        self.assertEqual(cards[2464]["column"], "claimed")
        self.assertEqual((cards[2502]["column"], cards[2502]["label"]),
                         ("running", "running RS6-on-s1323-render on laptop"))
        self.assertEqual(cards[2504]["column"], "waiting_device")
        self.assertEqual(cards[2490]["label"], "holding wa")
        self.assertEqual(cards[2495]["column"], "compiling")
        self.assertEqual(cards[2388]["label"], "waiting for the wa compile lane")
        self.assertEqual(cards[2492]["column"], "done")
        self.assertEqual(first["sources"]["activity"]["error"], None)

        (harness.state / "locks" / "claim.2464").unlink()
        os.killpg(harness.tick_processes[0].pid, signal.SIGKILL)
        harness.tick_processes[0].wait(timeout=5)
        clock_epoch[0] += 30
        second = collector.snapshot()
        self.assertNotIn(tick_pid, [tick["pid"] for tick in second["ticks"]])
        moved = {card["issue"]: card for card in second["flow"]["cards"]}[2464]
        self.assertEqual(moved["column"], "waiting")
        self.assertTrue(moved["moved"])
        self.assertEqual(second["flow"]["transitions"][0]["issue"], 2464)

        page: str = HtmlRenderer().render(second, 30, "access")
        for expected in ('class="flow-canvas"', 'class="fdev', "laptop", "compile: #2495, 1 waiting",
                         'class="fcard waiting moved"', "--dx:", '<path class="fedge device holds',
                         'class="fedge compile waits', "Waiting for a device", "#2464</b> claimed →",
                         "&lt;script&gt;alert(1)&lt;/script&gt;"):
            self.assertIn(expected, page)
        self.assertEqual(page.count("<script"), 1)
        self.assertIn('<script defer src="/dashboard.js', page)
        self.assertNotIn(FixtureHarness.FAKE_TOKEN, page + json.dumps(second, default=str))


class BareInstanceTest(unittest.TestCase):
    """
    An instance with no resource, device, run-queue or build-lock key: those panels and their sources stay off.
    """

    FAKE_GH: ClassVar[str] = textwrap.dedent("""\
        #!/usr/bin/env python3
        import json
        print(json.dumps({"data": {"repository": {"issues": {"pageInfo": {"hasNextPage": False}, "nodes": []}},
                                   "search": {"nodes": []}}}))
    """)

    def test_the_collector_reads_no_optional_source_and_the_page_leaves_their_panels_out(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home: Path = Path(directory)
            (home / "state" / "logs").mkdir(parents=True)
            config = DashboardConfig(harness_home=home, data_dir=home / "data", repo=None, remote_interval_s=0,
                                     usage_cache=home / "usage-cache.json", crontab_command=("true",))
            collector: Collector = Collector(config, None, clock=lambda: FixtureHarness.PINNED_EPOCH)
            commands: list[list[str]] = []
            real_run: Callable[..., Any] = subprocess.run

            def recording_run(arguments: list[str], *args: Any, **kwargs: Any) -> Any:
                commands.append(list(arguments))
                return real_run(arguments, *args, **kwargs)

            with patch("dashboard_lib.collector.subprocess.run", recording_run), \
                    patch("dashboard_lib.buildlocks.subprocess.run", recording_run):
                collector.refresh_local_sample_in_place()
                collector.refresh_remote_probes_in_place()
                snapshot: JsonDict = collector.snapshot()
        self.assertIsNone(snapshot["leases"])
        self.assertIsNone(snapshot["health"]["stalls"])
        self.assertEqual(sorted(snapshot["sources"]),
                         ["claimable_sh", "github", "local", "paused", "procs", "remote"])
        self.assertEqual(snapshot["flow"]["lanes"], [])
        self.assertEqual([column["key"] for column in snapshot["flow"]["columns"]],
                         ["waiting", "claimed", "landing", "held", "done"])
        self.assertEqual([machine["key"] for machine in snapshot["machines"]], ["local"])
        self.assertIsNone(snapshot["machines"][0]["gpu"])
        self.assertIsNone(snapshot["flow"]["devices"][0]["holders"])
        self.assertIsNone(snapshot["flow"]["devices"][0]["compile"])
        self.assertEqual([command[0] for command in commands], ["bash", "true"])
        self.assertNotIn("--- device", commands[0][2])
        for name in ("local", "paused", "remote"):
            self.assertIsNone(snapshot["sources"][name]["error"], name)
        page: str = HtmlRenderer().render(snapshot, 30)
        for absent in ("Resource slots", "Stalls", "Device", "lease:", "<th>Leases", "compile lane", "▾",
                       "no slot held", '"grid split"'):
            self.assertNotIn(absent, page)
        self.assertIn("<title>Harness status</title>", page)

    def test_once_against_a_bare_instance_prints_a_snapshot_without_those_panels_or_errors(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root: Path = Path(directory)
            (root / "state").mkdir()
            fake_gh: Path = root / "gh"
            fake_gh.write_text(self.FAKE_GH)
            fake_gh.chmod(0o755)
            completed = subprocess.run(
                [sys.executable, str(REPO_ROOT / "bin" / "dashboard.py"), "--harness-home", str(root),
                 "--gh", str(fake_gh), "--usage-cache", str(root / "usage-cache.json"), "--remote-interval", "0",
                 "--claimable-interval", "0", "--once"],
                capture_output=True, text=True, timeout=60,
                env={key: value for key, value in dict(
                    os.environ, HARNESS_INSTANCE_DIR=str(FIXTURES / "instance"),
                    HARNESS_STATE=str(root / "state")).items() if key != "HARNESS_ENV"},
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            state: JsonDict = json.loads(completed.stdout)
            self.assertTrue((root / "state" / "dashboard" / "local_history.tsv").exists())
        self.assertEqual((state["repo"], state["title"]), ("example/app", "Harness status"))
        self.assertIsNone(state["leases"])
        self.assertIsNone(state["health"]["stalls"])
        self.assertEqual(state["flow"]["lanes"], [])
        self.assertNotIn("lease", state["sources"])
        self.assertNotIn("activity", state["sources"])
        self.assertNotIn("host_health_tsv", state["sources"])
        failing = {name: source["error"] for name, source in state["sources"].items() if source["error"]}
        if not sys.platform.startswith("linux"):
            failing.pop("procs", None)
        self.assertEqual(failing, {})

    def test_harness_env_wins_over_the_instance_dir(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            other: Path = Path(directory) / "other.env"
            other.write_text("GH_REPO=example/other\n")
            with patch.dict(os.environ, {"HARNESS_INSTANCE_DIR": str(FIXTURES / "instance")}):
                os.environ.pop("HARNESS_ENV", None)
                self.assertEqual(dashboard.DashboardApp.env_path(Path(directory)),
                                 FIXTURES / "instance" / "harness.env")
                os.environ["HARNESS_ENV"] = str(other)
                self.assertEqual(dashboard.DashboardApp.env_path(Path(directory)), other)
            with patch.dict(os.environ, {}, clear=False):
                os.environ.pop("HARNESS_ENV", None)
                os.environ.pop("HARNESS_INSTANCE_DIR", None)
                self.assertEqual(dashboard.DashboardApp.env_path(Path(directory)),
                                 Path(directory) / "instance" / "harness.env")



class InstanceConfigTest(unittest.TestCase):
    """
    What the dashboard takes from harness.env: paths, commands and the placeholders they may name.
    """

    def test_a_worktree_names_its_issue_under_the_configured_root_only(self) -> None:
        self.assertEqual(StateParsers.issue_from_path("/srv/trees/app-42/src", "/srv/trees/"), 42)
        self.assertEqual(StateParsers.issue_from_path("/srv/trees/app-7", "/srv/trees"), 7)
        self.assertIsNone(StateParsers.issue_from_path("/srv/elsewhere/app-42", "/srv/trees"))
        self.assertIsNone(StateParsers.issue_from_path("/srv/trees/app-42", ""))
        self.assertEqual(StateParsers.issue_from_path("/srv/runs/2026-11-26-2502-push/runs", ""), 2502)

    def test_the_queue_log_name_is_configurable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            runs: Path = Path(directory) / "2026-11-26-2502-push" / "runs"
            shutil.copytree(FIXTURES / "runqueues" / "running", runs)
            (runs / "queue.log").rename(runs / "custom.log")
            processes = {4101: ProcessInfo(4101, 1, 0.0, ("bash", "/h/bin/run_queue.sh"))}
            default = RunQueueReader(str(Path(directory) / "*" / "runs"), "", (), 6 * 3600).read_queues(
                processes, time.time())
            custom = RunQueueReader(str(Path(directory) / "*" / "runs"), "", (), 6 * 3600, "custom.log").read_queues(
                processes, time.time())
        self.assertEqual((default[0].holder_pid, default[0].alive), (None, False))
        self.assertEqual((custom[0].holder_pid, custom[0].state), (4101, "running"))

    def test_commands_expand_only_the_two_harness_placeholders(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        env_file: Path = harness.home / "instance" / "harness.env"
        env_file.write_text(env_file.read_text() + textwrap.dedent("""\
            RESOURCE_STATUS_CMD="$HARNESS_HOME/bin/resource-status status '$HOME' \\$PATH"
            DEVICE_PROBE_CMD="${HARNESS_INSTANCE_DIR}/bin/probe --name=$HARNESS_HOMEX"
            QUEUE_DEPTH_CMD=$HARNESS_INSTANCE_DIR/bin/count
        """))
        with patch.dict(os.environ, {"HARNESS_INSTANCE_DIR": str(harness.home / "instance")}):
            os.environ.pop("HARNESS_ENV", None)
            config = dashboard.DashboardApp(dashboard.DashboardApp.parse_options(
                ["--harness-home", str(harness.home)])).config
        self.assertEqual(config.resource_status_command,
                         (str(harness.home / "bin" / "resource-status"), "status", "$HOME", "$PATH"))
        self.assertEqual(config.device_probe_command,
                         (str(harness.home / "instance" / "bin" / "probe"), "--name=$HARNESS_HOMEX"))
        self.assertEqual(config.queue_depth_command, (str(harness.home / "instance" / "bin" / "count"),))
        self.assertEqual(config.worktree_root, str(harness.home / "trees"))
        self.assertEqual(config.run_queue_log, "queue.log")

    def test_state_and_usage_cache_come_from_the_env_file_and_the_environment_wins(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        env_file: Path = harness.home / "instance" / "harness.env"
        env_file.write_text(env_file.read_text() + "HARNESS_STATE=$HARNESS_HOME/other-state\n"
                                                    "USAGE_CACHE=$HOME/usage.json\nRUN_QUEUE_LOG=custom.log\n")
        base: list[str] = ["--harness-home", str(harness.home)]
        with patch.dict(os.environ, {"HARNESS_INSTANCE_DIR": str(harness.home / "instance")}):
            for key in ("HARNESS_ENV", "HARNESS_STATE", "USAGE_CACHE"):
                os.environ.pop(key, None)
            config = dashboard.DashboardApp(dashboard.DashboardApp.parse_options(base)).config
            self.assertEqual((config.state_dir, config.data_dir, config.usage_cache, config.run_queue_log),
                             (harness.home / "other-state", harness.home / "other-state" / "dashboard",
                              Path.home() / "usage.json", "custom.log"))
            os.environ.update(HARNESS_STATE=str(harness.state), USAGE_CACHE=str(harness.home / "usage-cache.json"))
            config = dashboard.DashboardApp(dashboard.DashboardApp.parse_options(base)).config
            self.assertEqual((config.state_dir, config.usage_cache),
                             (harness.state, harness.home / "usage-cache.json"))
            printed = subprocess.run([sys.executable, str(REPO_ROOT / "bin" / "dashboard.py"), *base,
                                      "--print-data-dir"], capture_output=True, text=True, timeout=30)
            self.assertEqual(printed.stdout.strip(), str(harness.state / "dashboard"))

    def test_the_device_probe_runs_as_an_argv_here_and_quoted_over_ssh(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home: Path = Path(directory)
            (home / "state").mkdir()
            argv: tuple[str, ...] = (sys.executable, "-c", "import sys; print(' '.join(sys.argv[1:]))",
                                     "GPU X, 5, 10, 20;", "$(id)")
            config = DashboardConfig(harness_home=home, data_dir=home / "data", repo=None, remote_interval_s=0,
                                     crontab_command=("true",), device_probe_command=argv)
            collector: Collector = Collector(config, None, clock=lambda: FixtureHarness.PINNED_EPOCH)
            self.assertEqual(collector._run_device_probe(), "GPU X, 5, 10, 20; $(id)\n")
            self.assertIn(shlex.join(argv), collector.remote_probe_script)
            self.assertNotIn("sh -c", collector.remote_probe_script)

    def test_the_queue_count_runs_queue_depth_cmd_when_set(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        counter: Path = harness.home / "instance" / "count"
        counter.write_text(f"#!/usr/bin/env bash\necho \"count label=$ISSUE_LABEL\" >> \"{harness.calls_log}\"\n"
                           "echo 5\n")
        counter.chmod(0o755)
        collector: Collector = harness.collector(queue_depth_command=(str(counter),))
        collector.refresh_claimable_in_place()
        self.assertEqual(collector._result("claimable_sh").data, 5)
        self.assertIn("count label=ai:plan", harness.calls())
        self.assertFalse(any(call.startswith("claimable.sh") for call in harness.calls()))

if __name__ == "__main__":
    unittest.main()
