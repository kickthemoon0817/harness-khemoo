//! Finds the harness's live `claude -p` ticks in /proc and the issue each one works.
//!
//! A tick is a process whose argv[0] base name is claude, claude-code or node and argv[1] is
//! `-p`, the local ticks the launcher runs and the tick-pid helper's default agent executables,
//! and whose stdout is a `state/logs/tick-*.log` of this harness home, as the launcher's runner
//! redirects it; a `claude -p` of another harness home or outside any harness is left out. Its
//! issue comes, cheapest first, from the newest `state/locks/claim.<issue>` naming its pid, then
//! from a lease it holds, then from a cached GitHub claim-comment lookup the caller supplies.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use super::files::read_text_limited;
use super::models::{ClaimRecord, LeaseSlot, ProcessInfo, TickProcess};
use super::parsers::{model_label, tool_command};
use super::redact::{summarize, summarize_argv};
use super::text::{basename, clip};

const CLAUDE_BINARIES: [&str; 3] = ["claude", "claude-code", "node"];
const LANDING_MARK: &str = "This tick is the merge slot's landing tick";
const TOOL_COMMAND_MAX_CHARS: usize = 160;
const PROC_FILE_BYTES: u64 = 1024 * 1024;
static SLOT_LOCK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/locks/slot([0-9]{1,9})\.lock$").expect("a constant pattern compiles")
});

/// A harness tick before its issue is known: the process and what its descriptors say.
#[derive(Clone, Debug)]
pub struct TickCandidate {
    pub process: ProcessInfo,
    pub slot: Option<i64>,
    pub log: Option<String>,
}

pub type ProcessTable = HashMap<i64, ProcessInfo>;

/// Reads the process table and recognises harness ticks.
pub struct ProcessScanner {
    pub state_dir: PathBuf,
    proc_root: PathBuf,
}

impl ProcessScanner {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            state_dir: state_dir.to_path_buf(),
            proc_root: PathBuf::from("/proc"),
        }
    }

    /// Every process's parent, start time and argv; processes that exit mid-read are left out.
    pub fn scan_processes(&self) -> io::Result<ProcessTable> {
        let boot_epoch = self.boot_epoch()?;
        #[allow(clippy::cast_precision_loss)]
        let clock_ticks = rustix::param::clock_ticks_per_second() as f64;
        let mut processes = HashMap::new();
        for entry in std::fs::read_dir(&self.proc_root)? {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .filter(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
                .and_then(|name| name.parse::<i64>().ok())
            else {
                continue;
            };
            let (Ok(stat), Ok(cmdline)) = (
                read_text_limited(&entry.path().join("stat"), PROC_FILE_BYTES),
                read_text_limited(&entry.path().join("cmdline"), PROC_FILE_BYTES),
            ) else {
                continue;
            };
            let fields: Vec<&str> = stat
                .rsplit_once(')')
                .map_or("", |(_, rest)| rest)
                .split_whitespace()
                .collect();
            if fields.len() < 20 {
                continue;
            }
            let (Ok(ppid), Ok(start_ticks)) = (fields[1].parse::<i64>(), fields[19].parse::<u64>())
            else {
                continue;
            };
            #[allow(clippy::cast_precision_loss)]
            let start_epoch = boot_epoch + start_ticks as f64 / clock_ticks;
            let argv = cmdline
                .split('\0')
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect();
            processes.insert(
                pid,
                ProcessInfo {
                    pid,
                    ppid,
                    start_epoch,
                    argv,
                },
            );
        }
        Ok(processes)
    }

    /// The harness ticks in the process table, oldest first, with their slot lock and log when
    /// their descriptors name them.
    pub fn find_ticks(&self, processes: &ProcessTable) -> Vec<TickCandidate> {
        let logs_prefix = format!("{}/logs/tick-", self.state_dir.display());
        let state_prefix = self.state_dir.display().to_string();
        let mut candidates = Vec::new();
        for process in processes.values() {
            if process.argv.len() < 2
                || process.argv[1] != "-p"
                || !CLAUDE_BINARIES.contains(&basename(&process.argv[0]))
            {
                continue;
            }
            let targets = self.descriptor_targets(process.pid);
            let Some(stdout_target) = targets
                .get("1")
                .filter(|target| target.starts_with(&logs_prefix))
            else {
                continue;
            };
            let mut slot = None;
            for target in targets.values() {
                if let Some(found) = SLOT_LOCK.captures(target)
                    && target.starts_with(&state_prefix)
                {
                    slot = found.get(1).and_then(|number| number.as_str().parse().ok());
                }
            }
            candidates.push(TickCandidate {
                process: process.clone(),
                slot,
                log: Some(basename(stdout_target).to_owned()),
            });
        }
        candidates.sort_by(|a, b| a.process.start_epoch.total_cmp(&b.process.start_epoch));
        candidates
    }

    /// Names each tick's model, lane, age, issue and current tool call, oldest first.
    pub fn describe_ticks(
        candidates: &[TickCandidate],
        processes: &ProcessTable,
        claims: &[ClaimRecord],
        lease_slots: &[LeaseSlot],
        github_issue_by_pid: &HashMap<i64, i64>,
        now: f64,
    ) -> Vec<TickProcess> {
        let issue_by_pid = map_pids_to_issues(claims, lease_slots, github_issue_by_pid);
        let mut children_by_parent: HashMap<i64, Vec<&ProcessInfo>> = HashMap::new();
        for process in processes.values() {
            children_by_parent
                .entry(process.ppid)
                .or_default()
                .push(process);
        }
        candidates
            .iter()
            .map(|candidate| {
                let argv = &candidate.process.argv;
                let mut model = String::new();
                for pair in argv.windows(2) {
                    if pair[0] == "--model" {
                        model.clone_from(&pair[1]);
                    }
                }
                let prompt = argv.get(2).map_or("", String::as_str);
                let lane = if prompt.starts_with(LANDING_MARK) {
                    "landing"
                } else if prompt
                    .split('\n')
                    .next()
                    .unwrap_or_default()
                    .contains(", the model lane:")
                {
                    "model"
                } else {
                    "standard"
                };
                let issue = issue_by_pid.get(&candidate.process.pid);
                let (doing, doing_age_s, tool_shells) =
                    current_tool_call(candidate.process.pid, &children_by_parent, now);
                #[allow(clippy::cast_possible_truncation)]
                TickProcess {
                    pid: candidate.process.pid,
                    model_label: model_label(&model),
                    model,
                    lane: lane.to_owned(),
                    age_s: (now - candidate.process.start_epoch).max(0.0) as i64,
                    slot: candidate.slot,
                    log: candidate.log.clone(),
                    issue: issue.map(|(issue, _)| *issue),
                    issue_source: issue.map(|(_, source)| (*source).to_owned()),
                    doing,
                    doing_age_s,
                    tool_shells,
                }
            })
            .collect()
    }

    /// What a lease holder pid is: `tick`, a redacted summary of its command, or `dead`.
    pub fn describe_holder(
        pid: Option<i64>,
        processes: &ProcessTable,
        tick_pids: &[i64],
    ) -> String {
        let Some(pid) = pid else { return String::new() };
        if tick_pids.contains(&pid) {
            return "tick".to_owned();
        }
        match processes.get(&pid) {
            None => "dead".to_owned(),
            Some(process) => {
                let summary = summarize_argv(&process.argv, 2);
                if summary.is_empty() {
                    "?".to_owned()
                } else {
                    summary
                }
            }
        }
    }

    fn descriptor_targets(&self, pid: i64) -> HashMap<String, String> {
        let mut targets = HashMap::new();
        let Ok(entries) = std::fs::read_dir(self.proc_root.join(pid.to_string()).join("fd")) else {
            return targets;
        };
        for entry in entries.flatten() {
            if let Ok(target) = std::fs::read_link(entry.path()) {
                targets.insert(
                    entry.file_name().to_string_lossy().into_owned(),
                    target.to_string_lossy().into_owned(),
                );
            }
        }
        targets
    }

    fn boot_epoch(&self) -> io::Result<f64> {
        let stat = read_text_limited(&self.proc_root.join("stat"), PROC_FILE_BYTES)?;
        stat.lines()
            .find_map(|line| {
                line.strip_prefix("btime ")
                    .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no btime in /proc/stat"))
    }
}

/// A pid to the issue it works and where that came from (`claim`, `lease` or `github`),
/// cheapest source first. A tick takes its claim before its claim comment, so the newest claim
/// naming a pid is the issue it works now; a pid with no claim falls back to a lease it holds,
/// then to the GitHub lookup.
pub fn map_pids_to_issues(
    claims: &[ClaimRecord],
    lease_slots: &[LeaseSlot],
    github_issue_by_pid: &HashMap<i64, i64>,
) -> HashMap<i64, (i64, &'static str)> {
    let mut mapping: HashMap<i64, (i64, &'static str)> = github_issue_by_pid
        .iter()
        .map(|(pid, issue)| (*pid, (*issue, "github")))
        .collect();
    for slot in lease_slots {
        if let (Some(pid), Some(issue)) = (slot.pid, slot.issue) {
            mapping.insert(pid, (issue, "lease"));
        }
    }
    let mut newest_since_by_pid: HashMap<i64, &str> = HashMap::new();
    for claim in claims {
        if claim.since.as_str() >= newest_since_by_pid.get(&claim.pid).copied().unwrap_or("") {
            newest_since_by_pid.insert(claim.pid, &claim.since);
            mapping.insert(claim.pid, (claim.issue, "claim"));
        }
    }
    mapping
}

/// The tick's youngest tool shell, a redacted summary of the command it runs, its age, and how
/// many tool shells the tick has open; the raw command line never leaves here.
fn current_tool_call(
    tick_pid: i64,
    children_by_parent: &HashMap<i64, Vec<&ProcessInfo>>,
    now: f64,
) -> (Option<String>, Option<i64>, i64) {
    let tool_shells: Vec<&ProcessInfo> = children_by_parent
        .get(&tick_pid)
        .into_iter()
        .flatten()
        .copied()
        .filter(|child| {
            let head = child
                .argv
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            !head.contains("mcp")
                && child.argv.len() >= 3
                && ["bash", "sh", "zsh"].contains(&basename(&child.argv[0]))
                && child.argv[1] == "-c"
        })
        .collect();
    let Some(youngest) = tool_shells.iter().copied().reduce(|best, shell| {
        if shell.start_epoch > best.start_epoch {
            shell
        } else {
            best
        }
    }) else {
        return (None, None, 0);
    };
    let summary = match tool_command(&youngest.argv[2]) {
        Some(command) => {
            let summary = summarize(&command, 4, 32);
            if summary.is_empty() {
                "shell step".to_owned()
            } else {
                summary
            }
        }
        None => children_by_parent
            .get(&youngest.pid)
            .and_then(|grandchildren| grandchildren.first())
            .map_or_else(
                || "shell step".to_owned(),
                |grandchild| summarize_argv(&grandchild.argv, 2),
            ),
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    (
        Some(clip(&summary, TOOL_COMMAND_MAX_CHARS)),
        Some((now - youngest.start_epoch).max(0.0) as i64),
        tool_shells.len() as i64,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(slot: i64, pid: i64) -> LeaseSlot {
        LeaseSlot {
            slot: Some(slot),
            state: "held".to_owned(),
            holder: Some("2490".to_owned()),
            issue: Some(2490),
            pid: Some(pid),
            since: Some("x".to_owned()),
            domain: None,
            tags: Vec::new(),
            host: "h".to_owned(),
        }
    }

    #[test]
    fn newest_claim_wins_then_lease_then_github() {
        let claim = |issue, pid, since: &str| ClaimRecord {
            issue,
            pid,
            since: since.to_owned(),
        };
        let claims = [
            claim(2001, 10, "2026-11-25T01:00:00Z"),
            claim(2464, 10, "2026-11-26T01:00:00Z"),
            claim(2300, 30, "2026-11-26T00:00:00Z"),
        ];
        let mapping = map_pids_to_issues(
            &claims,
            &[slot(4, 10), slot(5, 20)],
            &[(20, 9999), (40, 2600)].into(),
        );
        assert_eq!(mapping[&10], (2464, "claim"));
        assert_eq!(mapping[&20], (2490, "lease"));
        assert_eq!(mapping[&30], (2300, "claim"));
        assert_eq!(mapping[&40], (2600, "github"));
    }
}
