"""
Remembers each card's last state and position, so the page can animate a move and list the transitions.
"""
import json
import threading
from datetime import datetime, timezone
from pathlib import Path
from typing import ClassVar

from dashboard_lib.models import JsonDict


class FlowTracker:
    """
    Compares each diagram with the one before it.

    A card whose position changed carries `from_x`/`from_y` (where it was) for `animate_s` seconds after the
    change was seen, so the next page load slides it from there; a card not shown on the last diagram is marked
    `fresh`, and so is a line not on it. A card whose label changed adds a transition, newest first, kept to
    `keep` entries; a card a full column leaves out (`overflow`) still counts, with no position. The last positions and the transitions are saved under the data directory, so a restart
    neither forgets the trail nor reads every card as new.
    """

    STATE_VERSION: ClassVar[int] = 1

    def __init__(self, state_path: Path, animate_s: float, keep: int = 30) -> None:
        """
        Args:
            state_path: The file the tracker keeps its state in.
            animate_s: How long after a move a card still carries where it came from.
            keep: Transitions kept.
        """
        self.state_path: Path = state_path
        self.animate_s: float = animate_s
        self.keep: int = keep
        self._lock: threading.Lock = threading.Lock()
        self._cards: dict[str, JsonDict] = {}
        self._edges: set[str] = set()
        self._transitions: list[JsonDict] = []
        self._has_baseline: bool = False

    def load(self) -> None:
        """
        Loads the saved state, when there is one.
        """
        try:
            document: JsonDict = json.loads(self.state_path.read_text())
        except (OSError, ValueError):
            return
        if document.get("version") != self.STATE_VERSION:
            return
        with self._lock:
            self._cards = {str(key): value for key, value in (document.get("cards") or {}).items()}
            self._edges = set(document.get("edges") or [])
            self._transitions = list(document.get("transitions") or [])[: self.keep]
            self._has_baseline = True

    def update_in_place(self, flow: JsonDict, now: float) -> None:
        """
        Marks moved and fresh cards and lines on a new diagram, and records its transitions.

        Args:
            flow: The diagram from FlowBuilder; its cards and edges gain `from_x`, `from_y`, `moved`, `fresh`.
            now: The current epoch.
        """
        with self._lock:
            changed: bool = False
            seen: set[str] = set()
            for card in flow["cards"]:
                key: str = str(card["issue"])
                seen.add(key)
                previous: JsonDict | None = self._cards.get(key)
                card["fresh"] = self._has_baseline and (previous is None or previous.get("x") is None)
                card["moved"] = False
                if previous is not None and previous.get("x") is not None:
                    if (previous["x"], previous["y"]) != (card["x"], card["y"]):
                        previous = dict(previous, from_x=previous["x"], from_y=previous["y"], moved_at=now)
                        changed = True
                    if previous.get("moved_at") is not None and now - previous["moved_at"] <= self.animate_s:
                        card.update(moved=True, from_x=previous["from_x"], from_y=previous["from_y"])
                if previous is not None and previous.get("label") != card["label"]:
                    self._transitions.insert(0, {"time": self._iso(now), "issue": card["issue"],
                                                 "from": previous.get("label", ""), "to": card["label"]})
                    changed = True
                record: JsonDict = {"x": card["x"], "y": card["y"], "label": card["label"]}
                if card["moved"]:
                    record.update(from_x=card["from_x"], from_y=card["from_y"],
                                  moved_at=(previous or {}).get("moved_at"))
                if self._cards.get(key) != record:
                    changed = True
                self._cards[key] = record
            for hidden in flow.get("overflow", []):
                key = str(hidden["issue"])
                seen.add(key)
                previous = self._cards.get(key)
                if previous is not None and previous.get("label") != hidden["label"]:
                    self._transitions.insert(0, {"time": self._iso(now), "issue": hidden["issue"],
                                                 "from": previous.get("label", ""), "to": hidden["label"]})
                    changed = True
                hidden_record: JsonDict = {"x": None, "y": None, "label": hidden["label"]}
                if previous != hidden_record:
                    changed = True
                self._cards[key] = hidden_record
            for key in [key for key in self._cards if key not in seen]:
                self._transitions.insert(0, {"time": self._iso(now), "issue": int(key),
                                             "from": self._cards[key].get("label", ""), "to": "off the board"})
                del self._cards[key]
                changed = True
            edge_keys: set[str] = set()
            for edge in flow["edges"]:
                edge_key: str = f"{edge['issue']}|{edge['device']}|{edge['lane']}|{edge['kind']}"
                edge["fresh"] = self._has_baseline and edge_key not in self._edges
                edge_keys.add(edge_key)
            if edge_keys != self._edges:
                self._edges = edge_keys
                changed = True
            del self._transitions[self.keep:]
            if not self._has_baseline:
                changed = True
            self._has_baseline = True
            flow["transitions"] = [dict(transition) for transition in self._transitions]
            if changed:
                self._save()

    def _save(self) -> None:
        """
        Writes the state atomically; a failure only loses the memory across a restart.
        """
        try:
            self.state_path.parent.mkdir(parents=True, exist_ok=True)
            temporary_path: Path = self.state_path.with_suffix(".json.new")
            temporary_path.write_text(json.dumps({"version": self.STATE_VERSION, "cards": self._cards,
                                                  "edges": sorted(self._edges), "transitions": self._transitions}))
            temporary_path.replace(self.state_path)
        except OSError:
            return

    @staticmethod
    def _iso(epoch: float) -> str:
        """
        Args:
            epoch: Epoch seconds.

        Returns:
            ISO-8601 UTC to the second.
        """
        return datetime.fromtimestamp(epoch, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
