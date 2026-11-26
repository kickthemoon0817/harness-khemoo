#!/usr/bin/env python3
"""
Serves the harness status dashboard on 127.0.0.1: machines, GPU slots and leases, live ticks, the issue queue,
landings and health signals, as an auto-refreshing page (`/`) and as JSON (`/api/state`).

    dashboard.py [--harness-home DIR] [--port 8787] [--data-dir DIR] [--queue-label ai:plan]
    dashboard.py --once            print one snapshot as JSON and exit

GH_TOKEN and GH_REPO are read from <harness home>/config/harness.env (only those two keys); the token reaches
`gh` through its environment and is never shown. bin/dashboard.sh starts this detached.
"""
import argparse
import json
import os
import signal
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from types import FrameType
from typing import ClassVar

sys.path.insert(0, str(Path(__file__).resolve().parent))

from dashboard_lib.collector import Collector, DashboardConfig  # noqa: E402
from dashboard_lib.github import GitHubClient  # noqa: E402
from dashboard_lib.harness_env import HarnessEnvReader  # noqa: E402
from dashboard_lib.models import JsonDict  # noqa: E402
from dashboard_lib.queue import QueueLabels  # noqa: E402
from dashboard_lib.render import HtmlRenderer  # noqa: E402


class DashboardHttpServer(ThreadingHTTPServer):
    """
    The HTTP server, carrying the collector and renderer its handlers use.
    """

    daemon_threads: bool = True

    def __init__(self, port: int, collector: Collector, renderer: HtmlRenderer, refresh_s: int,
                 secret_values: tuple[str, ...]) -> None:
        """
        Args:
            port: The port on 127.0.0.1.
            collector: Builds the snapshots.
            renderer: Renders them as HTML.
            refresh_s: The page's reload interval.
            secret_values: Strings that must never leave in a response.
        """
        super().__init__(("127.0.0.1", port), DashboardRequestHandler)
        self.collector: Collector = collector
        self.renderer: HtmlRenderer = renderer
        self.refresh_s: int = refresh_s
        self.secret_values: tuple[str, ...] = tuple(value for value in secret_values if value)

    def scrub(self, body: str) -> str:
        """
        Removes every secret value from a response body.

        Args:
            body: The response text.

        Returns:
            The text with secrets replaced.
        """
        for value in self.secret_values:
            body = body.replace(value, "[redacted]")
        return body


class DashboardRequestHandler(BaseHTTPRequestHandler):
    """
    Answers `/` (HTML), `/api/state` (JSON) and `/healthz`; everything else is 404.
    """

    server: DashboardHttpServer
    server_version: ClassVar[str] = "harness-dashboard"

    def do_GET(self) -> None:
        """
        Serves one GET.
        """
        path: str = self.path.split("?", 1)[0]
        if path == "/healthz":
            self._send(200, "text/plain; charset=utf-8", "ok\n")
            return
        if path not in ("/", "/index.html", "/api/state"):
            self._send(404, "text/plain; charset=utf-8", "not found\n")
            return
        snapshot: JsonDict = self.server.collector.snapshot()
        if path == "/api/state":
            self._send(200, "application/json; charset=utf-8", json.dumps(snapshot, indent=1, default=str))
        else:
            self._send(200, "text/html; charset=utf-8", self.server.renderer.render(snapshot, self.server.refresh_s))

    def log_message(self, format: str, *args: object) -> None:
        """
        Logs only the request line and status, never headers.

        Args:
            format: The format string.
            *args: Its arguments.
        """
        sys.stderr.write(f"{self.log_date_time_string()} {format % args}\n")

    def _send(self, status: int, content_type: str, body: str) -> None:
        """
        Sends a response with the secrets scrubbed.

        Args:
            status: HTTP status.
            content_type: The Content-Type.
            body: The response text.
        """
        payload: bytes = self.server.scrub(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(payload)


class DashboardApp:
    """
    Reads the options and the two GitHub keys, then runs the collector and the server until stopped.
    """

    ENV_KEYS: ClassVar[frozenset[str]] = frozenset({"GH_TOKEN", "GH_REPO"})

    def __init__(self, options: argparse.Namespace) -> None:
        """
        Args:
            options: Parsed command-line options.
        """
        self.options: argparse.Namespace = options
        harness_home: Path = Path(options.harness_home).resolve()
        env_values: dict[str, str] = HarnessEnvReader.read_keys(harness_home / "config" / "harness.env", self.ENV_KEYS)
        self.token: str = env_values.get("GH_TOKEN", "")
        repo: str | None = options.repo or env_values.get("GH_REPO") or os.environ.get("GH_REPO") or None
        self.config: DashboardConfig = DashboardConfig(
            harness_home=harness_home,
            data_dir=Path(options.data_dir).resolve() if options.data_dir else harness_home / "state" / "dashboard",
            repo=repo,
            labels=QueueLabels(all_issues=options.all_label, queue=options.queue_label),
            usage_cache=Path(options.usage_cache).expanduser(),
            gh_binary=options.gh,
            history_hours=options.history_hours,
            github_interval_s=options.github_interval,
            local_interval_s=options.local_interval,
            remote_interval_s=options.remote_interval,
            claimable_interval_s=options.claimable_interval,
        )
        github: GitHubClient | None = GitHubClient(repo, self.token or None, options.gh) if repo else None
        self.collector: Collector = Collector(self.config, github)

    def run(self) -> int:
        """
        Prints one snapshot (`--once`) or serves until SIGINT or SIGTERM.

        Returns:
            The exit status.
        """
        if self.options.once:
            self.collector.config.data_dir.mkdir(parents=True, exist_ok=True)
            self.collector.refresh_local_sample_in_place()
            if self.config.remote_interval_s > 0:
                self.collector.refresh_remote_probes_in_place()
            self.collector.refresh_github_in_place()
            if self.config.claimable_interval_s > 0:
                self.collector.refresh_claimable_in_place()
            body: str = json.dumps(self.collector.snapshot(), indent=1, default=str)
            print(body.replace(self.token, "[redacted]") if self.token else body)
            return 0
        server: DashboardHttpServer = DashboardHttpServer(self.options.port, self.collector, HtmlRenderer(),
                                                          self.options.refresh, (self.token,))
        stop_requested: threading.Event = threading.Event()

        def request_stop(signal_number: int, frame: FrameType | None) -> None:
            stop_requested.set()
            threading.Thread(target=server.shutdown, daemon=True).start()

        signal.signal(signal.SIGTERM, request_stop)
        signal.signal(signal.SIGINT, request_stop)
        self.collector.start()
        sys.stderr.write(f"harness dashboard on http://127.0.0.1:{server.server_address[1]}/ "
                         f"(home {self.config.harness_home}, data {self.config.data_dir}, "
                         f"repo {self.config.repo or 'unset'}, queue label {self.config.labels.queue})\n")
        sys.stderr.flush()
        try:
            server.serve_forever(poll_interval=0.5)
        finally:
            self.collector.stop()
            server.server_close()
        return 0

    @staticmethod
    def parse_options(arguments: list[str]) -> argparse.Namespace:
        """
        Args:
            arguments: The command line after the program name.

        Returns:
            The options.
        """
        default_home: str = os.environ.get("HARNESS_HOME") or str(Path(__file__).resolve().parent.parent)
        parser: argparse.ArgumentParser = argparse.ArgumentParser(
            description="Harness status dashboard on 127.0.0.1.")
        parser.add_argument("--harness-home", default=default_home,
                            help="harness checkout to read (default: $HARNESS_HOME, else this script's repo)")
        parser.add_argument("--port", type=int, default=int(os.environ.get("DASHBOARD_PORT", "8787")))
        parser.add_argument("--data-dir", default=os.environ.get("DASHBOARD_DATA_DIR"),
                            help="the dashboard's own history and cache (default: <home>/state/dashboard)")
        parser.add_argument("--repo", default=None, help="owner/name; default GH_REPO from config/harness.env")
        parser.add_argument("--queue-label", default=os.environ.get("ISSUE_LABEL", "ai"),
                            help="the label claimable.sh counts (the harness's ISSUE_LABEL; default $ISSUE_LABEL "
                                 "or ai)")
        parser.add_argument("--all-label", default="ai", help="label of every issue the queue panels list")
        parser.add_argument("--usage-cache", default=os.environ.get("USAGE_CACHE", "~/.claude/usage-cache.json"))
        parser.add_argument("--gh", default="gh", help="gh executable")
        parser.add_argument("--refresh", type=int, default=30, help="page reload interval, seconds")
        parser.add_argument("--history-hours", type=float, default=6.0, help="sparkline window")
        parser.add_argument("--github-interval", type=float, default=60.0)
        parser.add_argument("--local-interval", type=float, default=30.0)
        parser.add_argument("--remote-interval", type=float, default=120.0,
                            help="read-only ssh probe of each remote host; 0 turns it off")
        parser.add_argument("--claimable-interval", type=float, default=300.0,
                            help="how often to run bin/claimable.sh; 0 turns it off")
        parser.add_argument("--once", action="store_true", help="print one JSON snapshot and exit")
        return parser.parse_args(arguments)

    @staticmethod
    def main() -> None:
        """
        Runs the dashboard from the command line.
        """
        sys.exit(DashboardApp(DashboardApp.parse_options(sys.argv[1:])).run())


if __name__ == "__main__":
    DashboardApp.main()
