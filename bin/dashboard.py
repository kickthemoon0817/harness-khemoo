#!/usr/bin/env python3
"""
Serves the harness status dashboard on 127.0.0.1: machines, GPU slots and leases, live ticks, the issue queue,
landings and health signals, as an auto-refreshing page (`/`) and as JSON (`/api/state`).

    dashboard.py [--harness-home DIR] [--port 8787] [--data-dir DIR] [--queue-label LABEL]
    dashboard.py --once            print one snapshot as JSON and exit

From <harness home>/config/harness.env it reads GH_TOKEN, GH_REPO and the label keys the scripts use, and no other
key; the token reaches `gh` through its environment and is never shown. bin/dashboard.sh starts this detached.
"""
import argparse
import gzip
import hashlib
import hmac
import json
import os
import secrets
import signal
import sys
import threading
import urllib.parse
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from types import FrameType
from typing import Any, ClassVar

sys.path.insert(0, str(Path(__file__).resolve().parent))

from dashboard_lib.collector import Collector, DashboardConfig  # noqa: E402
from dashboard_lib.github import GitHubClient  # noqa: E402
from dashboard_lib.harness_env import HarnessEnvReader  # noqa: E402
from dashboard_lib.queue import QueueLabels  # noqa: E402
from dashboard_lib.render import HtmlRenderer  # noqa: E402


@dataclass(frozen=True)
class Payload:
    """
    Ready-to-send representations, each validated against its exact wire bytes.
    """

    content_type: str
    plain: bytes
    plain_tag: str
    compressed: bytes
    compressed_tag: str

    @classmethod
    def prepare(cls, content_type: str, body: str) -> "Payload":
        """
        Args:
            content_type: Media type and charset.
            body: Scrubbed response text.

        Returns:
            Both encodings with strong validators.
        """
        plain = body.encode("utf-8")
        compressed = gzip.compress(plain, compresslevel=3, mtime=0)
        return cls(content_type, plain, f'"{hashlib.sha256(plain).hexdigest()}"',
                   compressed, f'"{hashlib.sha256(compressed).hexdigest()}"')


class DashboardHttpServer(ThreadingHTTPServer):
    """
    The HTTP server, carrying the collector and renderer its handlers use, the Host names it answers to and the
    access token of this start.
    """

    daemon_threads: bool = True

    def __init__(self, port: int, collector: Collector, renderer: HtmlRenderer, refresh_s: int,
                 secret_values: tuple[str, ...], access_token: str) -> None:
        """
        Args:
            port: The port on 127.0.0.1; 0 picks a free one.
            collector: Builds the snapshots.
            renderer: Renders them as HTML.
            refresh_s: The browser refresh interval.
            secret_values: Strings that must never leave in a response.
            access_token: The token required on every route as `?t=`.
        """
        super().__init__(("127.0.0.1", port), DashboardRequestHandler)
        self.collector: Collector = collector
        self.renderer: HtmlRenderer = renderer
        self.refresh_s: int = max(1, refresh_s)
        self.secret_values: tuple[str, ...] = tuple(value for value in secret_values if value)
        self.access_token: str = access_token
        script = Path(__file__).with_name("dashboard_lib").joinpath("dashboard.js").read_text(encoding="utf-8")
        self.static: dict[str, Payload] = {
            "/dashboard.js": Payload.prepare("text/javascript; charset=utf-8", self.scrub(script)),
            "/healthz": Payload.prepare("text/plain; charset=utf-8", "ok\n"),
        }
        self.routes: dict[str, Payload] = {}
        self.prior_fragments: dict[str, Payload] = {}
        bound_port: int = self.server_address[1]
        self.allowed_hosts: frozenset[str] = frozenset({f"127.0.0.1:{bound_port}", f"localhost:{bound_port}"})

    @property
    def url(self) -> str:
        """
        Returns:
            The page's URL, access token included.
        """
        return f"http://127.0.0.1:{self.server_address[1]}/?t={self.access_token}"

    def publish_in_place(self) -> None:
        """
        Collects and prepares one generation before swapping the handler's route table.
        """
        snapshot = self.collector.snapshot()
        raw = json.dumps(snapshot, separators=(",", ":"), default=str)
        if any(json.dumps(value)[1:-1] in raw for value in self.secret_values):
            snapshot = self._scrub_values(snapshot)
            raw = json.dumps(snapshot, separators=(",", ":"), default=str)
        previous = self.routes
        if "/api/state" in previous and previous["/api/state"].plain == raw.encode():
            return
        sections = {name: self.scrub(fragment) for name, fragment in
                    self.renderer.sections(snapshot, self.access_token).items()}
        routes = dict(self.static)
        versions: dict[str, str] = {}
        markup = "text/html; charset=utf-8"
        json_type = "application/json; charset=utf-8"
        for name, fragment in sections.items():
            digest = hashlib.sha256(fragment.encode()).hexdigest()
            path = "/api/sections/" + digest
            versions[name] = digest
            routes[path] = previous.get(path) or Payload.prepare(markup, fragment)
        page = self.renderer.document(sections, versions, self.refresh_s, self.access_token)
        routes["/"] = routes["/index.html"] = Payload.prepare(markup, self.scrub(page))
        routes["/api/state"] = Payload.prepare(json_type, raw)
        routes["/api/sections"] = Payload.prepare(json_type, json.dumps(versions, separators=(",", ":")))
        # A tab can receive the old index just before publication. Keep its fragments for one interval.
        self.prior_fragments = {path: body for path, body in previous.items() if path.startswith("/api/sections/")}
        self.routes = routes

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

    def _scrub_values(self, value: Any) -> Any:
        """
        Args:
            value: A snapshot value before HTML escaping or JSON serialization.

        Returns:
            The same structure with configured secrets removed from strings.
        """
        if isinstance(value, dict):
            return {self.scrub(key) if isinstance(key, str) else key: self._scrub_values(item)
                    for key, item in value.items()}
        if isinstance(value, (list, tuple)):
            return [self._scrub_values(item) for item in value]
        return self.scrub(value) if isinstance(value, str) else value


class DashboardRequestHandler(BaseHTTPRequestHandler):
    """
    Serves the last published dashboard, its sections, the client script and readiness.

    A request whose Host header is not `127.0.0.1:<port>` or `localhost:<port>` gets 421, so a page that rebinds
    its own name to 127.0.0.1 cannot read the dashboard. Every route requires this start's access token.
    """

    server: DashboardHttpServer
    server_version: ClassVar[str] = "harness-dashboard"
    SECURITY_HEADERS: ClassVar[tuple[tuple[str, str], ...]] = (
        ("Content-Security-Policy", "default-src 'none'; script-src 'self'; connect-src 'self'; "
                                    "style-src 'unsafe-inline'; img-src data:; "
                                    "base-uri 'none'; form-action 'none'; frame-ancestors 'none'"),
        ("Cross-Origin-Resource-Policy", "same-origin"),
        ("X-Content-Type-Options", "nosniff"),
        ("X-Frame-Options", "DENY"),
        ("Referrer-Policy", "no-referrer"),
        ("Cache-Control", "private, no-cache"),
        ("Vary", "Accept-Encoding"),
    )

    def do_GET(self) -> None:
        """
        Serves one GET.
        """
        host: str = (self.headers.get("Host") or "").strip().lower()
        if host not in self.server.allowed_hosts:
            self._error(421, "misdirected request: use 127.0.0.1 or localhost\n")
            return
        parts: urllib.parse.SplitResult = urllib.parse.urlsplit(self.path)
        given_token: str = urllib.parse.parse_qs(parts.query).get("t", [""])[0]
        if not hmac.compare_digest(given_token.encode(), self.server.access_token.encode()):
            self._error(403, "forbidden: open the URL that bin/dashboard.sh status prints\n")
            return
        payload = self.server.routes.get(parts.path) or self.server.prior_fragments.get(parts.path)
        if payload is None:
            self._error(404, "not found\n")
            return
        weights: dict[str, float] = {}
        for entry in ",".join(self.headers.get_all("Accept-Encoding", [])).lower().split(","):
            coding, _, parameter = entry.partition(";")
            quality = parameter.strip().removeprefix("q=")
            try:
                weights[coding.strip()] = float(quality) if parameter else 1.0
            except ValueError:
                weights[coding.strip()] = 0
        gzip_q = weights.get("gzip", weights.get("*", 0.0))
        identity_q = weights.get("identity", 0.0 if weights.get("*") == 0 else 1.0)
        use_gzip = gzip_q > 0 and gzip_q >= weights.get("identity", 0)
        if not use_gzip and identity_q <= 0:
            self._error(406, "no acceptable encoding\n")
            return
        tag = payload.compressed_tag if use_gzip else payload.plain_tag
        body = payload.compressed if use_gzip else payload.plain
        candidates = ",".join(self.headers.get_all("If-None-Match", [])).split(",")
        matched = any(item.strip().removeprefix("W/") in (tag, "*") for item in candidates)
        self.send_response(304 if matched else 200)
        self.send_header("ETag", tag)
        if use_gzip:
            self.send_header("Content-Encoding", "gzip")
        if not matched:
            self.send_header("Content-Type", payload.content_type)
            self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if not matched:
            self.wfile.write(body)

    def end_headers(self) -> None:
        """
        Applies the security and cache headers to successful and failed requests.
        """
        for name, value in self.SECURITY_HEADERS:
            self.send_header(name, value)
        super().end_headers()

    def log_message(self, format: str, *args: object) -> None:
        """
        Logs the request line and status, with the access token in the query masked, and never the headers.

        Args:
            format: The format string.
            *args: Its arguments.
        """
        line: str = format % args
        if self.server.access_token:
            line = line.replace(self.server.access_token, "[token]")
        sys.stderr.write(f"{self.log_date_time_string()} {line}\n")

    def _error(self, status: int, body: str) -> None:
        """
        Sends a response with the secrets scrubbed and the security headers set.

        Args:
            status: HTTP status.
            body: The response text.
        """
        payload: bytes = self.server.scrub(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


class DashboardApp:
    """
    Reads the options and the harness.env keys, then runs the collector and the server until stopped.
    """

    LABEL_KEYS: ClassVar[dict[str, str]] = {
        "ISSUE_LABEL": "queue", "WIP_LABEL": "wip", "SIGNOFF_LABEL": "signoff", "OPERATOR_LABEL": "operator",
        "READY_LABEL": "ready", "FABLE_LABEL": "fable", "FIRST_LABEL": "first", "LOW_LABEL": "low",
    }
    ENV_KEYS: ClassVar[frozenset[str]] = frozenset({"GH_TOKEN", "GH_REPO", *LABEL_KEYS})
    URL_FILE_NAME: ClassVar[str] = "dashboard.url"

    def __init__(self, options: argparse.Namespace) -> None:
        """
        Args:
            options: Parsed command-line options.
        """
        self.options: argparse.Namespace = options
        harness_home: Path = Path(options.harness_home).resolve()
        env_values: dict[str, str] = HarnessEnvReader.read_keys(harness_home / "config" / "harness.env", self.ENV_KEYS)
        self.github_token: str = env_values.get("GH_TOKEN", "")
        repo: str | None = options.repo or env_values.get("GH_REPO") or os.environ.get("GH_REPO") or None
        self.config: DashboardConfig = DashboardConfig(
            harness_home=harness_home,
            data_dir=Path(options.data_dir).resolve() if options.data_dir else harness_home / "state" / "dashboard",
            repo=repo,
            labels=self._labels(env_values, options),
            usage_cache=Path(options.usage_cache).expanduser(),
            gh_binary=options.gh,
            history_hours=options.history_hours,
            github_interval_s=options.github_interval,
            local_interval_s=options.local_interval,
            remote_interval_s=options.remote_interval,
            claimable_interval_s=options.claimable_interval,
            evidence_root=Path(options.evidence_root).expanduser(),
            build_lock_names=self.parse_build_lock_names(options.build_locks),
            build_lock_dir=Path(options.build_lock_dir),
            done_window_s=options.done_hours * 3600,
        )
        github: GitHubClient | None = GitHubClient(repo, self.github_token or None, options.gh) if repo else None
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
            print(body.replace(self.github_token, "[redacted]") if self.github_token else body)
            return 0
        server: DashboardHttpServer = DashboardHttpServer(self.options.port, self.collector, HtmlRenderer(),
                                                          self.options.refresh, (self.github_token,),
                                                          secrets.token_urlsafe(24))
        stop_requested: threading.Event = threading.Event()

        def request_stop(signal_number: int, frame: FrameType | None) -> None:
            stop_requested.set()
            threading.Thread(target=server.shutdown, daemon=True).start()

        signal.signal(signal.SIGTERM, request_stop)
        signal.signal(signal.SIGINT, request_stop)
        self.collector.start(server.publish_in_place, self.options.refresh)
        url_path: Path = self.config.data_dir / self.URL_FILE_NAME
        self.write_private_file(url_path, server.url + "\n")
        sys.stderr.write(f"harness dashboard on {server.url} "
                         f"(home {self.config.harness_home}, data {self.config.data_dir}, "
                         f"repo {self.config.repo or 'unset'}, queue label {self.config.labels.queue})\n")
        sys.stderr.flush()
        try:
            server.serve_forever(poll_interval=0.5)
        finally:
            self.collector.stop()
            server.server_close()
            url_path.unlink(missing_ok=True)
        return 0

    @staticmethod
    def write_private_file(path: Path, text: str) -> None:
        """
        Writes a file only its owner can read, never through a symlink planted at its path.

        Args:
            path: The file.
            text: Its contents.
        """
        descriptor: int = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "w") as private_file:
            private_file.write(text)

    @staticmethod
    def _labels(env_values: dict[str, str], options: argparse.Namespace) -> QueueLabels:
        """
        Takes each label from harness.env where it is set, the scripts' default where not, and the queue label
        from --queue-label when given.

        Args:
            env_values: The keys read from harness.env.
            options: The command-line options.

        Returns:
            The label names.
        """
        overrides: dict[str, str] = {field_name: env_values[key] for key, field_name in DashboardApp.LABEL_KEYS.items()
                                     if env_values.get(key)}
        if options.queue_label:
            overrides["queue"] = options.queue_label
        return QueueLabels(all_issues=options.all_label, **overrides)

    @staticmethod
    def parse_build_lock_names(text: str) -> tuple[tuple[str, str], ...]:
        """
        Args:
            text: `<user@host>=<name> ...`, bin/build_host.sh's BUILD_LOCK_NAMES.

        Returns:
            (host, name) pairs; a word without `=` is left out.
        """
        return tuple((host, name) for host, _, name in (word.partition("=") for word in text.split()) if host and name)

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
        parser.add_argument("--queue-label", default=None,
                            help="the label claimable.sh counts (default: ISSUE_LABEL from config/harness.env, "
                                 "else ai)")
        parser.add_argument("--all-label", default="ai", help="label of every issue the queue panels list")
        parser.add_argument("--usage-cache", default=os.environ.get("USAGE_CACHE", "~/.claude/usage-cache.json"))
        parser.add_argument("--gh", default="gh", help="gh executable")
        parser.add_argument("--refresh", type=int, default=30, help="snapshot and page refresh interval, seconds")
        parser.add_argument("--history-hours", type=float, default=6.0, help="sparkline window")
        parser.add_argument("--github-interval", type=float, default=60.0)
        parser.add_argument("--local-interval", type=float, default=30.0)
        parser.add_argument("--remote-interval", type=float, default=120.0,
                            help="read-only ssh probe of each remote host; 0 turns it off")
        parser.add_argument("--claimable-interval", type=float, default=300.0,
                            help="how often to run bin/claimable.sh; 0 turns it off")
        parser.add_argument("--evidence-root",
                            default=os.environ.get("EVIDENCE_ROOT", "~/tmp_workspace/artifacts/manure-plan/evidence"),
                            help="folder of evidence packets whose runs*/ folders bin/arm_queue.sh writes")
        parser.add_argument("--build-locks",
                            default=os.environ.get("BUILD_LOCK_NAMES",
                                                   "worv@10.50.1.6=5090 worv@worv-worker-03.local=w03"),
                            help="'<user@host>=<name> ...': each remote host's build lock name, as bin/build_host.sh")
        parser.add_argument("--build-lock-dir", default=os.environ.get("BUILD_LOCK_DIR", "/tmp"))
        parser.add_argument("--done-hours", type=float, default=6.0, help="how long a closed issue stays as done")
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
