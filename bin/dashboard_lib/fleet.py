"""
Reads fleet's JSON documents (`fleet status --json`, `fleet sessions --json`; docs/FLEET.md, "JSON output") into the
records the Devices and Agent sessions panels show.
"""
import json
import math
import re
from dataclasses import dataclass
from typing import Any, ClassVar


@dataclass(frozen=True)
class FleetGpu:
    """
    One GPU a device reports.
    """

    name: str
    util_pct: float | None
    mem_used_bytes: int | None
    mem_total_bytes: int | None


@dataclass(frozen=True)
class FleetAdapter:
    """
    One agent adapter on a device; `max_concurrent` 0 is discovery-only.
    """

    name: str
    models: tuple[str, ...]
    max_concurrent: int
    running: int


@dataclass(frozen=True)
class FleetDevice:
    """
    One device of the registry: its reading when it answered, else the error the hub got.
    """

    name: str
    transport: str
    error: str | None
    hostname: str = ""
    os: str = ""
    cpus: int | None = None
    load: tuple[float, ...] = ()
    cpu_percent: float | None = None
    mem_used_bytes: int | None = None
    mem_total_bytes: int | None = None
    sampled_at: int | None = None
    gpus: tuple[FleetGpu, ...] = ()
    adapters: tuple[FleetAdapter, ...] = ()
    sessions_truncated: bool = False


@dataclass(frozen=True)
class FleetSession:
    """
    One agent session on a device. `state` is the state's kind, with the exit code, signal or failure after it.
    """

    device: str
    id: str
    adapter: str
    origin: str
    state: str
    running: bool
    pid: int | None
    started_at: int
    ended_at: int | None
    cwd: str | None
    model: str | None
    summary: str
    labels: tuple[tuple[str, str], ...]


@dataclass(frozen=True)
class FleetReport:
    """
    A parsed document. `devices` is None for a sessions document, which reports no device readings; `errors` are
    `<device>: <code>: <message>` for each device that could not be asked.
    """

    generated_at: int
    devices: tuple[FleetDevice, ...] | None
    sessions: tuple[FleetSession, ...]
    errors: tuple[str, ...]
    truncated: tuple[str, ...]


class FleetParser:
    """
    Decodes a fleet document strictly: the version must be OUTPUT_VERSION and every field read must have its
    documented type, or the whole document is refused. Fields the panels do not read are ignored, as the format
    asks of its readers.
    """

    OUTPUT_VERSION: ClassVar[int] = 1
    INT_LIMIT: ClassVar[int] = 2**63
    FLOAT_LIMIT: ClassVar[float] = 1e300
    TRANSPORTS: ClassVar[frozenset[str]] = frozenset({"local", "ssh", "tls"})
    ORIGINS: ClassVar[frozenset[str]] = frozenset({"dispatched", "discovered"})
    STATE_DETAIL: ClassVar[dict[str, tuple[str, type] | None]] = {
        "running": None, "exited": ("code", int), "signalled": ("signal", int), "cancelled": None,
        "timed_out": None, "failed": ("message", str),
    }

    @classmethod
    def parse(cls, text: str) -> FleetReport:
        """
        Args:
            text: The stdout of `fleet status --json` or `fleet sessions --json`.

        Returns:
            The report.

        Raises:
            ValueError: The text is not such a document of OUTPUT_VERSION.
        """
        document: Any = json.loads(text)
        if not isinstance(document, dict):
            raise ValueError("fleet output is not a JSON object")
        version: int = cls._get(document, "version", int, "")
        if version != cls.OUTPUT_VERSION:
            raise ValueError(f"fleet output version {version}, expected {cls.OUTPUT_VERSION}")
        generated_at: int = cls._get(document, "generated_at", int, "")
        if "devices" in document:
            devices: list[FleetDevice] = []
            sessions: list[FleetSession] = []
            for index, entry in enumerate(cls._get(document, "devices", list, "")):
                device, device_sessions = cls._device(cls._object(entry, f"devices[{index}]"), f"devices[{index}]")
                devices.append(device)
                sessions += device_sessions
            return FleetReport(generated_at, tuple(devices), tuple(sessions),
                               tuple(f"{device.name}: {device.error}" for device in devices if device.error),
                               tuple(device.name for device in devices if device.sessions_truncated))
        sessions = []
        for index, entry in enumerate(cls._get(document, "sessions", list, "")):
            path = f"sessions[{index}]"
            item = cls._object(entry, path)
            sessions.append(cls._session(cls._get(item, "device", str, path), cls._get(item, "session", dict, path),
                                         path + ".session"))
        errors: list[str] = []
        for index, entry in enumerate(cls._get(document, "errors", list, "")):
            path = f"errors[{index}]"
            item = cls._object(entry, path)
            errors.append(f"{cls._get(item, 'device', str, path)}: "
                          f"{cls._error(cls._get(item, 'error', dict, path), path + '.error')}")
        truncated: list[Any] = cls._get(document, "truncated", list, "")
        if not all(isinstance(name, str) for name in truncated):
            raise ValueError("fleet output: truncated holds a non-string")
        return FleetReport(generated_at, None, tuple(sessions), tuple(errors), tuple(truncated))

    @classmethod
    def _device(cls, entry: dict[str, Any], path: str) -> tuple[FleetDevice, list[FleetSession]]:
        """
        Args:
            entry: One `devices` entry.
            path: Its place in the document, for errors.

        Returns:
            The device and its sessions.
        """
        name: str = cls._get(entry, "name", str, path)
        transport: str = cls._get(entry, "transport", str, path)
        if transport not in cls.TRANSPORTS:
            raise ValueError(f"fleet output: {path}.transport is {transport!r}")
        sessions: list[FleetSession] = [
            cls._session(name, cls._object(item, f"{path}.sessions[{index}]"), f"{path}.sessions[{index}]")
            for index, item in enumerate(cls._get(entry, "sessions", list, path))
        ]
        truncated: bool = cls._get(entry, "sessions_truncated", bool, path)
        if not cls._get(entry, "ok", bool, path):
            return FleetDevice(name, transport, cls._error(cls._get(entry, "error", dict, path), path + ".error"),
                               sessions_truncated=truncated), sessions
        status: dict[str, Any] = cls._get(entry, "status", dict, path)
        info: dict[str, Any] = cls._get(status, "device", dict, path + ".status")
        metrics: dict[str, Any] = cls._get(status, "metrics", dict, path + ".status")
        memory: dict[str, Any] = cls._get(metrics, "memory", dict, path + ".status.metrics")
        load: list[Any] = cls._get(metrics, "load", list, path + ".status.metrics")
        if len(load) != 3 or not all(cls._is_number(value) for value in load):
            raise ValueError(f"fleet output: {path}.status.metrics.load is not three numbers")
        gpus: list[FleetGpu] = []
        for index, item in enumerate(cls._get(metrics, "gpus", list, path + ".status.metrics")):
            gpu_path: str = f"{path}.status.metrics.gpus[{index}]"
            gpu: dict[str, Any] = cls._object(item, gpu_path)
            gpus.append(FleetGpu(cls._get(gpu, "name", str, gpu_path),
                                 cls._get(gpu, "util_percent", float, gpu_path, nullable=True),
                                 cls._get(gpu, "memory_used_bytes", int, gpu_path, nullable=True),
                                 cls._get(gpu, "memory_total_bytes", int, gpu_path, nullable=True)))
        adapters: list[FleetAdapter] = []
        for index, item in enumerate(cls._get(status, "adapters", list, path + ".status")):
            adapter_path: str = f"{path}.status.adapters[{index}]"
            adapter: dict[str, Any] = cls._object(item, adapter_path)
            models: list[Any] = cls._get(adapter, "models", list, adapter_path)
            if not all(isinstance(model, str) for model in models):
                raise ValueError(f"fleet output: {adapter_path}.models holds a non-string")
            adapters.append(FleetAdapter(cls._get(adapter, "name", str, adapter_path), tuple(models),
                                         cls._get(adapter, "max_concurrent", int, adapter_path),
                                         cls._get(adapter, "running", int, adapter_path)))
        return FleetDevice(
            name, transport, None,
            hostname=cls._get(info, "hostname", str, path + ".status.device"),
            os=cls._get(info, "os", str, path + ".status.device"),
            cpus=cls._get(info, "cpus", int, path + ".status.device"),
            load=tuple(float(value) for value in load),
            cpu_percent=cls._get(metrics, "cpu_percent", float, path + ".status.metrics"),
            mem_used_bytes=cls._get(memory, "used_bytes", int, path + ".status.metrics.memory"),
            mem_total_bytes=cls._get(memory, "total_bytes", int, path + ".status.metrics.memory"),
            sampled_at=cls._get(metrics, "sampled_at", int, path + ".status.metrics"),
            gpus=tuple(gpus), adapters=tuple(adapters), sessions_truncated=truncated,
        ), sessions

    @classmethod
    def _session(cls, device: str, entry: dict[str, Any], path: str) -> FleetSession:
        """
        Args:
            device: The device the session runs on.
            entry: A `Session` object.
            path: Its place in the document, for errors.

        Returns:
            The session.
        """
        origin: str = cls._get(entry, "origin", str, path)
        if origin not in cls.ORIGINS:
            raise ValueError(f"fleet output: {path}.origin is {origin!r}")
        state: dict[str, Any] = cls._get(entry, "state", dict, path)
        kind: str = cls._get(state, "kind", str, path + ".state")
        if kind not in cls.STATE_DETAIL:
            raise ValueError(f"fleet output: {path}.state.kind is {kind!r}")
        detail: tuple[str, type] | None = cls.STATE_DETAIL[kind]
        state_text: str = kind if detail is None else f"{kind} {cls._get(state, detail[0], detail[1], path + '.state')}"
        labels: dict[str, Any] = cls._get(entry, "labels", dict, path)
        if not all(isinstance(value, str) for value in labels.values()):
            raise ValueError(f"fleet output: {path}.labels holds a non-string")
        # The issue label becomes an int; int() refuses a string past 4300 digits.
        if "issue" in labels and not re.fullmatch(r"[0-9]{1,9}", labels["issue"]):
            raise ValueError(f"fleet output: {path}.labels.issue is not an issue number")
        return FleetSession(
            device=device, id=cls._get(entry, "id", str, path), adapter=cls._get(entry, "adapter", str, path),
            origin=origin, state=state_text, running=kind == "running",
            pid=cls._get(entry, "pid", int, path, nullable=True),
            started_at=cls._get(entry, "started_at", int, path),
            ended_at=cls._get(entry, "ended_at", int, path, nullable=True),
            cwd=cls._get(entry, "cwd", str, path, nullable=True),
            model=cls._get(entry, "model", str, path, nullable=True),
            summary=cls._get(entry, "summary", str, path), labels=tuple(sorted(labels.items())),
        )

    @classmethod
    def _error(cls, error: dict[str, Any], path: str) -> str:
        """
        Args:
            error: A `{code, message}` object.
            path: Its place in the document, for errors.

        Returns:
            `<code>: <message>`.
        """
        return f"{cls._get(error, 'code', str, path)}: {cls._get(error, 'message', str, path)}"

    @staticmethod
    def _object(value: Any, path: str) -> dict[str, Any]:
        """
        Args:
            value: A decoded JSON value.
            path: Its place in the document, for errors.

        Returns:
            The value, when it is an object.
        """
        if not isinstance(value, dict):
            raise ValueError(f"fleet output: {path} is not an object")
        return value

    @classmethod
    def _get(cls, entry: Any, key: str, kind: type, path: str, nullable: bool = False) -> Any:
        """
        Args:
            entry: A decoded JSON object.
            key: The field.
            kind: Its type: str, int, float (any JSON number), bool, list or dict.
            path: The object's place in the document, for errors.
            nullable: Whether `null` is allowed.

        Returns:
            The field's value.

        Raises:
            ValueError: The field is missing or of another type.
        """
        where: str = f"{path}.{key}" if path else key
        if not isinstance(entry, dict) or key not in entry:
            raise ValueError(f"fleet output: {where} is missing")
        value: Any = entry[key]
        if value is None and nullable:
            return None
        if kind is float:
            valid: bool = cls._is_number(value)
        elif kind is int:
            valid = isinstance(value, int) and not isinstance(value, bool) and -cls.INT_LIMIT <= value < cls.INT_LIMIT
        else:
            valid = isinstance(value, kind)
        if not valid:
            raise ValueError(f"fleet output: {where} is not a {kind.__name__}")
        return value

    @classmethod
    def _is_number(cls, value: Any) -> bool:
        """
        Args:
            value: A decoded JSON value.

        Returns:
            True for a JSON number in the range fleet reports: an int in 0..2**63-1, or a finite float of magnitude
            at most FLOAT_LIMIT.
        """
        # The range is checked before any math: Python's ints are unbounded, and math.isfinite(10**400) raises
        # OverflowError. JSON has no NaN or Infinity; Python's decoder accepts them, and they would break the
        # page's JSON.
        if isinstance(value, bool):
            return False
        if isinstance(value, int):
            return 0 <= value < cls.INT_LIMIT
        return isinstance(value, float) and math.isfinite(value) and abs(value) <= cls.FLOAT_LIMIT
