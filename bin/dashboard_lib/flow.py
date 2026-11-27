"""
Derives the flow diagram: the devices, each issue's state and the device it occupies, laid out for the page.
"""
from dataclasses import dataclass, field
from datetime import datetime
from typing import ClassVar

from dashboard_lib.models import BuildLane, JsonDict, LeaseSlot, RunQueue, TickProcess
from dashboard_lib.queue import QueueIssue, QueueLabels
from dashboard_lib.redact import CommandRedactor


@dataclass(frozen=True)
class Occupancy:
    """
    An issue's hold on, or wait for, one device lane.
    """

    device: str
    lane: str
    kind: str


@dataclass
class FlowCard:
    """
    One issue on the diagram: its column, what it is doing, and the lanes it holds or waits for.
    """

    issue: int
    title: str
    url: str
    column: str
    label: str
    detail: str
    order: tuple[int, str, float]
    chips: list[str] = field(default_factory=list)
    occupancies: list[Occupancy] = field(default_factory=list)


@dataclass(frozen=True)
class FlowInputs:
    """
    Everything the diagram is derived from, as the collector already holds it.
    """

    groups: dict[str, list[QueueIssue]]
    ticks: tuple[TickProcess, ...]
    queues: tuple[RunQueue, ...]
    lanes: tuple[BuildLane, ...]
    lease_slots: tuple[LeaseSlot, ...]
    closed_issues: list[JsonDict]
    machines: list[JsonDict]
    titles: dict[int, tuple[str, str]]
    now: float


class FlowBuilder:
    """
    Places each issue in one state column and draws a line to each device lane it holds or waits for.

    An issue's column is the first that applies: running (a live run queue running or bringing up a job, or a
    held lease slot), waiting for a device (a live run queue whose lease answers end in BUSY), compiling (holding or
    waiting on a build lock), landing (a landing tick, or a tick on a ready issue), claimed (a live tick or claim),
    waiting (claimable, held back by the first-before-low gate, or waiting on a dependency), held (on the operator,
    waiting for sign-off, or a low issue waiting on a dependency), done (closed within the done window). Positions
    are computed here, in pixels on a fixed canvas, so the page can animate a card from where it was. The device
    columns are left out when the instance reports no resources or run queues, and compiling when it names no
    build lock.
    """

    COLUMNS: ClassVar[tuple[tuple[str, str], ...]] = (
        ("waiting", "Waiting"), ("claimed", "Claimed"), ("compiling", "Compiling"),
        ("waiting_device", "Waiting for a device"), ("running", "Running on a device"), ("landing", "Landing"),
        ("held", "Held"), ("done", "Done"),
    )
    DEVICE_COLUMNS: ClassVar[frozenset[str]] = frozenset({"waiting_device", "running"})
    CANVAS_WIDTH: ClassVar[int] = 1240
    GAP: ClassVar[int] = 8
    DEVICE_TOP: ClassVar[int] = 8
    DEVICE_HEIGHT: ClassVar[int] = 136
    COLUMN_TOP: ClassVar[int] = 200
    CARD_TOP: ClassVar[int] = 228
    CARD_HEIGHT: ClassVar[int] = 58
    MAX_CARDS: ClassVar[int] = 7

    def __init__(self, labels: QueueLabels, device_tags: dict[str, str], done_window_s: float,
                 device_lanes: bool, build_lanes: bool) -> None:
        """
        Args:
            labels: The harness's label names.
            device_tags: Remote host to its short tag (`user@host-a` to `a`); `local` is this host.
            done_window_s: How long a closed issue stays in the done column.
            device_lanes: The instance reports resource slots or run queues.
            build_lanes: The instance names build locks.
        """
        self.labels: QueueLabels = labels
        self.device_tags: dict[str, str] = device_tags
        self.done_window_s: float = done_window_s
        self.device_lanes: bool = device_lanes
        self.build_lanes: bool = build_lanes
        self.columns: tuple[tuple[str, str], ...] = tuple(
            (key, title) for key, title in self.COLUMNS
            if (device_lanes or key not in self.DEVICE_COLUMNS) and (build_lanes or key != "compiling"))

    def build(self, inputs: FlowInputs) -> JsonDict:
        """
        Derives and lays out the diagram.

        Args:
            inputs: The collector's current data.

        Returns:
            Devices, columns, cards (with positions), the cards a full column leaves out (`overflow`), edges, the
            lane kinds drawn and the canvas size, every text redacted.
        """
        devices: list[JsonDict] = self._devices(inputs)
        device_positions: dict[str, JsonDict] = {device["key"]: device for device in devices}
        cards: list[FlowCard] = self._cards(inputs)
        column_width: int = (self.CANVAS_WIDTH - (len(self.columns) + 1) * self.GAP) // len(self.columns)
        columns: list[JsonDict] = []
        placed: list[JsonDict] = []
        overflow: list[JsonDict] = []
        edges: list[JsonDict] = []
        lowest: int = self.CARD_TOP
        for index, (key, title) in enumerate(self.columns):
            x: int = self.GAP + index * (column_width + self.GAP)
            members: list[FlowCard] = sorted((card for card in cards if card.column == key),
                                             key=lambda card: card.order)
            shown: list[FlowCard] = members[:self.MAX_CARDS]
            columns.append({"key": key, "title": title, "x": x, "y": self.COLUMN_TOP, "w": column_width,
                            "count": len(members), "hidden": len(members) - len(shown),
                            "more_y": self.CARD_TOP + len(shown) * (self.CARD_HEIGHT + self.GAP)})
            for row, card in enumerate(shown):
                y: int = self.CARD_TOP + row * (self.CARD_HEIGHT + self.GAP)
                lowest = max(lowest, y + self.CARD_HEIGHT)
                placed.append({
                    "issue": card.issue, "title": card.title, "url": card.url, "column": key, "label": card.label,
                    "detail": card.detail, "chips": card.chips, "x": x, "y": y, "w": column_width,
                    "h": self.CARD_HEIGHT,
                })
                for occupancy in card.occupancies:
                    device: JsonDict | None = device_positions.get(occupancy.device)
                    if device is None:
                        continue
                    anchor_x: float = device["x"] + device["w"] * (0.3 if occupancy.lane == "device" else 0.75)
                    edges.append({"issue": card.issue, "lane": occupancy.lane, "kind": occupancy.kind,
                                  "device": occupancy.device, "x1": x + column_width / 2, "y1": y,
                                  "x2": anchor_x, "y2": device["y"] + device["h"]})
            if len(members) > len(shown):
                lowest = max(lowest, self.CARD_TOP + len(shown) * (self.CARD_HEIGHT + self.GAP) + 20)
                overflow.extend({"issue": card.issue, "column": key, "label": card.label}
                                for card in members[len(shown):])
        lanes: list[str] = (["device"] if self.device_lanes else []) + (["compile"] if self.build_lanes else [])
        return {"width": self.CANVAS_WIDTH, "height": lowest + 16, "devices": devices, "columns": columns,
                "cards": placed, "overflow": overflow, "edges": edges, "lanes": lanes}

    def _cards(self, inputs: FlowInputs) -> list[FlowCard]:
        """
        Derives one card an issue that is open in the queue, active on a device, or closed lately.

        Args:
            inputs: The collector's current data.

        Returns:
            The cards, unplaced.
        """
        labels: QueueLabels = self.labels
        queue_issue_by_number: dict[int, QueueIssue] = {
            issue.number: issue for members in inputs.groups.values() for issue in members}
        tick_by_issue: dict[int, TickProcess] = {tick.issue: tick for tick in inputs.ticks if tick.issue is not None}
        live_queues: dict[int, list[RunQueue]] = {}
        for run_queue in inputs.queues:
            if run_queue.alive and run_queue.issue is not None:
                live_queues.setdefault(run_queue.issue, []).append(run_queue)
        held_slots: dict[int, list[LeaseSlot]] = {}
        for slot in inputs.lease_slots:
            if slot.state == "held" and slot.issue is not None:
                held_slots.setdefault(slot.issue, []).append(slot)
        lane_parties: dict[int, list[tuple[BuildLane, int]]] = {}
        for lane in inputs.lanes:
            if lane.holder is not None and lane.holder.issue is not None:
                lane_parties.setdefault(lane.holder.issue, []).append((lane, 0))
            for position, waiter in enumerate(lane.waiters, start=1):
                if waiter.issue is not None:
                    lane_parties.setdefault(waiter.issue, []).append((lane, position))
        closed_by_number: dict[int, JsonDict] = {}
        for closed in inputs.closed_issues:
            closed_epoch: float = self._epoch(closed.get("closedAt"))
            if inputs.now - closed_epoch <= self.done_window_s:
                closed_by_number.setdefault(int(closed["number"]), closed)

        numbers: set[int] = set(tick_by_issue) | set(live_queues) | set(held_slots) | set(lane_parties) | set(
            closed_by_number)
        numbers |= {number for number, issue in queue_issue_by_number.items() if self._on_board(issue)}
        cards: list[FlowCard] = []
        for number in numbers:
            queue_issue: QueueIssue | None = queue_issue_by_number.get(number)
            closed: JsonDict | None = closed_by_number.get(number)
            title, url = inputs.titles.get(number, ("", ""))
            if not title and closed:
                title, url = str(closed.get("title", "")), str(closed.get("url", ""))
            chips: list[str] = [label.removeprefix(labels.all_issues + ":") for label in
                                (queue_issue.labels if queue_issue else ())
                                if label in (labels.first, labels.lane, labels.low, labels.ready)]
            card: FlowCard = FlowCard(issue=number, title=CommandRedactor.redact(title), url=url, column="",
                                      label="", detail="", order=(0, "", float(number)), chips=chips)
            tick: TickProcess | None = tick_by_issue.get(number)
            if tick is not None:
                card.chips.insert(0, tick.model_label)
            self._place(card, queue_issue, tick, live_queues.get(number, []), held_slots.get(number, []),
                        lane_parties.get(number, []), closed, inputs.now)
            if card.column:
                card.label = CommandRedactor.redact(card.label)
                card.detail = CommandRedactor.redact(card.detail)
                cards.append(card)
        return cards

    def _place(self, card: FlowCard, queue_issue: QueueIssue | None, tick: TickProcess | None,
               run_queues: list[RunQueue], slots: list[LeaseSlot], lane_parties: list[tuple[BuildLane, int]],
               closed: JsonDict | None, now: float) -> None:
        """
        Sets a card's column, label, detail, order and lanes.

        Args:
            card: The card, filled in place.
            queue_issue: The issue's queue record, if it is open with the all-issues label.
            tick: The live tick working it, if any.
            run_queues: Its live run queues.
            slots: The lease slots it holds.
            lane_parties: Each build lane it holds (position 0) or waits on (position from 1).
            closed: Its close record, if it closed within the done window.
            now: The current epoch.
        """
        labels: QueueLabels = self.labels
        running: list[RunQueue] = [queue for queue in run_queues if queue.state in ("running", "starting")]
        waiting: list[RunQueue] = [queue for queue in run_queues if queue.state == "waiting_device"]
        for queue in running:
            host: str = queue.job_host or queue.granted_host or "local"
            card.occupancies.append(Occupancy(host, "device", "holds"))
        for slot in slots:
            if not any(occupancy.device == slot.host for occupancy in card.occupancies):
                card.occupancies.append(Occupancy(slot.host, "device", "holds"))
        for queue in waiting:
            if queue.want and queue.want not in ("any", "local"):
                card.occupancies.append(Occupancy(queue.want, "device", "waits"))
        for lane, position in lane_parties:
            card.occupancies.append(Occupancy(lane.host, "compile", "holds" if position == 0 else "waits"))
        reason: str = queue_issue.reason if queue_issue else ""
        issue_labels: tuple[str, ...] = queue_issue.labels if queue_issue else ()

        if running:
            queue = next((queue for queue in running if queue.state == "running"), running[0])
            tag: str = self._tag(queue.job_host or queue.granted_host or "local")
            card.column = "running"
            card.label = f"running {queue.job} on {tag}" if queue.job else f"bringing up on {tag}"
            card.detail = card.label + (f" since {self._clock(queue.job_started)}" if queue.job_started else "")
        elif slots:
            tags: str = " ".join(dict.fromkeys(tag for slot in slots for tag in slot.tags))
            card.column = "running"
            card.label = f"holding {self._tag(slots[0].host)}"
            card.detail = f"{card.label}, slot {', '.join(str(slot.slot) for slot in slots)} {tags}".strip()
        elif waiting:
            queue = waiting[0]
            card.column = "waiting_device"
            card.label = "waiting for a device"
            minutes: int = max(0, int((now - self._epoch(queue.busy_since)) // 60))
            card.detail = f"BUSY {minutes} min: {queue.busy_reason or 'no reason given'}"
        elif lane_parties:
            lane, position = min(lane_parties, key=lambda party: party[1])
            card.column = "compiling"
            if position == 0:
                card.label = f"compiling on {self._tag(lane.host)}"
                card.detail = f"{card.label}, {len(lane.waiters)} waiting"
            else:
                card.label = f"waiting for the {self._tag(lane.host)} compile lane"
                card.detail = f"{position} in line for {self._tag(lane.host)}"
        elif tick is not None and (tick.lane == "landing" or labels.ready in issue_labels):
            card.column = "landing"
            card.label = "landing"
            card.detail = f"{tick.model_label} tick, {self._minutes(tick.age_s)}"
        elif tick is not None or reason.startswith("claimed by live pid"):
            card.column = "claimed"
            card.label = "claimed"
            card.detail = f"{tick.model_label} tick, {self._minutes(tick.age_s)}" if tick else reason
            if tick is not None and tick.doing:
                card.detail += f": {tick.doing}"
        elif queue_issue is not None and queue_issue.group == "claimable":
            card.column = "waiting"
            card.label = "claimable"
            card.detail = reason
            card.order = (0, "", float(card.issue) - (100000 if labels.first in issue_labels else 0))
        elif queue_issue is not None and queue_issue.group == "held":
            card.column = "waiting"
            card.label = "held back by the gate"
            card.detail = reason.removeprefix("held: ")
            card.order = (1, "", float(card.issue))
        elif queue_issue is not None and queue_issue.group == "other" and queue_issue.open_dependencies \
                and labels.low not in issue_labels:
            card.column = "waiting"
            card.label = "waiting on a dependency"
            card.detail = reason
            card.order = (2, "", float(card.issue))
        elif queue_issue is not None and queue_issue.group in ("operator", "signoff"):
            card.column = "held"
            card.label = "on the operator" if queue_issue.group == "operator" else "waiting for sign-off"
            card.detail = card.label
            card.order = (0 if queue_issue.group == "operator" else 1, "", float(card.issue))
        elif queue_issue is not None and labels.low in issue_labels:
            card.column = "held"
            card.label = "low priority"
            card.detail = reason
            card.order = (2, "", float(card.issue))
        elif closed is not None:
            card.column = "done"
            card.label = "done"
            card.detail = f"closed {self._clock(closed.get('closedAt'))}"
            card.order = (0, "", -self._epoch(closed.get("closedAt")))

    def _devices(self, inputs: FlowInputs) -> list[JsonDict]:
        """
        Builds one node a machine: its metrics, lease holders and compile lane when the instance has them, and
        an idle-device flag.

        Args:
            inputs: The collector's current data.

        Returns:
            Device nodes with positions, this host first.
        """
        machines: list[JsonDict] = inputs.machines
        count: int = max(1, len(machines))
        width: int = (self.CANVAS_WIDTH - (count + 1) * self.GAP) // count
        lanes_by_host: dict[str, BuildLane] = {lane.host: lane for lane in inputs.lanes}
        devices: list[JsonDict] = []
        for index, machine in enumerate(machines):
            key: str = machine["key"]
            gpu: JsonDict = machine.get("gpu") or {}
            slots_by_hold: dict[tuple[str, str, str], list[str]] = {}
            for slot in inputs.lease_slots:
                if slot.host == key and slot.state != "free":
                    owner: str = f"#{slot.issue}" if slot.issue is not None else str(slot.holder or "?")
                    hold: tuple[str, str, str] = (owner, " ".join(slot.tags), slot.state)
                    slots_by_hold.setdefault(hold, []).append(str(slot.slot))
            holders: list[str] = [
                f"{owner} {'slots' if len(numbers) > 1 else 'slot'} {', '.join(numbers)} {tags}".strip()
                + (" (orphan)" if state == "orphan" else "")
                for (owner, tags, state), numbers in slots_by_hold.items()
            ]
            lane: BuildLane | None = lanes_by_host.get(key)
            compile_text: str | None = "no compile lane" if self.build_lanes else None
            if lane is not None:
                holder_text: str = (f"#{lane.holder.issue}" if lane.holder and lane.holder.issue is not None
                                    else (lane.holder.tree or f"pid {lane.holder.pid}") if lane.holder else "free")
                compile_text = f"compile: {holder_text}" + (f", {len(lane.waiters)} waiting" if lane.waiters else "")
            queue_waiting: bool = any(queue.alive and queue.state == "waiting_device" and queue.want in (key, "any")
                                      for queue in inputs.queues)
            idle_with_queue: bool = gpu.get("util_pct") == 0 and queue_waiting and key != "local"
            devices.append({
                "key": key,
                "tag": self._tag(key),
                "name": CommandRedactor.redact(str(machine.get("name", key))),
                "gpu_name": CommandRedactor.redact(str(gpu.get("name") or "")),
                "status": machine.get("status"),
                "gpu_util_pct": gpu.get("util_pct"),
                "gpu_mem_used_mb": gpu.get("mem_used_mb"),
                "gpu_mem_total_mb": gpu.get("mem_total_mb"),
                "load1": machine.get("load1"),
                "nproc": machine.get("nproc"),
                "holders": [CommandRedactor.redact(text) for text in holders] if self.device_lanes else None,
                "compile": CommandRedactor.redact(compile_text) if compile_text is not None else None,
                "compile_waiters": len(lane.waiters) if lane else 0,
                "idle_with_queue": idle_with_queue,
                "idle_flag": ("0 % while held, queue waiting" if holders else "device idle, queue waiting")
                if idle_with_queue else "",
                "x": self.GAP + index * (width + self.GAP),
                "y": self.DEVICE_TOP,
                "w": width,
                "h": self.DEVICE_HEIGHT,
            })
        return devices

    def _on_board(self, issue: QueueIssue) -> bool:
        """
        Args:
            issue: An open issue's queue record.

        Returns:
            True for an issue of the harness's queue: anything but one that lacks the queue label and is not low.
        """
        if issue.group != "other":
            return True
        return bool(issue.open_dependencies) and self.labels.queue in issue.labels or issue.reason.startswith(
            "claimed by live pid")

    def _tag(self, host: str) -> str:
        """
        Args:
            host: `local` or a remote host.

        Returns:
            The host's short tag: `local`, its lock name (`gpu-a`), an IP address whole, or a host name's first
            label.
        """
        if host == "local":
            return "local"
        if host in self.device_tags:
            return self.device_tags[host]
        name: str = host.split("@", 1)[-1]
        return name if name.replace(".", "").isdigit() else name.split(".", 1)[0]

    @staticmethod
    def _epoch(iso_text: object) -> float:
        """
        Args:
            iso_text: An ISO-8601 UTC time.

        Returns:
            Epoch seconds, 0 when missing or unreadable.
        """
        if not isinstance(iso_text, str) or not iso_text:
            return 0.0
        try:
            return datetime.fromisoformat(iso_text.replace("Z", "+00:00")).timestamp()
        except ValueError:
            return 0.0

    @staticmethod
    def _clock(iso_text: object) -> str:
        """
        Args:
            iso_text: An ISO-8601 UTC time.

        Returns:
            `HH:MMZ`, or an empty string.
        """
        return f"{iso_text[11:16]}Z" if isinstance(iso_text, str) and len(iso_text) >= 16 else ""

    @staticmethod
    def _minutes(seconds: int | None) -> str:
        """
        Args:
            seconds: An age.

        Returns:
            `<n> min`, or `?`.
        """
        return f"{seconds // 60} min" if seconds is not None else "?"
