//! Pure parsers for the harness state files and command outputs the dashboard reads.
//!
//! Every parser takes text and returns records; none reads a file. A line that does not match its
//! format is skipped (or, for the resource status, kept as an unparsed line) so one malformed row
//! never hides the rest.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use regex::Regex;

use super::models::{
    AgentUsageLine, AllocatorLine, ClaimRecord, ContainerInfo, DeviceProcess, DeviceReading,
    HostHealthRow, HostStreak, LeaseSlot, LeaseStatus, LeaseWait, MachineProbe, RemoteCard,
    StallLine, UsageWindowLine,
};
use super::text::{is_word_char, optional_float, optional_int, parse_iso, split_lines};

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a constant pattern compiles")
}

const HOST_HEALTH_COLUMNS: usize = 9;
static REMOTE_CARD: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^remote (?:device|card|host): (\S+) \(slots ([0-9]{1,9})-([0-9]{1,9})\)$")
});
static LOCAL_OFF: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^local (?:device|card|host): off(?:$|[^\p{L}\p{N}_])"));
static HARNESS_ONLY: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^harness ticks: (.*?) only(?:$|[^\p{L}\p{N}_])"));
static SLOT_FREE: LazyLock<Regex> = LazyLock::new(|| regex(r"^slot ([0-9]{1,9}) free$"));
static SLOT_HELD: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^(?:slot ([0-9]{1,9})|device) (HELD|ORPHAN) (\S+) ([0-9]{1,9}) (\S+)((?: \S+)*?) \((alive|pid dead)\)$",
    )
});
static WAIT: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^wait (whole-device|whole-card|whole-pool|single-slot) issue ([0-9]{1,9}) for (-?[0-9]{1,9}) min(?: \((.*)\))?$",
    )
});
static STALL: LazyLock<Regex> = LazyLock::new(|| regex(r"^(\S+) STALL (\S+) (\S+) ?(.*)$"));
static ALLOCATOR: LazyLock<Regex> = LazyLock::new(|| regex(r"^([0-9]{8}T[0-9]{6}Z) (.*)$"));
static ALLOCATOR_FIELD: LazyLock<Regex> = LazyLock::new(|| regex(r"([a-z_]+)=([^\s%]+)%?"));
static USAGE_PART: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?:^|\s)([a-z0-9_]{1,32})=([0-9]{1,9})\[([^\]\s]*)\]"));
static ALLOCATOR_SLOT: LazyLock<Regex> = LazyLock::new(|| regex(r"^(slot[0-9]+) "));
static SPAWNING: LazyLock<Regex> = LazyLock::new(|| regex(r"-> spawning ([0-9]+)"));
static MODEL: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^claude-([a-z]+)-([0-9]+)(?:-([0-9]{1,2}))?(?:-[0-9]{8})?$"));
static TOOL_EVAL: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?s)(?:^|[^\p{L}\p{N}_])eval '(.*)' < /dev/null"));
static DOCKER_CGROUP: LazyLock<Regex> = LazyLock::new(|| regex(r"docker[-/]([0-9a-f]{12})"));
static PACKET_ISSUE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?:^|/)[0-9]{4}-[0-9]{2}-[0-9]{2}-([0-9]{1,9})-[^/]*(?:/|$)"));
static CLAIM_NAME: LazyLock<Regex> = LazyLock::new(|| regex(r"^claim\.([0-9]{1,9})$"));
static MEMINFO_KB: LazyLock<Regex> = LazyLock::new(|| regex(r"^[0-9]{1,15}$"));

fn group<'t>(captures: &regex::Captures<'t>, index: usize) -> Option<&'t str> {
    captures.get(index).map(|found| found.as_str())
}

fn number(captures: &regex::Captures<'_>, index: usize) -> i64 {
    group(captures, index)
        .and_then(|text| text.parse().ok())
        .unwrap_or_default()
}

/// Parses `state/host_health.tsv`, header included: one row a well-formed line, in file order.
pub fn parse_host_health_tsv(text: &str) -> Vec<HostHealthRow> {
    let mut rows = Vec::new();
    for line in split_lines(text) {
        let columns: Vec<&str> = line.split('\t').collect();
        if columns.len() != HOST_HEALTH_COLUMNS || columns[0] == "time" {
            continue;
        }
        let Some(epoch) = parse_iso(columns[0]) else {
            continue;
        };
        rows.push(HostHealthRow {
            time: columns[0].to_owned(),
            epoch,
            host: columns[1].to_owned(),
            connect_s: optional_float(columns[2]),
            upload_mb_s: optional_float(columns[3]),
            upload_timed_out: columns[3] == "timeout",
            load1: optional_float(columns[4]),
            nproc: optional_int(columns[5]),
            gpu_util_pct: optional_float(columns[6]),
            gpu_mem_used_mb: optional_float(columns[7]),
            status: columns[8].trim().to_owned(),
        });
    }
    rows
}

/// Parses `state/host_health.streaks`: `<host> <ok|demoted> <bad> <good> <since>` a line.
pub fn parse_streaks(text: &str) -> HashMap<String, HostStreak> {
    let mut streaks = HashMap::new();
    for line in split_lines(text) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 5 || line.starts_with('#') {
            continue;
        }
        let (Some(bad), Some(good), Some(since)) = (
            optional_int(parts[2]),
            optional_int(parts[3]),
            optional_int(parts[4]),
        ) else {
            continue;
        };
        streaks.insert(
            parts[0].to_owned(),
            HostStreak {
                host: parts[0].to_owned(),
                state: parts[1].to_owned(),
                bad_probes: bad,
                good_probes: good,
                since_epoch: since,
            },
        );
    }
    streaks
}

/// Parses the resource status format; `device`, `card` and `host` name the same thing in its
/// `remote` and `local` lines, and `whole-device` and `whole-card` the same wait.
pub fn parse_lease_status(text: &str) -> LeaseStatus {
    let mut status = LeaseStatus::default();
    let mut pending_slot_lines = Vec::new();
    for raw_line in split_lines(text) {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(card) = REMOTE_CARD.captures(line) {
            status.remote_cards.push(RemoteCard {
                host: group(&card, 1).unwrap_or_default().to_owned(),
                first_slot: number(&card, 2),
                last_slot: number(&card, 3),
            });
        } else if let Some(only) = HARNESS_ONLY.captures(line) {
            status.harness_only_hosts = group(&only, 1)
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_owned)
                .collect();
        } else if LOCAL_OFF.is_match(line) {
            status.local_card_off = true;
        } else if let Some(priority) = line.strip_prefix("operator priority:") {
            status.operator_priority = priority.split_whitespace().map(str::to_owned).collect();
        } else if let Some(wait) = WAIT.captures(line) {
            let kind = group(&wait, 1).unwrap_or_default();
            status.waits.push(LeaseWait {
                kind: if kind == "whole-card" {
                    "whole-device"
                } else {
                    kind
                }
                .to_owned(),
                issue: number(&wait, 2),
                minutes: number(&wait, 3),
                tag: group(&wait, 4).unwrap_or_default().to_owned(),
            });
        } else if line.starts_with("slot ") || line.starts_with("device ") {
            pending_slot_lines.push(line.to_owned());
        } else {
            status.unparsed_lines.push(line.to_owned());
        }
    }
    for line in pending_slot_lines {
        match parse_slot_line(&line, &status.remote_cards) {
            None => status.unparsed_lines.push(line),
            Some(slot) if slot.slot.is_none() => status.device = Some(slot),
            Some(slot) => status.slots.push(slot),
        }
    }
    status
}

/// The newest `limit` stall lines of `state/stalls.log` (or its tail), oldest first.
pub fn parse_stalls(text: &str, limit: usize) -> Vec<StallLine> {
    let stalls: Vec<StallLine> = split_lines(text)
        .into_iter()
        .filter_map(|line| {
            let found = STALL.captures(line.trim())?;
            Some(StallLine {
                time: group(&found, 1)?.to_owned(),
                kind: group(&found, 2)?.to_owned(),
                key: group(&found, 3)?.to_owned(),
                detail: group(&found, 4).unwrap_or_default().to_owned(),
            })
        })
        .collect();
    newest(stalls, limit)
}

fn newest<T>(mut items: Vec<T>, limit: usize) -> Vec<T> {
    if limit == 0 {
        return Vec::new();
    }
    let excess = items.len().saturating_sub(limit);
    items.drain(..excess);
    items
}

/// The newest `limit` decisions of `state/logs/allocator.log` (or its tail), oldest first;
/// `kind` is spawn, chain, landing, cooldown, usage or other.
pub fn parse_allocator(text: &str, limit: usize) -> Vec<AllocatorLine> {
    let mut decisions = Vec::new();
    for line in split_lines(text) {
        let Some(found) = ALLOCATOR.captures(line.trim()) else {
            continue;
        };
        let stamp = group(&found, 1).unwrap_or_default();
        let rest = group(&found, 2).unwrap_or_default();
        let time = format!(
            "{}-{}-{}T{}:{}:{}Z",
            &stamp[0..4],
            &stamp[4..6],
            &stamp[6..8],
            &stamp[9..11],
            &stamp[11..13],
            &stamp[13..15]
        );
        let kind = if rest.contains("chains tick") {
            "chain"
        } else if rest.contains("landing tick") {
            "landing"
        } else if rest.contains("usage limit") {
            "cooldown"
        } else if rest.contains("-> spawning") {
            "spawn"
        } else if rest.contains("usage reading") {
            "usage"
        } else {
            "other"
        };
        // The agents' parts first: their windows hold `%` and `:`, which no field does.
        let mut usage = Vec::new();
        let mut plain = rest.to_owned();
        for found in USAGE_PART.captures_iter(rest) {
            let (Some(whole), Some(agent), Some(cap), Some(windows)) =
                (found.get(0), found.get(1), found.get(2), found.get(3))
            else {
                continue;
            };
            usage.push(AgentUsageLine {
                agent: agent.as_str().to_owned(),
                cap: cap.as_str().parse().unwrap_or_default(),
                windows: usage_windows(windows.as_str()),
            });
            plain.replace_range(whole.range(), &" ".repeat(whole.len()));
        }
        let mut fields = allocator_fields(&plain);
        if let Some(spawning) = SPAWNING.captures(rest).and_then(|found| group(&found, 1)) {
            fields.insert("spawning".to_owned(), spawning.to_owned());
        }
        decisions.push(AllocatorLine {
            time,
            kind: kind.to_owned(),
            slot: ALLOCATOR_SLOT
                .captures(rest)
                .and_then(|slot| group(&slot, 1))
                .map(str::to_owned),
            fields,
            usage,
            text: rest.to_owned(),
        });
    }
    newest(decisions, limit)
}

/// The `name=value` fields of an allocator line, a name starting at a word boundary: one field
/// may follow another's `%` directly (`a=1%b=2`).
fn allocator_fields(text: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let mut position = 0;
    while let Some(found) = ALLOCATOR_FIELD.captures_at(text, position) {
        let (Some(whole), Some(name), Some(value)) = (found.get(0), found.get(1), found.get(2))
        else {
            break;
        };
        if text[..whole.start()]
            .chars()
            .next_back()
            .is_some_and(is_word_char)
        {
            // Every later start in this name follows a word character too.
            position = name.end();
            continue;
        }
        fields.insert(name.as_str().to_owned(), value.as_str().to_owned());
        position = whole.end();
    }
    fields
}

/// The windows of an agent's part, `five_hour:10%,weekly:68%`; none for `unknown`.
fn usage_windows(text: &str) -> Option<Vec<UsageWindowLine>> {
    if text == "unknown" {
        return None;
    }
    Some(
        text.split(',')
            .filter_map(|window| {
                let (name, used) = window.split_once(':')?;
                Some(UsageWindowLine {
                    name: name.to_owned(),
                    used_pct: optional_float(used.strip_suffix('%')?)?,
                })
            })
            .collect(),
    )
}

/// Parses one `state/locks/claim.<issue>` file: `<issue> <pid> <since>`.
pub fn parse_claim(file_name: &str, text: &str) -> Option<ClaimRecord> {
    let issue = CLAIM_NAME
        .captures(file_name)
        .map(|found| number(&found, 1))?;
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() < 3 {
        return None;
    }
    Some(ClaimRecord {
        issue,
        pid: optional_int(parts[1])?,
        since: parts[2].to_owned(),
    })
}

/// Parses the machine probe's output: a `load` line, two meminfo lines, then `--- device`,
/// `--- processes` and `--- cgroup` sections when a device is probed, and a `--- docker` section.
pub fn parse_machine_probe(text: &str) -> MachineProbe {
    let mut section = "head".to_owned();
    let mut probe = MachineProbe::default();
    let mut meminfo_kb: HashMap<&str, i64> = HashMap::new();
    let mut app_rows: Vec<(i64, Option<f64>, String)> = Vec::new();
    let mut container_by_pid: HashMap<i64, String> = HashMap::new();
    for line in split_lines(text) {
        if let Some(name) = line.strip_prefix("--- ") {
            name.trim().clone_into(&mut section);
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        match section.as_str() {
            "head" => {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts[0] == "load" && parts.len() >= 3 {
                    probe.load1 = optional_float(parts[1]);
                    probe.nproc = optional_int(parts[2]);
                } else if (parts[0] == "MemTotal:" || parts[0] == "MemAvailable:")
                    && parts.len() >= 2
                    && MEMINFO_KB.is_match(parts[1])
                {
                    meminfo_kb.insert(
                        parts[0].trim_end_matches(':'),
                        parts[1].parse().unwrap_or_default(),
                    );
                }
            }
            "device" if probe.gpu.is_none() => {
                let columns: Vec<&str> = line.split(',').map(str::trim).collect();
                if columns.len() >= 4 {
                    probe.gpu = Some(DeviceReading {
                        name: columns[0].to_owned(),
                        util_pct: optional_float(columns[1]),
                        mem_used_mb: optional_float(columns[2]),
                        mem_total_mb: optional_float(columns[3]),
                    });
                }
            }
            "processes" => {
                let columns: Vec<&str> = line.splitn(3, ',').map(str::trim).collect();
                if columns.len() == 3
                    && let Some(pid) = optional_int(columns[0])
                {
                    app_rows.push((pid, optional_float(columns[1]), columns[2].to_owned()));
                }
            }
            "cgroup" => {
                let (pid_text, cgroup_text) = line.split_once(' ').unwrap_or((line, ""));
                if let Some(pid) = optional_int(pid_text)
                    && let Some(found) = DOCKER_CGROUP
                        .captures(cgroup_text)
                        .and_then(|found| group(&found, 1))
                {
                    container_by_pid.insert(pid, found.to_owned());
                }
            }
            "docker" => {
                let columns: Vec<&str> = line.split('\t').collect();
                if columns.len() >= 4 {
                    probe.containers.push(ContainerInfo {
                        container_id: columns[0].chars().take(12).collect(),
                        name: columns[1].to_owned(),
                        status: columns[2].to_owned(),
                        image: columns[3].to_owned(),
                    });
                }
            }
            _ => {}
        }
    }
    let name_by_id: HashMap<&str, &str> = probe
        .containers
        .iter()
        .map(|container| (container.container_id.as_str(), container.name.as_str()))
        .collect();
    probe.gpu_processes = app_rows
        .into_iter()
        .map(|(pid, used_mb, name)| {
            let container_id = container_by_pid.get(&pid);
            let container = container_id.map(|id| {
                name_by_id
                    .get(id.as_str())
                    .map_or_else(|| id.clone(), |name| (*name).to_owned())
            });
            DeviceProcess {
                pid,
                used_mb,
                name,
                container,
            }
        })
        .collect();
    let total = meminfo_kb.get("MemTotal").copied();
    let available = meminfo_kb.get("MemAvailable").copied();
    #[allow(clippy::cast_precision_loss)]
    {
        probe.mem_used_mb = total
            .zip(available)
            .map(|(total, available)| (total - available) as f64 / 1024.0);
        probe.mem_total_mb = total.map(|total| total as f64 / 1024.0);
    }
    probe
}

/// The issue a worktree (`<worktree root>/<name>-<issue>`) or run packet
/// (`<YYYY-MM-DD>-<issue>-<slug>`) in a path belongs to; an empty root matches no worktree.
pub fn issue_from_path(path: &str, worktree_root: &str) -> Option<i64> {
    let root = worktree_root.trim_end_matches('/');
    if !root.is_empty()
        && let Ok(pattern) = Regex::new(&format!(
            r"{}/[A-Za-z][A-Za-z0-9-]*-([0-9]{{1,9}})(?:/|$)",
            regex::escape(root)
        ))
        && let Some(found) = pattern.captures(path)
    {
        return Some(number(&found, 1));
    }
    PACKET_ISSUE.captures(path).map(|found| number(&found, 1))
}

/// Names a model id for people: `claude-opus-5-5` reads `Opus 5.5`; an id that does not follow
/// the pattern reads as itself.
pub fn model_label(model_id: &str) -> String {
    let Some(found) = MODEL.captures(model_id) else {
        return if model_id.is_empty() {
            "?".to_owned()
        } else {
            model_id.to_owned()
        };
    };
    let family = group(&found, 1).unwrap_or_default();
    let mut capitalized: String = family
        .chars()
        .take(1)
        .flat_map(char::to_uppercase)
        .collect();
    capitalized.push_str(&family.chars().skip(1).collect::<String>());
    let mut version = group(&found, 2).unwrap_or_default().to_owned();
    if let Some(minor) = group(&found, 3) {
        version.push('.');
        version.push_str(minor);
    }
    format!("{capitalized} {version}")
}

/// The user command in a Claude Code tool shell's `bash -c` argument, which wraps it as
/// `eval '<command>' < /dev/null`, with its quoting undone and whitespace collapsed.
pub fn tool_command(bash_argument: &str) -> Option<String> {
    let command = TOOL_EVAL
        .captures(bash_argument)
        .and_then(|found| group(&found, 1))?;
    let command = command.replace("'\"'\"'", "'").replace("'\\''", "'");
    Some(command.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn parse_slot_line(line: &str, remote_cards: &[RemoteCard]) -> Option<LeaseSlot> {
    if let Some(free) = SLOT_FREE.captures(line) {
        let slot = number(&free, 1);
        return Some(LeaseSlot {
            slot: Some(slot),
            state: "free".to_owned(),
            holder: None,
            issue: None,
            pid: None,
            since: None,
            domain: None,
            tags: Vec::new(),
            host: slot_host(slot, remote_cards),
        });
    }
    let held = SLOT_HELD.captures(line)?;
    let slot = group(&held, 1).and_then(|text| text.parse().ok());
    let holder = group(&held, 3).unwrap_or_default().to_owned();
    let mut domain = None;
    let mut tags = Vec::new();
    for tag in group(&held, 6).unwrap_or_default().split_whitespace() {
        if let Some(value) = tag.strip_prefix("domain=") {
            domain = optional_int(value);
        } else {
            tags.push(tag.to_owned());
        }
    }
    Some(LeaseSlot {
        slot,
        state: if group(&held, 2) == Some("HELD") {
            "held"
        } else {
            "orphan"
        }
        .to_owned(),
        issue: optional_int(&holder),
        holder: Some(holder),
        pid: Some(number(&held, 4)),
        since: group(&held, 5).map(str::to_owned),
        domain,
        tags,
        host: slot.map_or_else(|| "local".to_owned(), |slot| slot_host(slot, remote_cards)),
    })
}

fn slot_host(slot: i64, remote_cards: &[RemoteCard]) -> String {
    remote_cards
        .iter()
        .find(|card| card.first_slot <= slot && slot <= card.last_slot)
        .map_or_else(|| "local".to_owned(), |card| card.host.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::testkit::fixture_text;

    #[test]
    fn host_health_rows() {
        let rows = parse_host_health_tsv(&fixture_text("host_health.tsv"));
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0].host, "ci@worker-a.local");
        assert_eq!(rows[0].upload_mb_s, Some(65.68));
        assert_eq!(rows[2].gpu_util_pct, Some(32.0));
        assert!(rows[3].upload_timed_out);
        assert_eq!(rows[3].upload_mb_s, None);
        assert_eq!(rows[3].status, "slow");
        assert_eq!(rows[5].status, "down");
        assert_eq!(rows[5].load1, None);
        assert!((rows[5].epoch - 1_795_656_961.0).abs() < f64::EPSILON);
    }

    #[test]
    fn streaks() {
        let streaks = parse_streaks(&fixture_text("host_health.streaks"));
        assert_eq!(streaks["ci@192.0.2.6"].state, "demoted");
        assert_eq!(streaks["ci@192.0.2.6"].bad_probes, 3);
        assert_eq!(streaks["ci@worker-a.local"].good_probes, 275);
    }

    #[test]
    fn lease_status() {
        let status = parse_lease_status(&fixture_text("lease_status.txt"));
        let hosts: Vec<&str> = status
            .remote_cards
            .iter()
            .map(|card| card.host.as_str())
            .collect();
        assert_eq!(hosts, ["ci@worker-a.local", "ci@192.0.2.6"]);
        assert_eq!(status.remote_cards[1].first_slot, 6);
        assert_eq!(
            status.harness_only_hosts,
            ["ci@192.0.2.6", "ci@worker-a.local"]
        );
        assert!(status.local_card_off);
        let by_slot: HashMap<i64, &LeaseSlot> = status
            .slots
            .iter()
            .filter_map(|slot| Some((slot.slot?, slot)))
            .collect();
        assert_eq!(status.slots.len(), 7);
        assert_eq!(by_slot[&1].state, "free");
        assert_eq!(by_slot[&1].host, "local");
        assert_eq!(
            (
                by_slot[&2].holder.as_deref(),
                by_slot[&2].issue,
                by_slot[&2].pid
            ),
            (Some("owner"), None, Some(4242))
        );
        assert_eq!(by_slot[&3].state, "orphan");
        assert_eq!(by_slot[&4].issue, Some(2490));
        assert_eq!(by_slot[&4].tags, ["remote-all", "render"]);
        assert_eq!(by_slot[&4].domain, Some(80));
        assert_eq!(by_slot[&4].host, "ci@worker-a.local");
        assert_eq!(by_slot[&6].host, "ci@192.0.2.6");
        let device = status.device.as_ref().expect("a device lease");
        assert_eq!(
            (device.issue, device.tags.clone()),
            (Some(2466), vec!["device".to_owned()])
        );
        assert_eq!(status.operator_priority, ["2490", "2488"]);
        let waits: Vec<(&str, i64, i64, &str)> = status
            .waits
            .iter()
            .map(|wait| {
                (
                    wait.kind.as_str(),
                    wait.issue,
                    wait.minutes,
                    wait.tag.as_str(),
                )
            })
            .collect();
        assert_eq!(
            waits,
            [
                ("whole-device", 2467, 35, "priority"),
                ("single-slot", 2470, 12, "first in line")
            ]
        );
        assert_eq!(status.unparsed_lines, ["something unexpected"]);
    }

    #[test]
    fn lease_status_accepts_card_and_host_for_device() {
        let status = parse_lease_status(
            "remote card: ci@a (slots 1-2)\nremote host: ci@b (slots 3-3)\nlocal card: off\n\
             wait whole-card issue 5 for 2 min\nwait whole-pool issue 6 for 3 min\n",
        );
        let cards: Vec<(&str, i64)> = status
            .remote_cards
            .iter()
            .map(|card| (card.host.as_str(), card.first_slot))
            .collect();
        assert_eq!(cards, [("ci@a", 1), ("ci@b", 3)]);
        assert!(status.local_card_off);
        let waits: Vec<(&str, i64)> = status
            .waits
            .iter()
            .map(|wait| (wait.kind.as_str(), wait.issue))
            .collect();
        assert_eq!(waits, [("whole-device", 5), ("whole-pool", 6)]);
        assert!(status.unparsed_lines.is_empty());
    }

    #[test]
    fn stalls_keep_the_newest() {
        let stalls = parse_stalls(&fixture_text("stalls.log"), 2);
        let kinds: Vec<&str> = stalls.iter().map(|stall| stall.kind.as_str()).collect();
        assert_eq!(kinds, ["lock_wait", "device_idle"]);
        assert_eq!(stalls[0].key, "/tmp/dev-build-remote-dev-a.lock");
        assert!(stalls[1].detail.starts_with("device 0 %"));
    }

    #[test]
    fn allocator_lines() {
        let lines = parse_allocator(&fixture_text("allocator.log"), 10);
        let kinds: Vec<&str> = lines.iter().map(|line| line.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["chain", "spawn", "cooldown", "usage", "landing", "chain"]
        );
        assert_eq!(lines[0].time, "2026-11-26T01:00:13Z");
        assert_eq!(lines[0].slot.as_deref(), Some("slot2"));
        assert_eq!(lines[1].usage.len(), 2);
        assert_eq!(
            lines[1].usage[1],
            AgentUsageLine {
                agent: "codex".to_owned(),
                cap: 2,
                windows: Some(vec![UsageWindowLine {
                    name: "primary".to_owned(),
                    used_pct: 71.5
                }]),
            }
        );
        assert_eq!(
            lines[1].usage[0]
                .windows
                .as_ref()
                .map(|windows| windows[1].used_pct),
            Some(50.0)
        );
        assert_eq!(lines[4].usage[1].windows, None);
        assert!(!lines[1].fields.contains_key("claude"));
        assert_eq!(lines[1].fields["allowed"], "10");
        assert_eq!(lines[1].fields["spawning"], "1");
        assert_eq!(lines[4].fields["ready"], "1");
        assert_eq!(lines[5].fields["claimable"], "2");
        let glued = allocator_fields("pct=1%b=2 Xc=3 d=4");
        assert_eq!(
            glued.keys().map(String::as_str).collect::<Vec<_>>(),
            ["b", "d", "pct"]
        );
    }

    #[test]
    fn claim_file() {
        assert_eq!(
            parse_claim("claim.2464", "2464 18907 2026-11-26T01:00:51Z\n"),
            Some(ClaimRecord {
                issue: 2464,
                pid: 18907,
                since: "2026-11-26T01:00:51Z".to_owned()
            })
        );
        assert_eq!(parse_claim("claim.guard", ""), None);
        assert_eq!(parse_claim("claim.12", "garbage"), None);
    }

    #[test]
    fn machine_probe() {
        let probe = parse_machine_probe(&fixture_text("machine_probe.txt"));
        assert_eq!((probe.load1, probe.nproc), (Some(8.12), Some(24)));
        assert!((probe.mem_total_mb.unwrap_or_default() - 65_481_572.0 / 1024.0).abs() < 1e-6);
        assert!(
            (probe.mem_used_mb.unwrap_or_default() - (65_481_572.0 - 18_771_028.0) / 1024.0).abs()
                < 1e-6
        );
        let gpu = probe.gpu.as_ref().expect("a device");
        assert_eq!(gpu.name, "NVIDIA GeForce RTX 5080");
        assert_eq!(
            (gpu.util_pct, gpu.mem_used_mb, gpu.mem_total_mb),
            (Some(63.0), Some(5940.0), Some(16303.0))
        );
        let processes: Vec<(i64, Option<&str>)> = probe
            .gpu_processes
            .iter()
            .map(|process| (process.pid, process.container.as_deref()))
            .collect();
        assert_eq!(
            processes,
            [(3_557_139, Some("dev-run-4")), (4_100_000, None)]
        );
        let names: Vec<&str> = probe
            .containers
            .iter()
            .map(|container| container.name.as_str())
            .collect();
        assert_eq!(names, ["dev-run-4", "sim_dev"]);
    }

    #[test]
    fn machine_probe_without_device() {
        let probe = parse_machine_probe("load 0.5 4\n--- docker\n");
        assert!(probe.gpu.is_none());
        assert!(probe.gpu_processes.is_empty());
    }

    #[test]
    fn model_labels() {
        assert_eq!(model_label("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_label("claude-sonnet-5-2"), "Sonnet 5.2");
        assert_eq!(model_label("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(model_label("custom"), "custom");
    }

    #[test]
    fn tool_commands() {
        let wrapped = "source /x/snapshot.sh 2>/dev/null || true && eval 'timeout 60 bash -c '\"'\"'until grep -q \
                       x f; do sleep 10; done'\"'\"'; date -u' < /dev/null && pwd -P >| /tmp/claude-cwd";
        assert_eq!(
            tool_command(wrapped).as_deref(),
            Some("timeout 60 bash -c 'until grep -q x f; do sleep 10; done'; date -u")
        );
        assert_eq!(tool_command("plain command"), None);
    }

    #[test]
    fn a_worktree_names_its_issue_under_the_configured_root_only() {
        assert_eq!(
            issue_from_path("/srv/trees/app-42/src", "/srv/trees/"),
            Some(42)
        );
        assert_eq!(issue_from_path("/srv/trees/app-7", "/srv/trees"), Some(7));
        assert_eq!(
            issue_from_path("/srv/trees/my-app-12", "/srv/trees"),
            Some(12)
        );
        assert_eq!(issue_from_path("/srv/elsewhere/app-42", "/srv/trees"), None);
        assert_eq!(issue_from_path("/srv/trees/app-42", ""), None);
        assert_eq!(
            issue_from_path("/srv/runs/2026-11-26-2502-push/runs", ""),
            Some(2502)
        );
    }

    #[test]
    fn huge_numbers_in_paths_and_state_files_read_as_no_number() {
        let huge = "9".repeat(5000);
        assert_eq!(
            issue_from_path(&format!("/srv/work/app-{huge}"), "/srv/work"),
            None
        );
        assert_eq!(
            issue_from_path(&format!("/x/2026-11-26-{huge}-slug/runs"), ""),
            None
        );
        assert_eq!(
            parse_claim(&format!("claim.{huge}"), "1 2 2026-11-26T00:00:00Z"),
            None
        );
        assert!(parse_streaks(&format!("h ok {huge} 1 1\n")).is_empty());
        assert_eq!(
            parse_lease_status(&format!(
                "slot 1 HELD {huge} 7 2026-11-26T00:00:00Z (alive)\n"
            ))
            .slots[0]
                .issue,
            None
        );
    }
}
