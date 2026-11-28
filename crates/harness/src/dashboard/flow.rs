//! Derives the flow diagram: the devices, each issue's state and the device it occupies, laid out
//! for the page.
//!
//! An issue's column is the first that applies: running (a live run queue running or bringing up
//! a job, or a held lease slot), waiting for a device (a live run queue whose lease answers end in
//! BUSY), compiling (holding or waiting on a build lock), landing (a landing tick, or a tick on a
//! ready issue), claimed (a live tick or claim), waiting (claimable, held back by the
//! first-before-low gate, or waiting on a dependency), held (on the operator, waiting for
//! sign-off, or a low issue waiting on a dependency), done (closed within the done window).
//! Positions are computed here, in pixels on a fixed canvas, so the page can animate a card from
//! where it was. The device columns are left out when the instance reports no resources or run
//! queues, and compiling when it names no build lock.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::{Value, json};

use super::github::ClosedNode;
use super::models::{BuildLane, LeaseSlot, RunQueue, TickProcess};
use super::queue::{Groups, QueueIssue, QueueLabels};
use super::redact::redact;
use super::text::parse_iso;

const COLUMNS: [(&str, &str); 8] = [
    ("waiting", "Waiting"),
    ("claimed", "Claimed"),
    ("compiling", "Compiling"),
    ("waiting_device", "Waiting for a device"),
    ("running", "Running on a device"),
    ("landing", "Landing"),
    ("held", "Held"),
    ("done", "Done"),
];
const CANVAS_WIDTH: i64 = 1240;
const GAP: i64 = 8;
const DEVICE_TOP: i64 = 8;
const DEVICE_HEIGHT: i64 = 136;
const COLUMN_TOP: i64 = 200;
const CARD_TOP: i64 = 228;
const CARD_HEIGHT: i64 = 58;
pub const MAX_CARDS: usize = 7;

/// An issue's hold on, or wait for, one device lane.
struct Occupancy {
    device: String,
    lane: &'static str,
    kind: &'static str,
}

/// One issue on the diagram: its column, what it is doing, and the lanes it holds or waits for.
struct FlowCard {
    issue: i64,
    title: String,
    url: String,
    column: &'static str,
    label: String,
    detail: String,
    order: (i64, f64),
    chips: Vec<String>,
    occupancies: Vec<Occupancy>,
}

/// Everything the diagram is derived from, as the collector already holds it.
pub struct FlowInputs<'a> {
    pub groups: &'a Groups,
    pub ticks: &'a [TickProcess],
    pub queues: &'a [RunQueue],
    pub lanes: &'a [BuildLane],
    pub lease_slots: &'a [LeaseSlot],
    pub closed_issues: &'a [ClosedNode],
    pub machines: &'a [Value],
    pub titles: &'a HashMap<i64, (String, String)>,
    pub now: f64,
}

pub struct FlowBuilder {
    pub labels: QueueLabels,
    /// Remote host to its short tag (`user@host-a` to `a`); `local` is this host.
    pub device_tags: HashMap<String, String>,
    pub done_window_s: f64,
    /// The instance reports resource slots or run queues.
    pub device_lanes: bool,
    /// The instance names build locks.
    pub build_lanes: bool,
}

impl FlowBuilder {
    fn columns(&self) -> Vec<(&'static str, &'static str)> {
        COLUMNS
            .into_iter()
            .filter(|(key, _)| {
                (self.device_lanes || !matches!(*key, "waiting_device" | "running"))
                    && (self.build_lanes || *key != "compiling")
            })
            .collect()
    }

    /// Derives and lays out the diagram: devices, columns, cards (with positions), the cards a
    /// full column leaves out (`overflow`), edges, the lane kinds drawn and the canvas size, every
    /// text redacted.
    pub fn build(&self, inputs: &FlowInputs<'_>) -> Value {
        let devices = self.devices(inputs);
        let device_positions: HashMap<String, &Value> = devices
            .iter()
            .filter_map(|device| Some((device["key"].as_str()?.to_owned(), device)))
            .collect();
        let cards = self.cards(inputs);
        let columns = self.columns();
        let count = i64::try_from(columns.len()).unwrap_or(1);
        let column_width = (CANVAS_WIDTH - (count + 1) * GAP) / count;
        let mut column_views = Vec::new();
        let mut placed = Vec::new();
        let mut overflow = Vec::new();
        let mut edges = Vec::new();
        let mut lowest = CARD_TOP;
        for (index, (key, title)) in columns.iter().enumerate() {
            let x = GAP + i64::try_from(index).unwrap_or_default() * (column_width + GAP);
            let mut members: Vec<&FlowCard> =
                cards.iter().filter(|card| card.column == *key).collect();
            members.sort_by(|a, b| {
                a.order
                    .0
                    .cmp(&b.order.0)
                    .then(a.order.1.total_cmp(&b.order.1))
            });
            let shown = &members[..members.len().min(MAX_CARDS)];
            let shown_count = i64::try_from(shown.len()).unwrap_or_default();
            column_views.push(json!({
                "key": key, "title": title, "x": x, "y": COLUMN_TOP, "w": column_width, "count": members.len(),
                "hidden": members.len() - shown.len(), "more_y": CARD_TOP + shown_count * (CARD_HEIGHT + GAP),
            }));
            for (row, card) in shown.iter().enumerate() {
                let y = CARD_TOP + i64::try_from(row).unwrap_or_default() * (CARD_HEIGHT + GAP);
                lowest = lowest.max(y + CARD_HEIGHT);
                placed.push(json!({
                    "issue": card.issue, "title": card.title, "url": card.url, "column": key, "label": card.label,
                    "detail": card.detail, "chips": card.chips, "x": x, "y": y, "w": column_width, "h": CARD_HEIGHT,
                }));
                for occupancy in &card.occupancies {
                    let Some(device) = device_positions.get(&occupancy.device) else {
                        continue;
                    };
                    let device_x = device["x"].as_f64().unwrap_or_default();
                    let device_w = device["w"].as_f64().unwrap_or_default();
                    let anchor_x = device_x
                        + device_w
                            * if occupancy.lane == "device" {
                                0.3
                            } else {
                                0.75
                            };
                    #[allow(clippy::cast_precision_loss)]
                    let x1 = x as f64 + column_width as f64 / 2.0;
                    edges.push(json!({
                        "issue": card.issue, "lane": occupancy.lane, "kind": occupancy.kind, "device": occupancy.device,
                        "x1": x1, "y1": y, "x2": anchor_x,
                        "y2": device["y"].as_i64().unwrap_or_default() + device["h"].as_i64().unwrap_or_default(),
                    }));
                }
            }
            if members.len() > shown.len() {
                lowest = lowest.max(CARD_TOP + shown_count * (CARD_HEIGHT + GAP) + 20);
                overflow.extend(
                    members[shown.len()..].iter().map(
                        |card| json!({"issue": card.issue, "column": key, "label": card.label}),
                    ),
                );
            }
        }
        let mut lanes = Vec::new();
        if self.device_lanes {
            lanes.push("device");
        }
        if self.build_lanes {
            lanes.push("compile");
        }
        json!({
            "width": CANVAS_WIDTH, "height": lowest + 16, "devices": devices, "columns": column_views, "cards": placed,
            "overflow": overflow, "edges": edges, "lanes": lanes,
        })
    }

    /// One card an issue that is open in the queue, active on a device, or closed lately.
    fn cards(&self, inputs: &FlowInputs<'_>) -> Vec<FlowCard> {
        let labels = &self.labels;
        let queue_issue_by_number: HashMap<i64, &QueueIssue> = inputs
            .groups
            .values()
            .flatten()
            .map(|issue| (issue.number, issue))
            .collect();
        let tick_by_issue: HashMap<i64, &TickProcess> = inputs
            .ticks
            .iter()
            .filter_map(|tick| Some((tick.issue?, tick)))
            .collect();
        let mut live_queues: BTreeMap<i64, Vec<&RunQueue>> = BTreeMap::new();
        for queue in inputs.queues {
            if queue.alive
                && let Some(issue) = queue.issue
            {
                live_queues.entry(issue).or_default().push(queue);
            }
        }
        let mut held_slots: BTreeMap<i64, Vec<&LeaseSlot>> = BTreeMap::new();
        for slot in inputs.lease_slots {
            if slot.state == "held"
                && let Some(issue) = slot.issue
            {
                held_slots.entry(issue).or_default().push(slot);
            }
        }
        let mut lane_parties: BTreeMap<i64, Vec<(&BuildLane, usize)>> = BTreeMap::new();
        for lane in inputs.lanes {
            if let Some(issue) = lane.holder.as_ref().and_then(|holder| holder.issue) {
                lane_parties.entry(issue).or_default().push((lane, 0));
            }
            for (position, waiter) in lane.waiters.iter().enumerate() {
                if let Some(issue) = waiter.issue {
                    lane_parties
                        .entry(issue)
                        .or_default()
                        .push((lane, position + 1));
                }
            }
        }
        let mut closed_by_number: BTreeMap<i64, &ClosedNode> = BTreeMap::new();
        for closed in inputs.closed_issues {
            if inputs.now - epoch(closed.closed_at.as_deref()) <= self.done_window_s {
                closed_by_number.entry(closed.number).or_insert(closed);
            }
        }
        let mut numbers: BTreeSet<i64> = tick_by_issue.keys().copied().collect();
        numbers.extend(live_queues.keys());
        numbers.extend(held_slots.keys());
        numbers.extend(lane_parties.keys());
        numbers.extend(closed_by_number.keys());
        numbers.extend(
            queue_issue_by_number
                .iter()
                .filter(|(_, issue)| self.on_board(issue))
                .map(|(number, _)| *number),
        );
        let all_prefix = format!("{}:", labels.all_issues);
        let mut cards = Vec::new();
        for number in numbers {
            let queue_issue = queue_issue_by_number.get(&number).copied();
            let closed = closed_by_number.get(&number).copied();
            let (mut title, mut url) = inputs.titles.get(&number).cloned().unwrap_or_default();
            if title.is_empty()
                && let Some(closed) = closed
            {
                title = closed.title.clone().unwrap_or_default();
                url = closed.url.clone().unwrap_or_default();
            }
            let mut chips: Vec<String> = queue_issue
                .map(|issue| issue.labels.as_slice())
                .unwrap_or_default()
                .iter()
                .filter(|label| {
                    [&labels.first, &labels.lane, &labels.low, &labels.ready].contains(label)
                })
                .map(|label| label.strip_prefix(&all_prefix).unwrap_or(label).to_owned())
                .collect();
            let tick = tick_by_issue.get(&number).copied();
            if let Some(tick) = tick {
                chips.insert(0, tick.model_label.clone());
            }
            #[allow(clippy::cast_precision_loss)]
            let mut card = FlowCard {
                issue: number,
                title: redact(&title),
                url,
                column: "",
                label: String::new(),
                detail: String::new(),
                order: (0, number as f64),
                chips,
                occupancies: Vec::new(),
            };
            self.place(
                &mut card,
                queue_issue,
                tick,
                live_queues
                    .get(&number)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
                held_slots
                    .get(&number)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
                lane_parties
                    .get(&number)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
                closed,
                inputs.now,
            );
            if !card.column.is_empty() {
                card.label = redact(&card.label);
                card.detail = redact(&card.detail);
                cards.push(card);
            }
        }
        cards
    }

    /// Sets a card's column, label, detail, order and lanes.
    #[allow(clippy::too_many_arguments, clippy::assigning_clones)]
    fn place(
        &self,
        card: &mut FlowCard,
        queue_issue: Option<&QueueIssue>,
        tick: Option<&TickProcess>,
        run_queues: &[&RunQueue],
        slots: &[&LeaseSlot],
        lane_parties: &[(&BuildLane, usize)],
        closed: Option<&ClosedNode>,
        now: f64,
    ) {
        let labels = &self.labels;
        let running: Vec<&RunQueue> = run_queues
            .iter()
            .copied()
            .filter(|queue| queue.state == "running" || queue.state == "starting")
            .collect();
        let waiting: Vec<&RunQueue> = run_queues
            .iter()
            .copied()
            .filter(|queue| queue.state == "waiting_device")
            .collect();
        for queue in &running {
            card.occupancies.push(Occupancy {
                device: queue_host(queue).to_owned(),
                lane: "device",
                kind: "holds",
            });
        }
        for slot in slots {
            if !card
                .occupancies
                .iter()
                .any(|occupancy| occupancy.device == slot.host)
            {
                card.occupancies.push(Occupancy {
                    device: slot.host.clone(),
                    lane: "device",
                    kind: "holds",
                });
            }
        }
        for queue in &waiting {
            if let Some(want) = queue
                .want
                .as_deref()
                .filter(|want| *want != "any" && *want != "local")
            {
                card.occupancies.push(Occupancy {
                    device: want.to_owned(),
                    lane: "device",
                    kind: "waits",
                });
            }
        }
        for (lane, position) in lane_parties {
            card.occupancies.push(Occupancy {
                device: lane.host.clone(),
                lane: "compile",
                kind: if *position == 0 { "holds" } else { "waits" },
            });
        }
        let reason = queue_issue.map_or("", |issue| issue.reason.as_str());
        let has = |label: &str| {
            queue_issue.is_some_and(|issue| issue.labels.iter().any(|name| name == label))
        };
        let group = queue_issue.map_or("", |issue| issue.group.as_str());
        #[allow(clippy::cast_precision_loss)]
        let number = card.issue as f64;

        if let Some(first) = running.first() {
            let queue = running
                .iter()
                .find(|queue| queue.state == "running")
                .unwrap_or(first);
            let tag = self.tag(queue_host(queue));
            card.column = "running";
            card.label = match &queue.job {
                Some(job) => format!("running {job} on {tag}"),
                None => format!("bringing up on {tag}"),
            };
            card.detail = match &queue.job_started {
                Some(started) => format!("{} since {}", card.label, clock(started)),
                None => card.label.clone(),
            };
        } else if let Some(first) = slots.first() {
            let mut tags: Vec<&str> = Vec::new();
            for tag in slots.iter().flat_map(|slot| slot.tags.iter()) {
                if !tags.contains(&tag.as_str()) {
                    tags.push(tag);
                }
            }
            let numbers: Vec<String> = slots
                .iter()
                .map(|slot| {
                    slot.slot
                        .map_or_else(|| "None".to_owned(), |slot| slot.to_string())
                })
                .collect();
            card.column = "running";
            card.label = format!("holding {}", self.tag(&first.host));
            card.detail = format!(
                "{}, slot {} {}",
                card.label,
                numbers.join(", "),
                tags.join(" ")
            )
            .trim()
            .to_owned();
        } else if let Some(queue) = waiting.first() {
            card.column = "waiting_device";
            card.label = "waiting for a device".to_owned();
            #[allow(clippy::cast_possible_truncation)]
            let minutes =
                (((now - epoch(queue.busy_since.as_deref())) / 60.0).floor() as i64).max(0);
            card.detail = format!(
                "BUSY {minutes} min: {}",
                queue
                    .busy_reason
                    .as_deref()
                    .filter(|reason| !reason.is_empty())
                    .unwrap_or("no reason given")
            );
        } else if let Some((lane, position)) =
            lane_parties.iter().min_by_key(|(_, position)| *position)
        {
            card.column = "compiling";
            let tag = self.tag(&lane.host);
            if *position == 0 {
                card.label = format!("compiling on {tag}");
                card.detail = format!("{}, {} waiting", card.label, lane.waiters.len());
            } else {
                card.label = format!("waiting for the {tag} compile lane");
                card.detail = format!("{position} in line for {tag}");
            }
        } else if let Some(tick) = tick.filter(|tick| tick.lane == "landing" || has(&labels.ready))
        {
            card.column = "landing";
            card.label = "landing".to_owned();
            card.detail = format!("{} tick, {}", tick.model_label, minutes_text(tick.age_s));
        } else if tick.is_some() || reason.starts_with("claimed by live pid") {
            card.column = "claimed";
            card.label = "claimed".to_owned();
            card.detail = match tick {
                Some(tick) => {
                    let mut detail =
                        format!("{} tick, {}", tick.model_label, minutes_text(tick.age_s));
                    if let Some(doing) = tick.doing.as_deref().filter(|doing| !doing.is_empty()) {
                        detail.push_str(": ");
                        detail.push_str(doing);
                    }
                    detail
                }
                None => reason.to_owned(),
            };
        } else if group == "claimable" {
            card.column = "waiting";
            card.label = "claimable".to_owned();
            card.detail = reason.to_owned();
            card.order = (0, number - if has(&labels.first) { 100_000.0 } else { 0.0 });
        } else if group == "held" {
            card.column = "waiting";
            card.label = "held back by the gate".to_owned();
            card.detail = reason.strip_prefix("held: ").unwrap_or(reason).to_owned();
            card.order = (1, number);
        } else if group == "other"
            && queue_issue.is_some_and(|issue| !issue.open_dependencies.is_empty())
            && !has(&labels.low)
        {
            card.column = "waiting";
            card.label = "waiting on a dependency".to_owned();
            card.detail = reason.to_owned();
            card.order = (2, number);
        } else if group == "operator" || group == "signoff" {
            card.column = "held";
            card.label = if group == "operator" {
                "on the operator"
            } else {
                "waiting for sign-off"
            }
            .to_owned();
            card.detail.clone_from(&card.label);
            card.order = (i64::from(group != "operator"), number);
        } else if queue_issue.is_some() && has(&labels.low) {
            card.column = "held";
            card.label = "low priority".to_owned();
            card.detail = reason.to_owned();
            card.order = (2, number);
        } else if let Some(closed) = closed {
            card.column = "done";
            card.label = "done".to_owned();
            card.detail = format!(
                "closed {}",
                closed.closed_at.as_deref().map(clock).unwrap_or_default()
            );
            card.order = (0, -epoch(closed.closed_at.as_deref()));
        }
    }

    /// One node a machine: its metrics, lease holders and compile lane when the instance has
    /// them, and an idle-device flag; this host first.
    fn devices(&self, inputs: &FlowInputs<'_>) -> Vec<Value> {
        let count = i64::try_from(inputs.machines.len()).unwrap_or(1).max(1);
        let width = (CANVAS_WIDTH - (count + 1) * GAP) / count;
        let lanes_by_host: HashMap<&str, &BuildLane> = inputs
            .lanes
            .iter()
            .map(|lane| (lane.host.as_str(), lane))
            .collect();
        let mut devices = Vec::new();
        for (index, machine) in inputs.machines.iter().enumerate() {
            let key = machine["key"].as_str().unwrap_or_default();
            let empty = Value::Object(serde_json::Map::new());
            let gpu = machine
                .get("gpu")
                .filter(|gpu| gpu.is_object())
                .unwrap_or(&empty);
            let mut holds: Vec<((String, String, String), Vec<String>)> = Vec::new();
            for slot in inputs.lease_slots {
                if slot.host != key || slot.state == "free" {
                    continue;
                }
                let owner = slot.issue.map_or_else(
                    || slot.holder.clone().unwrap_or_else(|| "?".to_owned()),
                    |issue| format!("#{issue}"),
                );
                let hold = (owner, slot.tags.join(" "), slot.state.clone());
                let number = slot
                    .slot
                    .map_or_else(|| "None".to_owned(), |slot| slot.to_string());
                match holds.iter_mut().find(|(existing, _)| *existing == hold) {
                    Some((_, numbers)) => numbers.push(number),
                    None => holds.push((hold, vec![number])),
                }
            }
            let holders: Vec<String> = holds
                .iter()
                .map(|((owner, tags, state), numbers)| {
                    let noun = if numbers.len() > 1 { "slots" } else { "slot" };
                    let mut text = format!("{owner} {noun} {} {tags}", numbers.join(", "))
                        .trim()
                        .to_owned();
                    if state == "orphan" {
                        text.push_str(" (orphan)");
                    }
                    text
                })
                .collect();
            let lane = lanes_by_host.get(key);
            let mut compile_text = self.build_lanes.then(|| "no compile lane".to_owned());
            if let Some(lane) = lane {
                let holder_text = match &lane.holder {
                    Some(holder) => match holder.issue {
                        Some(issue) => format!("#{issue}"),
                        None => holder
                            .tree
                            .clone()
                            .filter(|tree| !tree.is_empty())
                            .unwrap_or_else(|| format!("pid {}", holder.pid)),
                    },
                    None => "free".to_owned(),
                };
                let waiting = if lane.waiters.is_empty() {
                    String::new()
                } else {
                    format!(", {} waiting", lane.waiters.len())
                };
                compile_text = Some(format!("compile: {holder_text}{waiting}"));
            }
            let queue_waiting = inputs.queues.iter().any(|queue| {
                queue.alive
                    && queue.state == "waiting_device"
                    && queue
                        .want
                        .as_deref()
                        .is_some_and(|want| want == key || want == "any")
            });
            let idle_with_queue = gpu.get("util_pct").and_then(Value::as_f64) == Some(0.0)
                && queue_waiting
                && key != "local";
            let name = match machine.get("name") {
                Some(Value::String(name)) => name.clone(),
                Some(Value::Null) | None => key.to_owned(),
                Some(other) => other.to_string(),
            };
            let gpu_name = gpu.get("name").and_then(Value::as_str).unwrap_or_default();
            let idle_flag = match (idle_with_queue, holders.is_empty()) {
                (false, _) => "",
                (true, false) => "0 % while held, queue waiting",
                (true, true) => "device idle, queue waiting",
            };
            devices.push(json!({
                "key": key,
                "tag": self.tag(key),
                "name": redact(&name),
                "gpu_name": redact(gpu_name),
                "status": machine.get("status").cloned().unwrap_or(Value::Null),
                "gpu_util_pct": gpu.get("util_pct").cloned().unwrap_or(Value::Null),
                "gpu_mem_used_mb": gpu.get("mem_used_mb").cloned().unwrap_or(Value::Null),
                "gpu_mem_total_mb": gpu.get("mem_total_mb").cloned().unwrap_or(Value::Null),
                "load1": machine.get("load1").cloned().unwrap_or(Value::Null),
                "nproc": machine.get("nproc").cloned().unwrap_or(Value::Null),
                "holders": self.device_lanes.then(|| holders.iter().map(|text| redact(text)).collect::<Vec<_>>()),
                "compile": compile_text.map(|text| redact(&text)),
                "compile_waiters": lane.map_or(0, |lane| lane.waiters.len()),
                "idle_with_queue": idle_with_queue,
                "idle_flag": idle_flag,
                "x": GAP + i64::try_from(index).unwrap_or_default() * (width + GAP),
                "y": DEVICE_TOP,
                "w": width,
                "h": DEVICE_HEIGHT,
            }));
        }
        devices
    }

    /// True for an issue of the harness's queue: anything but one that lacks the queue label and
    /// is not low.
    fn on_board(&self, issue: &QueueIssue) -> bool {
        if issue.group != "other" {
            return true;
        }
        (!issue.open_dependencies.is_empty() && issue.labels.contains(&self.labels.queue))
            || issue.reason.starts_with("claimed by live pid")
    }

    /// The host's short tag: `local`, its lock name, an IP address whole, or a host name's first
    /// label.
    pub fn tag(&self, host: &str) -> String {
        if host == "local" {
            return "local".to_owned();
        }
        if let Some(tag) = self.device_tags.get(host) {
            return tag.clone();
        }
        let name = host.split_once('@').map_or(host, |(_, name)| name);
        let digits = name.replace('.', "");
        if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
            name.to_owned()
        } else {
            name.split('.').next().unwrap_or(name).to_owned()
        }
    }
}

fn queue_host(queue: &RunQueue) -> &str {
    queue
        .job_host
        .as_deref()
        .or(queue.granted_host.as_deref())
        .filter(|host| !host.is_empty())
        .unwrap_or("local")
}

/// Epoch seconds of an ISO time, 0 when missing or unreadable.
fn epoch(iso_text: Option<&str>) -> f64 {
    iso_text
        .filter(|text| !text.is_empty())
        .and_then(parse_iso)
        .unwrap_or(0.0)
}

/// `HH:MMZ` of an ISO time, or an empty string.
fn clock(iso_text: &str) -> String {
    iso_text
        .get(11..16)
        .map(|clock| format!("{clock}Z"))
        .unwrap_or_default()
}

fn minutes_text(seconds: i64) -> String {
    format!("{} min", seconds.div_euclid(60))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::models::LockParty;
    use crate::dashboard::queue::IssueNode;
    use crate::dashboard::queue::IssueQueue;
    use crate::dashboard::testkit::fixture_text;

    const NOW: f64 = 1_795_657_200.0;

    fn labels() -> QueueLabels {
        QueueLabels {
            queue: "ai:plan".to_owned(),
            lane: "ai:lane".to_owned(),
            ..QueueLabels::default()
        }
    }

    fn builder() -> FlowBuilder {
        FlowBuilder {
            labels: labels(),
            device_tags: [("ci@worker-a.local", "wa"), ("ci@192.0.2.6", "laptop")]
                .map(|(host, tag)| (host.to_owned(), tag.to_owned()))
                .into(),
            done_window_s: 6.0 * 3600.0,
            device_lanes: true,
            build_lanes: true,
        }
    }

    fn tick(
        pid: i64,
        lane: &str,
        age_s: i64,
        slot: i64,
        issue: i64,
        doing: Option<&str>,
    ) -> TickProcess {
        TickProcess {
            pid,
            model: "claude-opus-5-5".to_owned(),
            model_label: "Opus 5.5".to_owned(),
            lane: lane.to_owned(),
            age_s,
            slot: Some(slot),
            log: None,
            issue: Some(issue),
            issue_source: Some("claim".to_owned()),
            doing: doing.map(str::to_owned),
            doing_age_s: doing.map(|_| 5),
            tool_shells: i64::from(doing.is_some()),
        }
    }

    fn queue(
        issue: i64,
        state: &str,
        want: &str,
        job: Option<&str>,
        host: Option<&str>,
        busy: Option<(&str, &str)>,
    ) -> RunQueue {
        RunQueue {
            issue: Some(issue),
            holder_pid: Some(41),
            alive: true,
            state: state.to_owned(),
            want: Some(want.to_owned()),
            job: job.map(str::to_owned),
            job_host: host.map(str::to_owned),
            job_started: job.map(|_| "2026-11-26T01:30:00Z".to_owned()),
            busy_since: busy.map(|(since, _)| since.to_owned()),
            busy_reason: busy.map(|(_, reason)| reason.to_owned()),
            granted_host: host.map(str::to_owned),
            updated_epoch: NOW,
        }
    }

    fn slot(slot: i64, issue: i64, pid: i64, domain: i64, tags: &[&str], host: &str) -> LeaseSlot {
        LeaseSlot {
            slot: Some(slot),
            state: "held".to_owned(),
            holder: Some(issue.to_string()),
            issue: Some(issue),
            pid: Some(pid),
            since: Some("2026-11-26T01:00:00Z".to_owned()),
            domain: Some(domain),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            host: host.to_owned(),
        }
    }

    struct Fixture {
        groups: Groups,
        ticks: Vec<TickProcess>,
        queues: Vec<RunQueue>,
        lanes: Vec<BuildLane>,
        closed: Vec<ClosedNode>,
        machines: Vec<Value>,
        titles: HashMap<i64, (String, String)>,
    }

    fn fixture() -> Fixture {
        let issues: Vec<IssueNode> =
            serde_json::from_str(&fixture_text("issues.json")).expect("fixture");
        let states: BTreeMap<i64, String> = [(2380, "CLOSED"), (2499, "MERGED")]
            .map(|(number, state)| (number, state.to_owned()))
            .into();
        let groups = IssueQueue { labels: labels() }.classify(
            &issues,
            &states,
            &HashMap::new(),
            &HashMap::new(),
            &[],
        );
        Fixture {
            groups,
            ticks: vec![
                tick(11, "standard", 600, 1, 2464, Some("sleep 30")),
                tick(12, "landing", 120, 2, 2201, None),
            ],
            queues: vec![
                queue(
                    2502,
                    "running",
                    "any",
                    Some("RS6-p1"),
                    Some("ci@192.0.2.6"),
                    None,
                ),
                queue(
                    2490,
                    "waiting_device",
                    "ci@192.0.2.6",
                    None,
                    None,
                    Some((
                        "2026-11-26T01:28:00Z",
                        "slot limit GH_TOKEN=ghp_AbCdEfGhIjKlMnOpQrStUvWxYz012345",
                    )),
                ),
            ],
            lanes: vec![BuildLane {
                host: "ci@worker-a.local".to_owned(),
                holder: Some(LockParty {
                    pid: 51,
                    issue: Some(2495),
                    tree: Some("app-2495".to_owned()),
                }),
                waiters: vec![LockParty {
                    pid: 52,
                    issue: Some(2388),
                    tree: Some("app-2388".to_owned()),
                }],
            }],
            closed: serde_json::from_str(&fixture_text("closed.json")).expect("fixture"),
            machines: vec![
                json!({"key": "local", "name": "box", "status": "ok", "gpu": {"name": "RTX", "util_pct": 5.0}, "load1": 1.0, "nproc": 8}),
                json!({"key": "ci@worker-a.local", "name": "ci@worker-a.local", "status": "ok",
                       "gpu": {"util_pct": 40.0, "mem_used_mb": 5000.0, "mem_total_mb": 16000.0}, "load1": 4.0, "nproc": 16}),
                json!({"key": "ci@192.0.2.6", "name": "ci@192.0.2.6", "status": "ok", "gpu": {"util_pct": 0.0}, "load1": 2.0, "nproc": 24}),
            ],
            titles: issues
                .iter()
                .map(|issue| {
                    (
                        issue.number,
                        (
                            issue.title.clone().unwrap_or_default(),
                            issue.url.clone().unwrap_or_default(),
                        ),
                    )
                })
                .collect(),
        }
    }

    fn build(fixture: &Fixture, slots: &[LeaseSlot]) -> Value {
        builder().build(&FlowInputs {
            groups: &fixture.groups,
            ticks: &fixture.ticks,
            queues: &fixture.queues,
            lanes: &fixture.lanes,
            lease_slots: slots,
            closed_issues: &fixture.closed,
            machines: &fixture.machines,
            titles: &fixture.titles,
            now: NOW,
        })
    }

    fn default_slots() -> Vec<LeaseSlot> {
        vec![slot(
            4,
            2488,
            61,
            80,
            &["remote-all", "no-cost"],
            "ci@worker-a.local",
        )]
    }

    fn by_key<'v>(items: &'v Value, key: &str) -> HashMap<String, &'v Value> {
        items
            .as_array()
            .into_iter()
            .flatten()
            .map(|item| (item[key].to_string().trim_matches('"').to_owned(), item))
            .collect()
    }

    #[test]
    fn each_issue_lands_in_its_column() {
        let fixture = fixture();
        let flow = build(&fixture, &default_slots());
        let cards = by_key(&flow["cards"], "issue");
        let expected = [
            (2502, "running", "running RS6-p1 on laptop"),
            (2488, "running", "holding wa"),
            (2490, "waiting_device", "waiting for a device"),
            (2495, "compiling", "compiling on wa"),
            (2388, "compiling", "waiting for the wa compile lane"),
            (2201, "landing", "landing"),
            (2464, "claimed", "claimed"),
            (2504, "waiting", "held back by the gate"),
            (2503, "waiting", "held back by the gate"),
            (2467, "held", "on the operator"),
            (2200, "held", "waiting for sign-off"),
            (2492, "done", "done"),
        ];
        for (issue, column, label) in expected {
            let card = cards[&issue.to_string()];
            assert_eq!(
                (card["column"].as_str(), card["label"].as_str()),
                (Some(column), Some(label)),
                "{issue}"
            );
        }
        assert!(!cards.contains_key("1240"));
        assert!(!cards.contains_key("2310"));
        assert_eq!(cards["2388"]["detail"], "1 in line for wa");
        let detail = cards["2490"]["detail"].as_str().unwrap_or_default();
        assert!(detail.contains("[redacted]"));
        assert!(!flow.to_string().contains("ghp_"));
        assert!(detail.starts_with("BUSY 12 min: slot limit"), "{detail}");
    }

    #[test]
    fn edges_run_to_the_lanes_held_and_waited_for() {
        let fixture = fixture();
        let flow = build(&fixture, &default_slots());
        let mut edges: Vec<(i64, String, String, String)> = flow["edges"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|edge| {
                let text = |key: &str| edge[key].as_str().unwrap_or_default().to_owned();
                (
                    edge["issue"].as_i64().unwrap_or_default(),
                    text("device"),
                    text("lane"),
                    text("kind"),
                )
            })
            .collect();
        edges.sort();
        let expected: Vec<(i64, String, String, String)> = [
            (2388, "ci@worker-a.local", "compile", "waits"),
            (2488, "ci@worker-a.local", "device", "holds"),
            (2490, "ci@192.0.2.6", "device", "waits"),
            (2495, "ci@worker-a.local", "compile", "holds"),
            (2502, "ci@192.0.2.6", "device", "holds"),
        ]
        .map(|(issue, device, lane, kind)| {
            (issue, device.to_owned(), lane.to_owned(), kind.to_owned())
        })
        .to_vec();
        assert_eq!(edges, expected);
        let devices = by_key(&flow["devices"], "key");
        assert_eq!(devices["ci@192.0.2.6"]["idle_with_queue"], true);
        assert_eq!(
            devices["ci@192.0.2.6"]["idle_flag"],
            "device idle, queue waiting"
        );
        let held = build(
            &fixture,
            &[slot(6, 2502, 62, 82, &["remote-all"], "ci@192.0.2.6")],
        );
        assert_eq!(
            by_key(&held["devices"], "key")["ci@192.0.2.6"]["idle_flag"],
            "0 % while held, queue waiting"
        );
        assert_eq!(devices["ci@worker-a.local"]["idle_with_queue"], false);
        assert_eq!(
            devices["ci@worker-a.local"]["holders"],
            json!(["#2488 slot 4 remote-all no-cost"])
        );
        assert_eq!(
            devices["ci@worker-a.local"]["compile"],
            "compile: #2495, 1 waiting"
        );
        for edge in flow["edges"].as_array().into_iter().flatten() {
            let device = devices[edge["device"].as_str().unwrap_or_default()];
            assert_eq!(
                edge["y2"].as_i64(),
                Some(
                    device["y"].as_i64().unwrap_or_default()
                        + device["h"].as_i64().unwrap_or_default()
                )
            );
        }
    }

    #[test]
    fn columns_hold_at_most_their_cap() {
        let fixture = fixture();
        let flow = build(&fixture, &default_slots());
        for column in flow["columns"].as_array().into_iter().flatten() {
            let shown = flow["cards"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|card| card["column"] == column["key"])
                .count();
            let count = column["count"].as_u64().unwrap_or_default();
            let hidden = column["hidden"].as_u64().unwrap_or_default();
            assert_eq!(shown as u64, count - hidden);
            assert!(shown <= MAX_CARDS);
        }
    }
}
