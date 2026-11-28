//! `harness claimable`: the dependency-aware queue depth. Counts the open `ISSUE_LABEL` issues
//! without a live claimant whose "Depends on:" issue numbers are all closed, and which are not
//! parked for the owner (`SIGNOFF_LABEL`) or the operator (`OPERATOR_LABEL`). Prints one integer, or
//! nothing and exits non-zero when GitHub cannot be read.
//!
//! Priority: an issue labelled `LOW_LABEL` counts only when no other issue of the list is
//! claimable, and none labelled `FIRST_LABEL` is claimable anywhere; an issue labelled `LANE_LABEL`
//! (the model lane; off when empty) without `FIRST_LABEL` counts only while no `FIRST_LABEL` issue is
//! claimable anywhere. So low-priority work and lane work wait for the take-first queue.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

use crate::gh;
use crate::home::{env_nonempty, harness_home};

#[derive(clap::Args)]
#[command(after_help = "\
Reads GH_REPO (required), ISSUE_LABEL, WIP_LABEL, SIGNOFF_LABEL, OPERATOR_LABEL, FIRST_LABEL, LOW_LABEL
and LANE_LABEL, and the local claims under $HARNESS_STATE/locks (else $HARNESS_HOME/state/locks). With
QUEUE_TOTAL_FILE set, it also writes there how many open ISSUE_LABEL issues it listed, claimed ones included.")]
pub struct Args {}

static CLAIMED: LazyLock<Regex> =
    LazyLock::new(|| constant(r"harness tick .*?pid ([0-9]+) (?:claiming|resuming)"));
static WITHDRAWN: LazyLock<Regex> =
    LazyLock::new(|| constant(r"pid ([0-9]+) (?:withdrawing|pausing|releasing)"));
static DEPENDS_LINE: LazyLock<Regex> = LazyLock::new(|| constant(r"Depends on[^:\n]*:\s*(.*)"));
static ISSUE_REF: LazyLock<Regex> = LazyLock::new(|| constant(r"#([0-9]+)"));

fn constant(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|error| panic!("{pattern}: {error}"))
}

#[derive(Deserialize)]
struct Issue {
    number: u64,
    #[serde(default)]
    body: Option<String>,
    labels: Vec<Label>,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

impl Issue {
    fn has(&self, label: &str) -> bool {
        self.labels.iter().any(|own| own.name == label)
    }
}

struct Queue {
    repo: String,
    issue_label: String,
    wip: String,
    signoff: String,
    operator: String,
    first: String,
    low: String,
    lane: String,
    claim_locks: Option<PathBuf>,
    open: HashSet<u64>,
}

pub fn run(_args: &Args) -> ExitCode {
    match count() {
        Ok(count) => {
            println!("{count}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("harness claimable: {message}");
            ExitCode::from(1)
        }
    }
}

/// The setting, or `default` when it is unset or empty, as `${NAME:=default}` reads it.
fn setting(name: &str, default: &str) -> String {
    env_nonempty(name).map_or_else(
        || default.to_owned(),
        |value| value.to_string_lossy().into_owned(),
    )
}

fn count() -> Result<usize, String> {
    let repo = env_nonempty("GH_REPO")
        .ok_or("GH_REPO is not set: name the repository as owner/name")?
        .to_string_lossy()
        .into_owned();
    // A tick takes a local claim (libexec/claim.sh) before its claim comment reaches GitHub.
    let claim_locks = env_nonempty("HARNESS_STATE")
        .map(PathBuf::from)
        .or_else(|| harness_home().map(|home| home.join("state")))
        .map(|state| state.join("locks"));
    let mut queue = Queue {
        repo,
        issue_label: setting("ISSUE_LABEL", "ai"),
        wip: setting("WIP_LABEL", "ai:wip"),
        signoff: setting("SIGNOFF_LABEL", "ai:signoff"),
        operator: setting("OPERATOR_LABEL", "ai:operator"),
        first: setting("FIRST_LABEL", "ai:first"),
        low: setting("LOW_LABEL", "ai:low"),
        lane: setting("LANE_LABEL", ""),
        claim_locks,
        open: HashSet::new(),
    };
    // gh's own message reaches the caller, which logs the last line of what the count printed.
    let listed = gh::run(&queue.list_args(&queue.issue_label), gh::Stderr::Inherit)
        .map_err(|error| format!("gh issue list failed: {error}"))?;
    if !listed.status.success() {
        return Err("gh issue list failed".to_owned());
    }
    let issues: Vec<Issue> = serde_json::from_slice(&listed.stdout)
        .map_err(|error| format!("gh issue list: {error}"))?;
    queue.open = issues.iter().map(|issue| issue.number).collect();
    // The supervisor's queue event weighs the open issues of the queue, claimed ones included, from
    // this same listing: a claim and its label move an issue out of the claimable count, never out
    // of this one.
    if let Some(path) = env_nonempty("QUEUE_TOTAL_FILE") {
        std::fs::write(PathBuf::from(path), format!("{}\n", issues.len()))
            .map_err(|error| format!("QUEUE_TOTAL_FILE: {error}"))?;
    }
    queue.count(&issues)
}

impl Queue {
    fn list_args<'a>(&'a self, label: &'a str) -> [&'a str; 12] {
        [
            "issue",
            "list",
            "--repo",
            &self.repo,
            "--label",
            label,
            "--state",
            "open",
            "--limit",
            "100",
            "--json",
            "number,body,labels",
        ]
    }

    fn count(&self, issues: &[Issue]) -> Result<usize, String> {
        let mut candidates = Vec::new();
        for issue in issues {
            if self.claimable(issue)? {
                candidates.push(issue);
            }
        }
        // An empty LANE_LABEL names no label, so with the lane off no issue is lane work.
        let lane_work = |issue: &Issue| {
            !self.lane.is_empty() && issue.has(&self.lane) && !issue.has(&self.first)
        };
        let gated = candidates
            .iter()
            .any(|issue| issue.has(&self.low) || lane_work(issue));
        let first_waiting = gated && self.first_waiting(&candidates)?;
        let mut keep = candidates
            .iter()
            .filter(|issue| !(issue.has(&self.low) || (lane_work(issue) && first_waiting)))
            .count();
        if keep == 0 && !first_waiting {
            keep = candidates
                .iter()
                .filter(|issue| issue.has(&self.low))
                .count();
        }
        Ok(keep)
    }

    fn claimable(&self, issue: &Issue) -> Result<bool, String> {
        if self.local_claim_alive(issue.number) {
            return Ok(false);
        }
        // An issue waiting for its sign-off is never claimable, however its claimant ended.
        // Nor is one paused until the operator answers it: a tick could only find it waiting.
        if issue.has(&self.signoff) || issue.has(&self.operator) {
            return Ok(false);
        }
        if issue.has(&self.wip) && self.claimant_alive(issue.number)? {
            return Ok(false);
        }
        // Every "Depends on" line counts: ticks append blockers on new lines.
        let body = issue.body.as_deref().unwrap_or_default();
        for line in DEPENDS_LINE.captures_iter(body) {
            for dependency in ISSUE_REF.captures_iter(&line[1]) {
                // A number too large to be an issue cannot be closed.
                let Ok(number) = dependency[1].parse::<u64>() else {
                    return Ok(false);
                };
                if !self.closed(number)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// What `gh issue view` printed, or nothing when it failed.
    fn view(&self, number: u64, json: &str, jq: &str) -> Result<String, String> {
        let number = number.to_string();
        let output = gh::run(
            &[
                "issue", "view", &number, "--repo", &self.repo, "--json", json, "--jq", jq,
            ],
            gh::Stderr::Discard,
        )
        .map_err(|error| format!("gh issue view: {error}"))?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn closed(&self, number: u64) -> Result<bool, String> {
        if self.open.contains(&number) {
            return Ok(false);
        }
        // A dependency named by a PR number is met once the PR merges (its state reads MERGED).
        Ok(matches!(
            self.view(number, "state", ".state")?.trim(),
            "CLOSED" | "MERGED"
        ))
    }

    /// Held while any pid that claimed and did not later withdraw is alive; withdrawn or dead
    /// claimants make the issue resumable.
    fn claimant_alive(&self, number: u64) -> Result<bool, String> {
        let comments = self.view(number, "comments", ".comments[].body")?;
        let withdrawn: HashSet<&str> = WITHDRAWN
            .captures_iter(&comments)
            .filter_map(|caps| caps.get(1).map(|pid| pid.as_str()))
            .collect();
        Ok(CLAIMED
            .captures_iter(&comments)
            .filter_map(|caps| caps.get(1).map(|pid| pid.as_str()))
            .any(|pid| !withdrawn.contains(pid) && alive(pid)))
    }

    /// The local claim is taken before the comment, so it sees a claimant the comments do not
    /// show yet.
    fn local_claim_alive(&self, number: u64) -> bool {
        let Some(locks) = &self.claim_locks else {
            return false;
        };
        let Ok(file) = File::open(locks.join(format!("claim.{number}"))) else {
            return false;
        };
        let mut bytes = Vec::new();
        if file.take(4096).read_to_end(&mut bytes).is_err() {
            return false;
        }
        String::from_utf8_lossy(&bytes)
            .split_whitespace()
            .nth(1)
            .is_some_and(alive)
    }

    /// Whether a `FIRST_LABEL` issue is claimable, among the candidates or anywhere else.
    fn first_waiting(&self, candidates: &[&Issue]) -> Result<bool, String> {
        if candidates.iter().any(|issue| issue.has(&self.first)) {
            return Ok(true);
        }
        if self.issue_label == self.first {
            return Ok(false);
        }
        let output = gh::run(&self.list_args(&self.first), gh::Stderr::Discard)
            .map_err(|error| format!("gh issue list: {error}"))?;
        if !output.status.success() {
            return Ok(false);
        }
        let others: Vec<Issue> = if output.stdout.is_empty() {
            Vec::new()
        } else {
            serde_json::from_slice(&output.stdout)
                .map_err(|error| format!("gh issue list: {error}"))?
        };
        for issue in others
            .iter()
            .filter(|issue| !self.open.contains(&issue.number))
        {
            if self.claimable(issue)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Whether the process `pid` (decimal digits) exists. Signal 0 tests it without sending one; a
/// process of another user refuses the signal, and is alive.
fn alive(pid: &str) -> bool {
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Some(pid) = pid
        .parse::<i32>()
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        Err(error) => error == rustix::io::Errno::PERM,
    }
}

#[cfg(test)]
mod tests {
    use super::alive;

    #[test]
    fn this_process_is_alive_and_nonsense_is_not() {
        assert!(alive(&std::process::id().to_string()));
        for pid in ["", "0", "-1", "abc", "99999999999"] {
            assert!(!alive(pid), "{pid}");
        }
    }
}
