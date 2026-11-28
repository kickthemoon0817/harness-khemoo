//! Reads the compile lanes: each host's build lock, who holds it and who waits in line.
//!
//! The lock files are `<lock dir>/<BUILD_LOCK_REMOTE>` for each remote host, `{name}` standing
//! for its lock name, and `<lock dir>/<BUILD_LOCK_LOCAL>` for this host; an empty name leaves that
//! side out. The holder is the pid the build writes into the lock file (`pid <n>, <tree>, ...`),
//! while it lives: lslocks cannot name a holder whose descriptor a child inherited. The waiters
//! are lslocks' blocked entries on the file. A party's issue comes from its worktree
//! (`<WORKTREE_ROOT>/<name>-<issue>`), read from the lock file for the holder and from the
//! working directory for a waiter, else from the tick above it.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

use super::command::{self, Environment, MAX_OUTPUT_BYTES, RunError};
use super::files::read_owned_text;

/// A lock file holds one short line.
const LOCK_FILE_BYTES: u64 = 4096;
use super::models::{BuildLane, LockParty};
use super::parsers::issue_from_path;
use super::procs::ProcessTable;
use super::text::basename;

static LOCK_FILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^pid ([0-9]{1,9}), ([^,]*)").expect("a constant pattern compiles")
});

pub struct BuildLockReader {
    /// Remote host to its lock name (`user@host-a` to `a`); a host without one is named from its
    /// address.
    pub lock_names: HashMap<String, String>,
    pub lock_dir: PathBuf,
    pub local_file: String,
    pub remote_file: String,
    pub timeout: Duration,
    pub worktree_root: String,
}

impl BuildLockReader {
    /// The lane of this host and of each remote host, as far as their lock names are configured,
    /// this host first; an error when lslocks could not run.
    pub fn read_lanes(
        &self,
        hosts: &[String],
        processes: &ProcessTable,
        issue_by_tick_pid: &HashMap<i64, i64>,
    ) -> Result<Vec<BuildLane>, RunError> {
        let argv = ["lslocks", "-J", "-o", "PID,MODE,PATH"].map(str::to_owned);
        let output = command::run(
            &argv,
            Environment::Inherit(Vec::new()),
            self.timeout,
            MAX_OUTPUT_BYTES,
        )?;
        let waiting_pids_by_path = parse_lslocks_waiters(&output.stdout);
        let mut named_locks: Vec<(String, PathBuf)> = Vec::new();
        if !self.local_file.is_empty() {
            named_locks.push(("local".to_owned(), self.lock_dir.join(&self.local_file)));
        }
        if !self.remote_file.is_empty() {
            for host in hosts {
                let name = self.lock_names.get(host).cloned().unwrap_or_else(|| {
                    host.split_once('@')
                        .map_or(host.as_str(), |(_, address)| address)
                        .chars()
                        .map(|character| {
                            if character.is_ascii_alphanumeric() {
                                character
                            } else {
                                '-'
                            }
                        })
                        .collect()
                });
                named_locks.push((
                    host.clone(),
                    self.lock_dir
                        .join(self.remote_file.replace("{name}", &name)),
                ));
            }
        }
        Ok(named_locks
            .into_iter()
            .map(|(host, path)| {
                let (holder_pid, holder_tree) =
                    parse_lock_file(&read_owned_text(&path, LOCK_FILE_BYTES).unwrap_or_default());
                let holder = holder_pid
                    .filter(|pid| processes.contains_key(pid))
                    .map(|pid| LockParty {
                        pid,
                        issue: issue_from_path(
                            holder_tree.as_deref().unwrap_or_default(),
                            &self.worktree_root,
                        )
                        .or_else(|| tick_issue(pid, processes, issue_by_tick_pid)),
                        tree: holder_tree
                            .as_deref()
                            .map(|tree| basename(tree.trim_end_matches('/')).to_owned()),
                    });
                let waiters = waiting_pids_by_path
                    .get(path.to_string_lossy().as_ref())
                    .into_iter()
                    .flatten()
                    .map(|pid| {
                        let working_directory = working_directory(*pid);
                        let tree = basename(&working_directory);
                        LockParty {
                            pid: *pid,
                            issue: issue_from_path(&working_directory, &self.worktree_root)
                                .or_else(|| tick_issue(*pid, processes, issue_by_tick_pid)),
                            tree: (!tree.is_empty()).then(|| tree.to_owned()),
                        }
                    })
                    .collect();
                BuildLane {
                    host,
                    holder,
                    waiters,
                }
            })
            .collect())
    }
}

/// `lslocks -J -o PID,MODE,PATH`'s blocked entries (a mode ending in `*`): lock path to the pids
/// waiting on it, oldest pid first.
pub fn parse_lslocks_waiters(text: &str) -> BTreeMap<String, Vec<i64>> {
    let document: Value = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(text).unwrap_or(Value::Null)
    };
    let mut waiters: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for entry in document["locks"].as_array().into_iter().flatten() {
        if !entry["mode"].as_str().unwrap_or_default().ends_with('*') {
            continue;
        }
        if let (Some(path), Some(pid)) = (entry["path"].as_str(), entry["pid"].as_i64()) {
            waiters.entry(path.to_owned()).or_default().push(pid);
        }
    }
    for pids in waiters.values_mut() {
        pids.sort_unstable();
        pids.dedup();
    }
    waiters
}

/// The holder line a build writes into its lock, `pid <n>, <tree>, ...`: the pid and the tree.
pub fn parse_lock_file(text: &str) -> (Option<i64>, Option<String>) {
    LOCK_FILE
        .captures(text.trim())
        .map_or((None, None), |found| {
            (
                found.get(1).and_then(|pid| pid.as_str().parse().ok()),
                found
                    .get(2)
                    .map(|tree| tree.as_str().trim().to_owned())
                    .filter(|tree| !tree.is_empty()),
            )
        })
}

/// The issue of the first live tick above a process.
fn tick_issue(
    pid: i64,
    processes: &ProcessTable,
    issue_by_tick_pid: &HashMap<i64, i64>,
) -> Option<i64> {
    let mut current = pid;
    for _ in 0..64 {
        if let Some(issue) = issue_by_tick_pid.get(&current) {
            return Some(*issue);
        }
        let process = processes.get(&current)?;
        if process.ppid <= 1 {
            return None;
        }
        current = process.ppid;
    }
    None
}

fn working_directory(pid: i64) -> String {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::testkit::fixture_text;

    #[test]
    fn parses_lslocks_waiters_and_the_lock_file() {
        let waiters = parse_lslocks_waiters(&fixture_text("lslocks.json"));
        let expected: BTreeMap<String, Vec<i64>> = [
            (
                "/tmp/dev-build-remote-gpu-a.lock".to_owned(),
                vec![2_131_760, 2_387_677],
            ),
            ("/tmp/dev-build-remote-dev-a.lock".to_owned(), vec![99]),
        ]
        .into();
        assert_eq!(waiters, expected);
        assert!(parse_lslocks_waiters("not json").is_empty());
        assert_eq!(
            parse_lock_file(&fixture_text("build_lock.txt")),
            (Some(2_088_784), Some("/home/x/trees/app-2464".to_owned()))
        );
        assert_eq!(parse_lock_file(""), (None, None));
        assert_eq!(parse_lock_file("pid 1, , x"), (Some(1), None));
    }
}
