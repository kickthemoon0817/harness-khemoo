#!/usr/bin/env python3
"""
Exercises bin/dashboard.py: the state-file parsers, the tick-to-issue mapping, the queue grouping, and the
collector, page and HTTP server against a scratch harness home with a fake lease.sh, claimable.sh and gh.
"""
import contextlib
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

REPO_ROOT: Path = Path(__file__).resolve().parent.parent
FIXTURES: Path = REPO_ROOT / "tests" / "fixtures" / "dashboard"
sys.path.insert(0, str(REPO_ROOT / "bin"))

import dashboard  # noqa: E402
from dashboard_lib.collector import Collector, DashboardConfig  # noqa: E402
from dashboard_lib.github import GitHubClient  # noqa: E402
from dashboard_lib.harness_env import HarnessEnvReader  # noqa: E402
from dashboard_lib.models import ClaimRecord, JsonDict, LeaseSlot  # noqa: E402
from dashboard_lib.parsers import StateParsers  # noqa: E402
from dashboard_lib.procs import ProcessScanner  # noqa: E402
from dashboard_lib.queue import IssueQueue, QueueLabels  # noqa: E402
from dashboard_lib.redact import CommandRedactor  # noqa: E402
from dashboard_lib.render import HtmlRenderer  # noqa: E402


class FixtureHarness:
    """
    A scratch harness home: state files from the fixtures, a fake lease.sh, claimable.sh and gh, a harness.env
    holding a fake token, and a fake tick process.
    """

    FAKE_TOKEN: ClassVar[str] = "ghp_FAKEtokenFAKEtokenFAKEtoken0123456789"
    PINNED_EPOCH: ClassVar[float] = 1795657200.0  # 2026-11-26T01:40:00Z

    def __init__(self) -> None:
        self.temporary_directory: tempfile.TemporaryDirectory[str] = tempfile.TemporaryDirectory()
        self.home: Path = Path(self.temporary_directory.name).resolve() / "harness"
        self.state: Path = self.home / "state"
        self.calls_log: Path = self.home / "calls.log"
        self.tick_process: subprocess.Popen[bytes] | None = None
        self._slot_lock_file: Any = None
        for directory in (self.home / "bin", self.home / "config", self.state / "locks", self.state / "logs",
                          self.state / "model-cooldown"):
            directory.mkdir(parents=True)
        shutil.copy(FIXTURES / "host_health.tsv", self.state / "host_health.tsv")
        shutil.copy(FIXTURES / "host_health.streaks", self.state / "host_health.streaks")
        shutil.copy(FIXTURES / "stalls.log", self.state / "stalls.log")
        shutil.copy(FIXTURES / "allocator.log", self.state / "logs" / "allocator.log")
        shutil.copy(FIXTURES / "usage-cache.json", self.home / "usage-cache.json")
        (self.state / "tick-model").write_text("claude-opus-5-5 claude-fable-5-2\n")
        (self.state / "model-cooldown" / "claude-fable-5-2").touch()
        (self.state / "locks" / "claim.2501").write_text(f"2501 {os.getpid()} 2026-11-26T01:10:00Z\n")
        (self.home / "config" / "harness.env").write_text(textwrap.dedent(f"""\
            # fixture
            TARGET_REPO=/nowhere
            export GH_TOKEN="{self.FAKE_TOKEN}"
            GH_REPO=example/repo   # the target
            ISSUE_LABEL=ai:plan
            FIRST_LABEL=ai:first
            LOW_LABEL="ai:low"
            OTHER_SECRET=do-not-read
        """))
        self._write_script("lease.sh", f"""\
            #!/usr/bin/env bash
            echo "lease.sh $*" >> "{self.calls_log}"
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
        a slot lock open, and one tool shell child wrapping its command as Claude Code does.

        Args:
            issue: The issue its claim file names.
            slot: The slot lock it holds.
            tool_command: The command its tool shell runs; it must block for a while and harm nothing.

        Returns:
            Its pid.
        """
        escaped_command: str = tool_command.replace("'", "'\"'\"'")
        tool_argument: Path = self.home / "tool_argument.txt"
        tool_argument.write_text(f"true && eval '{escaped_command}' < /dev/null && pwd -P >| /dev/null")
        script: Path = self.home / "fake_tick.sh"
        script.write_text(f'bash -c "$(cat \'{tool_argument}\')" &\nwait\n')
        self._slot_lock_file = (self.state / "locks" / f"slot{slot}.lock").open("w")
        log_file = (self.state / "logs" / f"tick-20261126T013000Z-slot{slot}.log").open("w")
        self.tick_process = subprocess.Popen(
            ["bash", "-c", f'exec -a claude bash -p "{script}" "This tick runs on claude-opus-5-5: never claim" '
                           "--model claude-opus-5-5"],
            stdout=log_file, stderr=subprocess.DEVNULL, pass_fds=(self._slot_lock_file.fileno(),),
            start_new_session=True,
        )
        log_file.close()
        pid: int = self.tick_process.pid
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

    def config(self, **overrides: Any) -> DashboardConfig:
        """
        Args:
            **overrides: DashboardConfig fields to change.

        Returns:
            A config reading this home, with its data dir inside it.
        """
        fields: dict[str, Any] = dict(
            harness_home=self.home, data_dir=self.home / "dashboard-data", repo="example/repo",
            labels=QueueLabels(queue="ai:plan"), usage_cache=self.home / "usage-cache.json",
            gh_binary=str(self.home / "bin" / "gh"), remote_interval_s=0,
            crontab_command=(str(self.home / "bin" / "crontab"),),
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
        Stops the fake tick and removes the home.
        """
        if self.tick_process is not None:
            try:
                os.killpg(self.tick_process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            self.tick_process.wait(timeout=5)
        if self._slot_lock_file is not None:
            self._slot_lock_file.close()
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
        self.assertEqual(rows[0].host, "worv@worker-a.local")
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
        self.assertEqual(streaks["worv@10.0.0.6"].state, "demoted")
        self.assertEqual(streaks["worv@10.0.0.6"].bad_probes, 3)
        self.assertEqual(streaks["worv@worker-a.local"].good_probes, 275)

    def test_lease_status(self) -> None:
        status = StateParsers.parse_lease_status((FIXTURES / "lease_status.txt").read_text())
        self.assertEqual([card.host for card in status.remote_cards], ["worv@worker-a.local", "worv@10.0.0.6"])
        self.assertEqual(status.remote_cards[1].first_slot, 6)
        self.assertEqual(status.harness_only_hosts, ("worv@10.0.0.6", "worv@worker-a.local"))
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
        self.assertEqual(by_slot[4].host, "worv@worker-a.local")
        self.assertEqual(by_slot[6].host, "worv@10.0.0.6")
        self.assertIsNotNone(status.device)
        self.assertEqual((status.device.issue, status.device.tags), (2466, ("device",)))
        self.assertEqual(status.operator_priority, ("2490", "2488"))
        self.assertEqual([(wait.kind, wait.issue, wait.minutes, wait.tag) for wait in status.waits],
                         [("whole-card", 2467, 35, "priority"), ("single-slot", 2470, 12, "first in line")])
        self.assertEqual(status.unparsed_lines, ("something unexpected",))

    def test_stalls_keep_the_newest(self) -> None:
        stalls = StateParsers.parse_stalls((FIXTURES / "stalls.log").read_text(), 2)
        self.assertEqual([stall.kind for stall in stalls], ["lock_wait", "gpu_idle"])
        self.assertEqual(stalls[0].key, "/tmp/worv-dev-build-remote-w03.lock")
        self.assertTrue(stalls[1].detail.startswith("card 0 %"))

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
                         [(3557139, "worv-iter-4"), (4100000, None)])
        self.assertEqual([container.name for container in probe.containers], ["worv-iter-4", "isaac_sim_dev"])

    def test_machine_probe_without_card(self) -> None:
        probe = StateParsers.parse_machine_probe("load 0.5 4\n--- gpu\n--- apps\n--- cgroup\n--- docker\n")
        self.assertIsNone(probe.gpu)
        self.assertEqual(probe.gpu_processes, ())

    def test_model_label(self) -> None:
        self.assertEqual(StateParsers.model_label("claude-opus-5-5"), "Opus 5.5")
        self.assertEqual(StateParsers.model_label("claude-fable-5-2"), "Fable 5.2")
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

    def test_plain_values_are_kept(self) -> None:
        for kept in self.KEPT:
            with self.subTest(kept["text"]):
                self.assertIn(kept["kept"], CommandRedactor.redact(kept["text"]))

    def test_summary_names_the_working_command_briefly(self) -> None:
        self.assertEqual(CommandRedactor.summarize(
            "cd /w/mpm-2494 && ls env/nucleus.env 2>/dev/null; DOCKER_HOST=ssh://worv@h timeout 1500 "
            "tools/dev/iter.sh build worv.env.manure > /tmp/p/build1.log 2>&1; echo rc=$?"),
            "timeout 1500 tools/dev/iter.sh build worv.env.manure +1 more")
        self.assertEqual(CommandRedactor.summarize(
            "P=/a/b; for i in $(seq 1 9); do sleep 60; done; grep -E ' rc=' $P/runs/status"), "sleep 60 +1 more")
        self.assertEqual(CommandRedactor.summarize(
            "timeout 560 bash -c \"until grep -q 'RS6 start' $P/runs/status; do sleep 20; done\"; date -u"),
            "timeout 560 bash -c 'grep -q RS6 start \u2026 +1 more' +1 more")
        self.assertEqual(CommandRedactor.summarize("date -u; echo done"), "date -u +1 more")
        self.assertEqual(CommandRedactor.summarize("cat /very/long/path/to/some/evidence/folder/with/a/file.log"),
                         "cat \u2026/file.log")
        self.assertEqual(CommandRedactor.summarize(""), "")
        self.assertEqual(CommandRedactor.summarize_argv(("bash", "/h/bin/arm_queue.sh", "--issue", "2490", "--packet",
                                                         "/p")), "arm_queue.sh --issue 2490 \u2026")
        self.assertEqual(CommandRedactor.summarize_argv(("python3", "/h/bin/x.py", "--token", "abc")),
                         "x.py --token [redacted]")


class SecretSurfaceTest(unittest.TestCase):
    """
    Puts every secret shape where the dashboard looks, a tick's tool call, a lease holder's command line and the
    stall log, and requires none to reach the JSON or the page.
    """

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
        groups = IssueQueue(QueueLabels(queue="ai:plan")).classify(
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
        self.assertEqual(reasons[2504], "held: a new ai:fable round waits while a ai:first issue is claimable")
        last_comment = {issue.number: issue.last_comment_at for members in groups.values() for issue in members}
        self.assertEqual(last_comment[2464], "2026-11-26T01:27:15Z")
        self.assertIsNone(last_comment[2495])

    def test_first_gate(self) -> None:
        def issue(number: int, *labels: str, body: str = "") -> JsonDict:
            return {"number": number, "title": f"t{number}", "body": body,
                    "labels": {"nodes": [{"name": name} for name in ("ai", "ai:plan", *labels)]}}

        def split(issues: list[JsonDict], first_only: list[JsonDict] | None = None) -> tuple[list[int], list[int]]:
            groups = IssueQueue(QueueLabels(queue="ai:plan")).classify(issues, {}, {}, {}, first_only)
            return (sorted(member.number for member in groups["claimable"]),
                    sorted(member.number for member in groups["held"]))

        self.assertEqual(split([issue(1), issue(2, "ai:low")]), ([1], [2]))
        self.assertEqual(split([issue(2, "ai:low"), issue(3, "ai:low")]), ([2, 3], []))
        self.assertEqual(split([issue(4, "ai:fable"), issue(2, "ai:low")]), ([4], [2]))
        self.assertEqual(split([issue(4, "ai:fable"), issue(5, "ai:fable", "ai:first")]), ([5], [4]))
        outside_first = {"number": 9, "title": "t9", "body": "", "labels": {"nodes": [{"name": "ai:first"}]}}
        self.assertEqual(split([issue(4, "ai:fable"), issue(2, "ai:low")], [outside_first]), ([], [2, 4]))
        self.assertEqual(split([issue(4, "ai:fable")], [outside_first]), ([], [4]))
        parked_first = {"number": 9, "title": "t9", "body": "",
                        "labels": {"nodes": [{"name": "ai:first"}, {"name": "ai:operator"}]}}
        self.assertEqual(split([issue(4, "ai:fable")], [parked_first]), ([4], []))
        listed_outside_queue = {"number": 8, "title": "t8", "body": "",
                                "labels": {"nodes": [{"name": "ai"}, {"name": "ai:first"}]}}
        self.assertEqual(split([issue(4, "ai:fable"), listed_outside_queue]), ([], [4]))
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
        ("new round and low", [(4, ("ai", "ai:plan", "ai:fable"), ""), (2, ("ai", "ai:plan", "ai:low"), "")], ()),
        ("new round behind first", [(4, ("ai", "ai:plan", "ai:fable"), ""),
                                    (5, ("ai", "ai:plan", "ai:fable", "ai:first"), "")], ()),
        ("first outside every ai issue", [(4, ("ai", "ai:plan", "ai:fable"), ""), (2, ("ai", "ai:plan", "ai:low"), ""),
                                          (9, ("ai:first",), "")], ()),
        ("parked first outside", [(4, ("ai", "ai:plan", "ai:fable"), ""), (9, ("ai:first", "ai:operator"), "")], ()),
        ("listed first outside the queue", [(4, ("ai", "ai:plan", "ai:fable"), ""), (8, ("ai", "ai:first"), "")], ()),
        ("blocked plain leaves low", [(2, ("ai", "ai:plan", "ai:low"), ""),
                                      (1, ("ai", "ai:plan"), "Depends on: #77")], ()),
        ("met dependency holds low", [(2, ("ai", "ai:plan", "ai:low"), ""),
                                      (1, ("ai", "ai:plan"), "Depends on: #78")], (78,)),
        ("first round and low", [(5, ("ai", "ai:plan", "ai:fable", "ai:first"), ""),
                                 (2, ("ai", "ai:plan", "ai:low"), "")], ()),
        ("blocked first outside", [(4, ("ai", "ai:plan", "ai:fable"), ""),
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
                             PARITY_FIXTURES=str(root / "fx"), GH_REPO="o/r", ISSUE_LABEL="ai:plan"),
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                groups = IssueQueue(QueueLabels(queue="ai:plan")).classify(
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

    def test_every_panel_has_data(self) -> None:
        snapshot = self.snapshot
        self.assertEqual([machine["key"] for machine in snapshot["machines"]],
                         ["local", "worv@worker-a.local", "worv@10.0.0.6"])
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
        self.assertEqual(harness["tick_models"], ["claude-opus-5-5", "claude-fable-5-2"])
        self.assertEqual([model["model"] for model in harness["cooling_models"]], ["claude-fable-5-2"])
        self.assertTrue(harness["work_waiting"])

    def test_calls_are_read_only(self) -> None:
        calls: list[str] = self.harness.calls()
        self.assertTrue(calls)
        for call in calls:
            if call.startswith("lease.sh"):
                self.assertEqual(call, "lease.sh status")
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
        for expected in ("Harness status", '<meta http-equiv="refresh" content="30;url=/', "worv@worker-a.local",
                         "moved last (3 bad probes)", "GPU slots and leases", "whole card", "operator priority",
                         "Running ticks", "#2464", "Opus 5.5", "Claimable", "from this read (ai:plan)",
                         "claimable.sh 2", "held back by the first-before-low gate: 2", "#2503",
                         "Waiting on the operator", "Waiting for sign-off", "#2200", "Landings", "#2499",
                         "Stalls", "lock_wait", "Allocator", "spawn", "usage", "<svg", "polyline",
                         "prefers-color-scheme:dark", "work waiting"):
            self.assertIn(expected, page)
        self.assertNotIn("<script>alert(1)</script>", page)
        self.assertIn("&lt;script&gt;alert(1)&lt;/script&gt;", page)
        self.assertNotIn("<script", page)

    def test_no_token_in_page_or_json(self) -> None:
        token: str = FixtureHarness.FAKE_TOKEN
        self.assertNotIn(token, json.dumps(self.snapshot, default=str))
        self.assertNotIn(token, HtmlRenderer().render(self.snapshot, 30))
        self.assertNotIn(token, repr(self.collector.github))
        self.assertNotIn(token, repr(self.collector.config))

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
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.shutdown)
        return server, server.server_address[1]

    @staticmethod
    def request(port: int, path: str, host: str | None) -> tuple[int, dict[str, str], str]:
        """
        Sends one GET with a chosen Host header, or none.

        Args:
            port: The server's port.
            path: Path and query.
            host: The Host header; None sends none.

        Returns:
            Status, headers and body.
        """
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
        try:
            connection.putrequest("GET", path, skip_host=True)
            if host is not None:
                connection.putheader("Host", host)
            connection.endheaders()
            response = connection.getresponse()
            return response.status, {name.lower(): value for name, value in response.getheaders()}, \
                response.read().decode()
        finally:
            connection.close()

    ACCESS_TOKEN: ClassVar[str] = "test-access-token-0123456789"

    def test_serves_page_json_and_health_on_loopback_without_the_github_token(self) -> None:
        harness: FixtureHarness = FixtureHarness()
        self.addCleanup(harness.close)
        server, port = self.serve(harness)
        self.assertEqual(server.server_address[0], "127.0.0.1")
        self.assertEqual(server.url, f"http://127.0.0.1:{port}/?t={self.ACCESS_TOKEN}")
        host: str = f"127.0.0.1:{port}"
        status, _, body = self.request(port, "/healthz", host)
        self.assertEqual((status, body), (200, "ok\n"))
        status, headers, page = self.request(port, f"/?t={self.ACCESS_TOKEN}", host)
        self.assertEqual((status, headers["content-type"]), (200, "text/html; charset=utf-8"))
        status, _, body = self.request(port, f"/api/state?t={self.ACCESS_TOKEN}", f"localhost:{port}")
        self.assertEqual(status, 200)
        state = json.loads(body)
        status, _, _ = self.request(port, f"/config/harness.env?t={self.ACCESS_TOKEN}", host)
        self.assertEqual(status, 404)
        self.assertIn("Harness status", page)
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
                                            ("/missing", f"127.0.0.1:{port}", 404)):
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
        self.assertEqual((labels.queue, labels.first, labels.low, labels.wip, labels.fable),
                         ("ai:plan", "ai:first", "ai:low", "ai:wip", "ai:fable"))
        overridden = dashboard.DashboardApp(dashboard.DashboardApp.parse_options(base + ["--queue-label", "ai"]))
        self.assertEqual(overridden.config.labels.queue, "ai")
        self.assertEqual(dashboard.DashboardApp.ENV_KEYS, frozenset({
            "GH_TOKEN", "GH_REPO", "ISSUE_LABEL", "WIP_LABEL", "SIGNOFF_LABEL", "OPERATOR_LABEL", "READY_LABEL",
            "FABLE_LABEL", "FIRST_LABEL", "LOW_LABEL"}))

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


if __name__ == "__main__":
    unittest.main()
