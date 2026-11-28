//! Reads the instance's run folders: whether a queue is alive, waiting for a device, or running a
//! job, and where.
//!
//! The queue log (`RUN_QUEUE_LOG`) gives the holder (its `holder <pid> start` line) and the device
//! answers, stamped `<UTC> <answer>`; the BUSY streak is the run of stamped BUSY answers at its
//! end, and an ACQUIRED answer names its host with `remote=<host>`. `status` gives the job lines
//! (`<job> start <UTC> ... host=<host>` until the job's end line, or a line starting with one of
//! the stop words), `granted:` lines, and `RELEASED <queue> <UTC>`, always its last line once the
//! queue ends. A job ends with `<job> rc=<n> end`, `<job> void<n>: ...` (abandoned with reason
//! number n), `<job> skipped: ...`, or `<job> up failed`, `<job> sidecar failed`, `<job> no
//! sidecar` (its environment did not come up).

use std::path::Path;
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

use regex::Regex;

use super::files::read_text_or_empty;
use super::models::RunQueue;
use super::parsers::issue_from_path;
use super::procs::ProcessTable;
use super::text::{is_word_char, split_lines};

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a constant pattern compiles")
}

static STAMP: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^([0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z) (.*)$"));
static HOLDER: LazyLock<Regex> = LazyLock::new(|| regex(r"^holder ([0-9]{1,9}) start "));
static WANT: LazyLock<Regex> = LazyLock::new(|| regex(r"(?:^|[^\p{L}\p{N}_])want=(\S+)"));
static REMOTE: LazyLock<Regex> = LazyLock::new(|| regex(r"(?:^|[^\p{L}\p{N}_])remote=(\S+)"));
static JOB_START: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^(\S+) start ([0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z)((?:[^\p{L}\p{N}_].*)?)$",
    )
});
static JOB_END: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^(\S+) (?:rc=[0-9]+ end|void[0-9]+:|skipped:|up failed|sidecar failed|no sidecar)")
});
static RELEASED: LazyLock<Regex> = LazyLock::new(|| regex(r"^RELEASED \S+ (\S+)"));
static HOST_FIELD: LazyLock<Regex> = LazyLock::new(|| regex(r"(?:^|[^\p{L}\p{N}_])host=(\S+)"));
static DOCKER_HOST: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?:^|[^\p{L}\p{N}_])DOCKER_HOST=ssh://(\S+)"));
static ISSUE_ARGUMENT: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?:^|\s)--issue\s+([0-9]{1,9})(?:$|[^\p{L}\p{N}_])"));

fn first_group(pattern: &Regex, text: &str) -> Option<String> {
    pattern
        .captures(text)
        .and_then(|found| found.get(1))
        .map(|found| found.as_str().to_owned())
}

/// What a run folder's queue log says about its holder and the last answers it got for a device.
#[derive(Debug, Default, PartialEq)]
pub struct QueueLogInfo {
    pub holder_pid: Option<i64>,
    pub want: Option<String>,
    pub busy_since: Option<String>,
    pub busy_reason: Option<String>,
    pub last_host: Option<String>,
}

/// What a run folder's `status` says: the job running now, the granted host, and whether the
/// queue ended.
#[derive(Debug, Default, PartialEq)]
pub struct StatusInfo {
    pub job: Option<String>,
    pub job_host: Option<String>,
    pub job_started: Option<String>,
    pub granted_host: Option<String>,
    pub finished_at: Option<String>,
}

/// The words that end the running job when a `status` line starts with one, followed by no word
/// character.
pub struct StopWords(Vec<String>);

impl StopWords {
    pub fn new(words: &[String]) -> Self {
        Self(words.to_vec())
    }

    fn matches(&self, line: &str) -> bool {
        self.0.iter().any(|word| {
            line.strip_prefix(word.as_str())
                .is_some_and(|rest| !rest.chars().next().is_some_and(is_word_char))
        })
    }
}

/// Finds the run folders `RUN_QUEUE_GLOB` matches that were written lately and reads their state;
/// a folder's parent is its packet.
pub struct RunQueueReader {
    pub folder_glob: String,
    pub holder_name: String,
    pub stop_words: StopWords,
    pub window_s: f64,
    pub log_name: String,
    pub worktree_root: String,
}

impl RunQueueReader {
    /// Every run folder written within the window, newest first.
    pub fn read_queues(&self, processes: &ProcessTable, now: f64) -> Vec<RunQueue> {
        let pattern = match self.folder_glob.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                format!("{}{rest}", std::env::var("HOME").unwrap_or_default())
            }
            _ => self.folder_glob.clone(),
        };
        let options = glob::MatchOptions {
            case_sensitive: true,
            require_literal_separator: true,
            require_literal_leading_dot: true,
        };
        let Ok(paths) = glob::glob_with(&pattern, options) else {
            return Vec::new();
        };
        let mut queues = Vec::new();
        for runs_path in paths.flatten() {
            if !runs_path.is_dir() {
                continue;
            }
            let updated = self.newest_mtime(&runs_path);
            if now - updated > self.window_s {
                continue;
            }
            queues.push(self.read_queue(&runs_path, processes, updated));
        }
        queues.sort_by(|a, b| b.updated_epoch.total_cmp(&a.updated_epoch));
        queues
    }

    /// One run folder's state: running, `waiting_device`, starting, finished or stopped.
    pub fn read_queue(&self, runs_path: &Path, processes: &ProcessTable, updated: f64) -> RunQueue {
        let queue_log = parse_queue_log(&read_text_or_empty(&runs_path.join(&self.log_name)));
        let status = parse_status(
            &read_text_or_empty(&runs_path.join("status")),
            Some(&self.stop_words),
        );
        let holder = queue_log.holder_pid.and_then(|pid| processes.get(&pid));
        let alive = holder.is_some_and(|holder| {
            self.holder_name.is_empty()
                || holder
                    .argv
                    .iter()
                    .take(3)
                    .any(|argument| argument.contains(&self.holder_name))
        });
        let mut issue = issue_from_path(&runs_path.to_string_lossy(), &self.worktree_root);
        if issue.is_none()
            && let Some(holder) = holder
        {
            issue = first_group(&ISSUE_ARGUMENT, &holder.argv.join(" "))
                .and_then(|number| number.parse().ok());
        }
        let state = if !alive {
            if status.finished_at.is_some() {
                "finished"
            } else {
                "stopped"
            }
        } else if status.job.is_some() {
            "running"
        } else if queue_log.busy_since.is_some() {
            "waiting_device"
        } else {
            "starting"
        };
        RunQueue {
            issue,
            holder_pid: queue_log.holder_pid,
            alive,
            state: state.to_owned(),
            want: queue_log.want,
            job: status.job,
            job_host: status.job_host,
            job_started: status.job_started,
            busy_since: queue_log.busy_since,
            busy_reason: queue_log.busy_reason,
            granted_host: status.granted_host.or(queue_log.last_host),
            updated_epoch: updated,
        }
    }

    fn newest_mtime(&self, runs_path: &Path) -> f64 {
        ["status", self.log_name.as_str()]
            .iter()
            .filter_map(|name| {
                std::fs::metadata(runs_path.join(name))
                    .ok()?
                    .modified()
                    .ok()
            })
            .filter_map(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|since| since.as_secs_f64())
            .fold(0.0, f64::max)
    }
}

/// The queue log's last holder and what it wants, the BUSY streak at its end (its first time and
/// the last reason), and the host of the last grant.
pub fn parse_queue_log(text: &str) -> QueueLogInfo {
    let mut info = QueueLogInfo::default();
    for line in split_lines(text) {
        if let Some(holder) = HOLDER.captures(line) {
            info.holder_pid = holder.get(1).and_then(|pid| pid.as_str().parse().ok());
            info.want = first_group(&WANT, line);
            info.busy_since = None;
            info.busy_reason = None;
            continue;
        }
        let Some(stamp) = STAMP.captures(line) else {
            continue;
        };
        let answer = stamp.get(2).map_or("", |answer| answer.as_str());
        if let Some(reason) = answer.strip_prefix("BUSY") {
            if info.busy_since.is_none() {
                info.busy_since = stamp.get(1).map(|time| time.as_str().to_owned());
            }
            info.busy_reason = Some(reason.trim().to_owned());
            continue;
        }
        info.busy_since = None;
        info.busy_reason = None;
        if answer.contains("ACQUIRED") {
            info.last_host =
                Some(first_group(&REMOTE, answer).unwrap_or_else(|| "local".to_owned()));
        }
    }
    info
}

/// `status`'s job running at its end (with its host and start), the last granted host, and when
/// the queue released, if its last word is a release.
pub fn parse_status(text: &str, stop_words: Option<&StopWords>) -> StatusInfo {
    let mut info = StatusInfo::default();
    for line in split_lines(text) {
        if line.starts_with("granted: ") {
            info.granted_host =
                Some(first_group(&DOCKER_HOST, line).unwrap_or_else(|| "local".to_owned()));
            info.finished_at = None;
            continue;
        }
        if let Some(released) = first_group(&RELEASED, line) {
            info.finished_at = Some(released);
            info.job = None;
            info.job_host = None;
            info.job_started = None;
            continue;
        }
        if let Some(start) = JOB_START.captures(line) {
            info.job = start.get(1).map(|job| job.as_str().to_owned());
            info.job_started = start.get(2).map(|time| time.as_str().to_owned());
            info.job_host = first_group(&HOST_FIELD, start.get(3).map_or("", |rest| rest.as_str()))
                .or_else(|| info.granted_host.clone());
            info.finished_at = None;
            continue;
        }
        let ended = first_group(&JOB_END, line).is_some_and(|job| Some(&job) == info.job.as_ref());
        if ended || stop_words.is_some_and(|words| words.matches(line)) {
            info.job = None;
            info.job_host = None;
            info.job_started = None;
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::models::ProcessInfo;
    use crate::dashboard::testkit::{FIXTURES, TempDir, copy_tree, set_mtime};

    fn queues_fixture(name: &str, file: &str) -> String {
        std::fs::read_to_string(FIXTURES.join("runqueues").join(name).join(file))
            .unwrap_or_default()
    }

    fn holder(pid: i64) -> (i64, ProcessInfo) {
        let argv = ["bash", "/h/bin/run_queue.sh", "--issue", "2502"]
            .map(str::to_owned)
            .to_vec();
        (
            pid,
            ProcessInfo {
                pid,
                ppid: 1,
                start_epoch: 0.0,
                argv,
            },
        )
    }

    fn reader(glob: String, holder: &str, log_name: &str) -> RunQueueReader {
        RunQueueReader {
            folder_glob: glob,
            holder_name: holder.to_owned(),
            stop_words: StopWords::new(&[]),
            window_s: 6.0 * 3600.0,
            log_name: log_name.to_owned(),
            worktree_root: String::new(),
        }
    }

    fn now() -> f64 {
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |since| since.as_secs_f64())
    }

    #[test]
    fn a_queue_running_a_job() {
        let lease = parse_queue_log(&queues_fixture("running", "queue.log"));
        assert_eq!(
            (
                lease.holder_pid,
                lease.want.as_deref(),
                lease.busy_since.as_deref(),
                lease.last_host.as_deref()
            ),
            (Some(4101), Some("any"), None, Some("ci@192.0.2.60"))
        );
        let status = parse_status(&queues_fixture("running", "status"), None);
        assert_eq!(
            (
                status.job.as_deref(),
                status.job_host.as_deref(),
                status.job_started.as_deref(),
                status.finished_at.as_deref()
            ),
            (
                Some("RS6-on-s1323-render"),
                Some("ci@192.0.2.60"),
                Some("2026-11-26T03:03:10Z"),
                None
            )
        );
    }

    #[test]
    fn a_queue_waiting_for_a_device_keeps_only_the_last_holders_busy_streak() {
        let lease = parse_queue_log(&queues_fixture("waiting", "queue.log"));
        assert_eq!(
            (lease.holder_pid, lease.want.as_deref()),
            (Some(4201), Some("ci@192.0.2.60"))
        );
        assert_eq!(
            (lease.busy_since.as_deref(), lease.busy_reason.as_deref()),
            (
                Some("2026-11-26T02:40:00Z"),
                Some("no remote card is free, alone and quiet")
            )
        );
        assert_eq!(
            parse_status(&queues_fixture("waiting", "status"), None).job,
            None
        );
    }

    #[test]
    fn a_stop_word_ends_the_running_job() {
        let text = "RS1 start 2026-11-26T02:47:25Z host=h\nNO-LEASE: lost the slot\n";
        assert_eq!(parse_status(text, None).job.as_deref(), Some("RS1"));
        let stop = StopWords::new(&["STOP".to_owned(), "NO-LEASE".to_owned()]);
        assert_eq!(parse_status(text, Some(&stop)).job, None);
        assert_eq!(
            parse_status("RS1 start 2026-11-26T02:47:25Z\nSTOPPED x\n", Some(&stop))
                .job
                .as_deref(),
            Some("RS1")
        );
    }

    #[test]
    fn a_released_queue_is_finished() {
        let status = parse_status(&queues_fixture("finished", "status"), None);
        assert_eq!(
            (
                status.job,
                status.finished_at.as_deref(),
                status.granted_host.as_deref()
            ),
            (
                None,
                Some("2026-11-26T02:51:16Z"),
                Some("ci@worker-b.local")
            )
        );
    }

    #[test]
    fn read_queues_names_state_issue_and_liveness() {
        let root = TempDir::new();
        let now = now();
        for (name, packet) in [
            ("running", "2026-11-26-2502-push"),
            ("waiting", "2026-11-26-2464-rest"),
            ("finished", "2026-11-26-2490-sub"),
        ] {
            let runs = root.path().join(packet).join("runs");
            copy_tree(&FIXTURES.join("runqueues").join(name), &runs);
            for file in ["status", "queue.log"] {
                set_mtime(&runs.join(file), now);
            }
        }
        let old = root.path().join("2026-11-20-2000-old").join("runs");
        copy_tree(&FIXTURES.join("runqueues").join("finished"), &old);
        for file in ["status", "queue.log"] {
            set_mtime(&old.join(file), now - 86400.0);
        }
        let processes: ProcessTable = [holder(4101), holder(4201)].into();
        let queues = reader(
            format!("{}/*/runs*", root.path().display()),
            "run_queue.sh",
            "queue.log",
        )
        .read_queues(&processes, now);
        let by_issue: std::collections::BTreeMap<i64, &RunQueue> = queues
            .iter()
            .filter_map(|queue| Some((queue.issue?, queue)))
            .collect();
        assert_eq!(
            by_issue.keys().copied().collect::<Vec<_>>(),
            [2464, 2490, 2502]
        );
        assert_eq!(
            (
                by_issue[&2502].state.as_str(),
                by_issue[&2502].job.as_deref()
            ),
            ("running", Some("RS6-on-s1323-render"))
        );
        assert_eq!(by_issue[&2464].state, "waiting_device");
        assert_eq!(
            (by_issue[&2490].state.as_str(), by_issue[&2490].alive),
            ("finished", false)
        );
    }

    #[test]
    fn the_queue_log_name_is_configurable() {
        let root = TempDir::new();
        let runs = root.path().join("2026-11-26-2502-push").join("runs");
        copy_tree(&FIXTURES.join("runqueues").join("running"), &runs);
        std::fs::rename(runs.join("queue.log"), runs.join("custom.log")).ok();
        let now = now();
        for file in ["status", "custom.log"] {
            set_mtime(&runs.join(file), now);
        }
        let argv = ["bash", "/h/bin/run_queue.sh"].map(str::to_owned).to_vec();
        let processes: ProcessTable = [(
            4101,
            ProcessInfo {
                pid: 4101,
                ppid: 1,
                start_epoch: 0.0,
                argv,
            },
        )]
        .into();
        let glob = format!("{}/*/runs", root.path().display());
        let default = reader(glob.clone(), "", "queue.log").read_queues(&processes, now);
        let custom = reader(glob, "", "custom.log").read_queues(&processes, now);
        assert_eq!((default[0].holder_pid, default[0].alive), (None, false));
        assert_eq!(
            (custom[0].holder_pid, custom[0].state.as_str()),
            (Some(4101), "running")
        );
    }
}
