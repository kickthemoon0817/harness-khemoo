//! Remembers each card's last state and position, so the page can animate a move and list the
//! transitions.
//!
//! Each diagram is compared with the one before it. A card whose position changed carries
//! `from_x`/`from_y` (where it was) for `animate_s` seconds after the change was seen, so the next
//! page load slides it from there; a card not shown on the last diagram is marked `fresh`, and so
//! is a line not on it. A card whose label changed adds a transition, newest first, kept to `keep`
//! entries; a card a full column leaves out (`overflow`) still counts, with no position. The last
//! positions and the transitions are saved under the data directory, so a restart neither
//! forgets the trail nor reads every card as new.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{Map, Value, json};

use super::files::{read_text, write_replacing};
use super::text::iso;

const STATE_VERSION: i64 = 1;

#[derive(Default)]
struct Memory {
    cards: BTreeMap<String, Value>,
    edges: BTreeSet<String>,
    transitions: Vec<Value>,
    has_baseline: bool,
}

impl Memory {
    fn set_card(&mut self, key: &str, record: Value) {
        self.cards.insert(key.to_owned(), record);
    }
}

pub struct FlowTracker {
    state_path: PathBuf,
    animate_s: f64,
    keep: usize,
    memory: Mutex<Memory>,
}

fn label_of(record: &Value) -> Value {
    record
        .get("label")
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()))
}

impl FlowTracker {
    pub fn new(state_path: PathBuf, animate_s: f64) -> Self {
        Self {
            state_path,
            animate_s,
            keep: 30,
            memory: Mutex::new(Memory::default()),
        }
    }

    /// Loads the saved state, when there is one.
    pub fn load(&self) {
        let Ok(text) = read_text(&self.state_path) else {
            return;
        };
        let Ok(Value::Object(document)) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        if document.get("version").and_then(Value::as_i64) != Some(STATE_VERSION) {
            return;
        }
        let mut memory = self
            .memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *memory = Memory::default();
        if let Some(Value::Object(cards)) = document.get("cards") {
            for (key, value) in cards {
                memory.set_card(key, value.clone());
            }
        }
        memory.edges = document
            .get("edges")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|edge| edge.as_str().map(str::to_owned))
            .collect();
        memory.transitions = document
            .get("transitions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        memory.transitions.truncate(self.keep);
        memory.has_baseline = true;
    }

    /// Marks moved and fresh cards and lines on a new diagram, and records its transitions; its
    /// cards and edges gain `from_x`, `from_y`, `moved` and `fresh`.
    pub fn update_in_place(&self, flow: &mut Value, now: f64) {
        let mut memory = self
            .memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut changed = false;
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let has_baseline = memory.has_baseline;
        let transition = |issue: &Value, from: Value, to: Value| json!({"time": iso(now), "issue": issue, "from": from, "to": to});
        if let Some(cards) = flow.get_mut("cards").and_then(Value::as_array_mut) {
            for card in cards.iter_mut() {
                let key = card["issue"].to_string();
                seen.insert(key.clone());
                let mut previous = memory.cards.get(&key).cloned();
                let positioned = previous
                    .as_ref()
                    .is_some_and(|previous| !previous["x"].is_null());
                card["fresh"] = Value::Bool(has_baseline && !positioned);
                card["moved"] = Value::Bool(false);
                if let Some(record) = previous.as_mut().filter(|_| positioned) {
                    if (&record["x"], &record["y"]) != (&card["x"], &card["y"]) {
                        let (x, y) = (record["x"].clone(), record["y"].clone());
                        record["from_x"] = x;
                        record["from_y"] = y;
                        record["moved_at"] = json!(now);
                        changed = true;
                    }
                    if let Some(moved_at) = record.get("moved_at").and_then(Value::as_f64)
                        && now - moved_at <= self.animate_s
                    {
                        card["moved"] = Value::Bool(true);
                        card["from_x"] = record["from_x"].clone();
                        card["from_y"] = record["from_y"].clone();
                    }
                }
                if let Some(previous) = &previous
                    && label_of(previous) != card["label"]
                {
                    let entry =
                        transition(&card["issue"], label_of(previous), card["label"].clone());
                    memory.transitions.insert(0, entry);
                    changed = true;
                }
                let mut record = json!({"x": card["x"], "y": card["y"], "label": card["label"]});
                if card["moved"] == Value::Bool(true) {
                    record["from_x"] = card["from_x"].clone();
                    record["from_y"] = card["from_y"].clone();
                    record["moved_at"] = previous
                        .as_ref()
                        .and_then(|previous| previous.get("moved_at"))
                        .cloned()
                        .unwrap_or(Value::Null);
                }
                if memory.cards.get(&key) != Some(&record) {
                    changed = true;
                }
                memory.set_card(&key, record);
            }
        }
        for hidden in flow
            .get("overflow")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            let key = hidden["issue"].to_string();
            seen.insert(key.clone());
            let previous = memory.cards.get(&key).cloned();
            if let Some(previous) = &previous
                && label_of(previous) != hidden["label"]
            {
                let entry = transition(
                    &hidden["issue"],
                    label_of(previous),
                    hidden["label"].clone(),
                );
                memory.transitions.insert(0, entry);
                changed = true;
            }
            let record = json!({"x": null, "y": null, "label": hidden["label"]});
            if previous.as_ref() != Some(&record) {
                changed = true;
            }
            memory.set_card(&key, record);
        }
        // In issue order, so the trail reads the same before and after a restart.
        let mut gone: Vec<String> = memory
            .cards
            .keys()
            .filter(|key| !seen.contains(*key))
            .cloned()
            .collect();
        gone.sort_by_key(|key| (key.parse::<i64>().unwrap_or(i64::MAX), key.clone()));
        for key in gone {
            let label = memory.cards.get(&key).map(label_of).unwrap_or_default();
            let issue = key
                .parse::<i64>()
                .map_or(Value::String(key.clone()), Value::from);
            let entry = transition(&issue, label, Value::String("off the board".to_owned()));
            memory.transitions.insert(0, entry);
            memory.cards.remove(&key);
            changed = true;
        }
        let mut edge_keys = BTreeSet::new();
        if let Some(edges) = flow.get_mut("edges").and_then(Value::as_array_mut) {
            for edge in edges.iter_mut() {
                let text = |key: &str| match &edge[key] {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                let edge_key = format!(
                    "{}|{}|{}|{}",
                    text("issue"),
                    text("device"),
                    text("lane"),
                    text("kind")
                );
                edge["fresh"] = Value::Bool(has_baseline && !memory.edges.contains(&edge_key));
                edge_keys.insert(edge_key);
            }
        }
        if edge_keys != memory.edges {
            memory.edges = edge_keys;
            changed = true;
        }
        let keep = self.keep;
        memory.transitions.truncate(keep);
        if !memory.has_baseline {
            changed = true;
        }
        memory.has_baseline = true;
        flow["transitions"] = Value::Array(memory.transitions.clone());
        if changed {
            self.save(&memory);
        }
    }

    /// Writes the state; a failure only loses the memory across a restart.
    fn save(&self, memory: &Memory) {
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let cards: Map<String, Value> = memory
            .cards
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let document = json!({"version": STATE_VERSION, "cards": cards, "edges": memory.edges, "transitions": memory.transitions});
        write_replacing(&self.state_path, &document.to_string()).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::testkit::TempDir;

    fn flow(cards: &[(i64, i64, i64, &str)]) -> Value {
        json!({
            "cards": cards.iter().map(|(issue, x, y, label)| json!({"issue": issue, "x": x, "y": y, "label": label})).collect::<Vec<_>>(),
            "edges": cards.iter().filter(|(_, _, _, label)| label.starts_with("running"))
                .map(|(issue, _, _, _)| json!({"issue": issue, "device": "d", "lane": "device", "kind": "holds"})).collect::<Vec<_>>(),
        })
    }

    fn card(flow: &Value, issue: i64) -> &Value {
        flow["cards"]
            .as_array()
            .and_then(|cards| cards.iter().find(|card| card["issue"] == issue))
            .unwrap_or(&Value::Null)
    }

    const RUNNING: &str = "running RS6-p1 on laptop";

    #[test]
    fn moves_fresh_cards_transitions_and_restart() {
        let directory = TempDir::new();
        let path = directory.path().join("flow_state.json");
        let tracker = FlowTracker::new(path.clone(), 20.0);
        let mut first = flow(&[(1, 10, 100, "claimed"), (2, 10, 160, "claimed")]);
        tracker.update_in_place(&mut first, 1000.0);
        assert!(
            first["cards"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|card| card["fresh"] == false && card["moved"] == false)
        );
        assert_eq!(first["transitions"], json!([]));

        let mut second = flow(&[
            (1, 300, 100, RUNNING),
            (2, 10, 100, "claimed"),
            (3, 10, 160, "claimed"),
        ]);
        tracker.update_in_place(&mut second, 1030.0);
        assert_eq!(
            (
                &card(&second, 1)["moved"],
                &card(&second, 1)["from_x"],
                &card(&second, 1)["from_y"]
            ),
            (&json!(true), &json!(10), &json!(100))
        );
        assert_eq!(
            (&card(&second, 2)["moved"], &card(&second, 2)["from_y"]),
            (&json!(true), &json!(160))
        );
        assert_eq!(card(&second, 3)["fresh"], true);
        assert_eq!(second["edges"][0]["fresh"], true);
        assert_eq!(second["transitions"][0]["issue"], 1);
        assert_eq!(
            (
                &second["transitions"][0]["from"],
                &second["transitions"][0]["to"]
            ),
            (&json!("claimed"), &json!(RUNNING))
        );

        let mut within = flow(&[
            (1, 300, 100, RUNNING),
            (2, 10, 100, "claimed"),
            (3, 10, 160, "claimed"),
        ]);
        tracker.update_in_place(&mut within, 1045.0);
        assert_eq!(card(&within, 1)["moved"], true);
        let mut later = flow(&[(1, 300, 100, RUNNING), (3, 10, 160, "claimed")]);
        tracker.update_in_place(&mut later, 1060.0);
        assert!(
            later["cards"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|card| card["moved"] == false)
        );
        assert_eq!(
            (
                &later["transitions"][0]["issue"],
                &later["transitions"][0]["to"]
            ),
            (&json!(2), &json!("off the board"))
        );

        let mut crowded = flow(&[(1, 300, 100, RUNNING)]);
        crowded["overflow"] = json!([{"issue": 3, "column": "claimed", "label": "claimed"}]);
        tracker.update_in_place(&mut crowded, 1070.0);
        assert_eq!(crowded["transitions"].as_array().map(Vec::len), Some(2));
        let mut shown_again = flow(&[(1, 300, 100, RUNNING), (3, 10, 160, "claimed")]);
        tracker.update_in_place(&mut shown_again, 1080.0);
        assert_eq!(card(&shown_again, 3)["fresh"], true);
        assert_eq!(shown_again["transitions"].as_array().map(Vec::len), Some(2));

        let restarted = FlowTracker::new(path, 20.0);
        restarted.load();
        let mut again = flow(&[(1, 300, 100, RUNNING), (3, 10, 160, "claimed")]);
        restarted.update_in_place(&mut again, 1100.0);
        assert!(
            again["cards"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|card| card["fresh"] == false)
        );
        assert_eq!(again["transitions"].as_array().map(Vec::len), Some(2));
    }
}
