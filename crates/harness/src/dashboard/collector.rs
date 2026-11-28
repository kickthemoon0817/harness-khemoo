//! Gathers the dashboard's data on a cadence per source; [`super::snapshot`] assembles them.
//!
//! The publisher collects the resource status, the process table and local state files at the
//! page refresh cadence. Local samples, SSH, GitHub, the queue count and fleet have refresh
//! threads of their own. Each snapshot carries their last results and ages. The only files
//! written are the dashboard's own, under `data_dir`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::buildlocks::BuildLockReader;
use super::command::{self, Environment, MAX_OUTPUT_BYTES, Output, RunError};
use super::files::{self, read_text, read_text_limited, read_text_or_empty};
use super::fleet::{self, FleetReport};
use super::flow::FlowBuilder;
use super::flowtrack::FlowTracker;
use super::github::{ClosedNode, GitHubClient, GitHubError};
use super::models::{
    BuildLane, ClaimRecord, HostHealthRow, LeaseStatus, MachineProbe, ProcessInfo, RunQueue,
};
use super::parsers::{self, model_label, parse_claim};
use super::procs::{ProcessScanner, ProcessTable, TickCandidate, map_pids_to_issues};
use super::queue::{IssueNode, IssueQueue, QueueLabels, parse_dependencies};
use super::runqueue::{RunQueueReader, StopWords};
use super::shlex;
use super::text::{basename, float_g, iso, optional_int, parse_iso, split_lines, tail_chars};

const LOAD_PROBE: &str = "printf 'load %s %s\\n' \"$(cut -d' ' -f1 /proc/loadavg)\" \"$(nproc)\"\n";
const MEMORY_PROBE: &str = "\nsed -n '/^MemTotal:/p;/^MemAvailable:/p' /proc/meminfo\n";
const DEVICE_REPORT: &str = "
echo '--- device'
printf '%s\\n' \"$device\" | head -1
echo '--- processes'
apps=$(printf '%s\\n' \"$device\" | tail -n +2)
[ -n \"$apps\" ] && printf '%s\\n' \"$apps\"
echo '--- cgroup'
for p in $(printf '%s\\n' \"$apps\" | cut -d, -f1); do printf '%s %s\\n' \"$p\" \"$(tr '\\n' ' ' < /proc/$p/cgroup 2>/dev/null)\"; done
";
const CONTAINER_PROBE: &str = "
echo '--- docker'
timeout 5 docker ps --format '{{.ID}}\\t{{.Names}}\\t{{.Status}}\\t{{.Image}}' 2>/dev/null
exit 0
";
const DOCKER_FORMAT: &str = "{{.ID}}\\t{{.Names}}\\t{{.Status}}\\t{{.Image}}";
const HISTORY_COLUMNS: [&str; 8] = [
    "time",
    "load1",
    "nproc",
    "mem_used_mb",
    "mem_total_mb",
    "gpu_util_pct",
    "gpu_mem_used_mb",
    "gpu_mem_total_mb",
];
pub const TAIL_BYTES: u64 = 65536;
/// A status document of hundreds of devices is well under a megabyte; the bound keeps a runaway
/// command from filling the dashboard's memory.
pub const FLEET_OUTPUT_MAX_BYTES: usize = 8 * 1024 * 1024;
const PID_LOOKUPS_PER_CYCLE: usize = 3;
const SMALL_FILE_BYTES: u64 = 1024 * 1024;

/// A URL carrying userinfo or a query string, where credentials ride.
static URL_WITH_CREDENTIALS: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"[A-Za-z][A-Za-z0-9+.-]*://(?:[^/\s@]*@|[^\s?#]*\?)")
        .expect("a constant pattern compiles")
});

/// Name parts that mark a secret, matched whole or as a part's end (`APIKEY`, `MYTOKEN`).
const SECRET_PART_ENDS: [&str; 18] = [
    "KEY",
    "AUTH",
    "PASS",
    "PASSWORD",
    "PASSWD",
    "PASSPHRASE",
    "COOKIE",
    "SESSION",
    "TOKEN",
    "SECRET",
    "CRED",
    "CREDS",
    "CREDENTIAL",
    "CREDENTIALS",
    "PAT",
    "DSN",
    "BEARER",
    "WEBHOOK",
];
/// Name parts that mark a secret at a part's start too (`SESSIONID`); short words such as `KEY`
/// and `PAT` only whole, so `KEYBOARD` and `PATH` stay.
const SECRET_PART_STARTS: [&str; 6] = [
    "TOKEN",
    "SECRET",
    "SESSION",
    "PASSWORD",
    "COOKIE",
    "CREDENTIAL",
];

/// True for a variable fleet never gets: it needs no credential of the dashboard's, and
/// `FLEET_STATUS_CMD` may be any command the instance names. `SSH_AUTH_SOCK` stays: an ssh
/// transport may authenticate through the agent.
fn secret_variable(name: &str, value: &str) -> bool {
    if name == "SSH_AUTH_SOCK" {
        return false;
    }
    let upper = name.to_ascii_uppercase();
    let secret_name = upper
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|part| {
            SECRET_PART_ENDS.iter().any(|word| part.ends_with(word))
                || SECRET_PART_STARTS.iter().any(|word| part.starts_with(word))
        });
    secret_name || URL_WITH_CREDENTIALS.is_match(value)
}

/// Where the dashboard reads from and writes to, how often each source refreshes, and the
/// instance's optional sources; each optional source left empty keeps its panel off the page.
///
/// `resource_status_command` is an argv printing the resource status format (`remote device: <h>
/// (slots a-b)`, `slot N free|HELD|ORPHAN ...`, `device ...`, `operator priority: ...`, `wait
/// ...`). `device_probe_command` is an argv run on each machine whose first line is `<name>, <util
/// %>, <memory used MiB>, <memory total MiB>` and whose further lines are `<pid>, <memory MiB>,
/// <process name>`. `queue_depth_command` is the launcher's queue count, an argv printing one
/// integer; empty runs `<harness_bin> claimable`. `worktree_root` is `WORKTREE_ROOT`, under which a
/// `<name>-<issue>` directory is that issue's worktree. `run_queue_glob` names run folders, each
/// with a `status` and a `run_queue_log`, live while a process whose first arguments hold
/// `run_queue_holder` holds them. `build_lock_local` and `build_lock_remote` (with `{name}` for
/// the host's `BUILD_LOCK_NAMES` name) are lock file names under `build_lock_dir`.
/// `fleet_status_command` is an argv printing `fleet status --json` or `fleet sessions --json`
/// (docs/FLEET.md); `fleet_environment` is set for it, as a tick's fleet gets it, so fleet finds
/// the instance's fleet.toml; a variable whose name looks secret is left out.
#[derive(Clone, Debug)]
pub struct Config {
    pub harness_home: PathBuf,
    pub data_dir: PathBuf,
    pub repo: Option<String>,
    pub harness_state: Option<PathBuf>,
    pub harness_bin: String,
    pub title: String,
    pub lane_model: String,
    /// The state directory as a tick's `HARNESS_TICK` may name it.
    pub tick_states: Vec<String>,
    /// harness.env's model list, usage and agent keys, which win over the environment's.
    pub usage_settings: Vec<(String, String)>,
    pub labels: QueueLabels,
    pub usage_cache: PathBuf,
    pub crontab_command: Vec<String>,
    pub history_hours: f64,
    pub github_interval_s: f64,
    pub local_interval_s: f64,
    pub remote_interval_s: f64,
    pub claimable_interval_s: f64,
    pub fast_ttl_s: f64,
    pub local_history_keep_rows: usize,
    pub stall_lines: usize,
    pub allocator_lines: usize,
    pub usage_max_age_s: f64,
    pub model_cooldown_s: f64,
    pub claim_grace_s: f64,
    pub command_timeout_s: f64,
    pub claimable_timeout_s: f64,
    pub ssh_connect_timeout_s: u64,
    pub remote_timeout_s: f64,
    pub host_health_stale_s: f64,
    pub dependency_ttl_s: f64,
    pub pid_lookup_retry_s: f64,
    pub claim_file_max_age_s: f64,
    pub resource_status_command: Vec<String>,
    pub device_probe_command: Vec<String>,
    pub device_probe_timeout_s: f64,
    pub queue_depth_command: Vec<String>,
    pub worktree_root: String,
    pub run_queue_glob: String,
    pub run_queue_log: String,
    pub run_queue_holder: String,
    pub run_queue_stop_words: Vec<String>,
    pub run_queue_window_s: f64,
    pub build_lock_names: Vec<(String, String)>,
    pub build_lock_dir: PathBuf,
    pub build_lock_local: String,
    pub build_lock_remote: String,
    pub done_window_s: f64,
    pub fleet_status_command: Vec<String>,
    pub fleet_environment: Vec<(String, String)>,
    pub fleet_interval_s: f64,
    pub fleet_timeout_s: f64,
    pub flow_animate_s: f64,
    pub activity_ttl_s: f64,
}

impl Config {
    /// A config with the dashboard's defaults and no optional source.
    pub fn new(harness_home: PathBuf, data_dir: PathBuf) -> Self {
        Self {
            harness_home,
            data_dir,
            repo: None,
            harness_state: None,
            harness_bin: "harness".to_owned(),
            title: "Harness status".to_owned(),
            lane_model: String::new(),
            tick_states: Vec::new(),
            usage_settings: Vec::new(),
            labels: QueueLabels::default(),
            usage_cache: PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".claude")
                .join("usage-cache.json"),
            crontab_command: vec!["crontab".to_owned(), "-l".to_owned()],
            history_hours: 6.0,
            github_interval_s: 60.0,
            local_interval_s: 30.0,
            remote_interval_s: 120.0,
            claimable_interval_s: 300.0,
            fast_ttl_s: 3.0,
            local_history_keep_rows: 1440,
            stall_lines: 10,
            allocator_lines: 12,
            usage_max_age_s: 1800.0,
            model_cooldown_s: 1800.0,
            claim_grace_s: 360.0,
            command_timeout_s: 8.0,
            claimable_timeout_s: 120.0,
            ssh_connect_timeout_s: 5,
            remote_timeout_s: 15.0,
            host_health_stale_s: 540.0,
            dependency_ttl_s: 600.0,
            pid_lookup_retry_s: 600.0,
            claim_file_max_age_s: 172_800.0,
            resource_status_command: Vec::new(),
            device_probe_command: Vec::new(),
            device_probe_timeout_s: 5.0,
            queue_depth_command: Vec::new(),
            worktree_root: String::new(),
            run_queue_glob: String::new(),
            run_queue_log: "queue.log".to_owned(),
            run_queue_holder: String::new(),
            run_queue_stop_words: Vec::new(),
            run_queue_window_s: 21_600.0,
            build_lock_names: Vec::new(),
            build_lock_dir: PathBuf::from("/tmp"),
            build_lock_local: String::new(),
            build_lock_remote: String::new(),
            done_window_s: 21_600.0,
            fleet_status_command: Vec::new(),
            fleet_environment: Vec::new(),
            fleet_interval_s: 30.0,
            fleet_timeout_s: 45.0,
            flow_animate_s: 20.0,
            activity_ttl_s: 10.0,
        }
    }

    /// The harness's state directory: `HARNESS_STATE`, else `<harness home>/state`.
    pub fn state_dir(&self) -> PathBuf {
        self.harness_state
            .clone()
            .unwrap_or_else(|| self.harness_home.join("state"))
    }

    /// True when the instance names a local or a remote build lock.
    pub fn build_locks_enabled(&self) -> bool {
        !self.build_lock_local.is_empty() || !self.build_lock_remote.is_empty()
    }
}

/// The last outcome of one data source: its data, when it was fetched and any error.
#[derive(Clone, Debug)]
pub struct Slot<T> {
    pub data: Option<T>,
    pub fetched_at: Option<f64>,
    pub error: Option<String>,
    pub duration_s: Option<f64>,
    pub attempted_at: Option<f64>,
}

impl<T> Slot<T> {
    pub fn fresh(data: T, started: f64, duration_s: Option<f64>) -> Self {
        Self {
            data: Some(data),
            fetched_at: Some(started),
            error: None,
            duration_s,
            attempted_at: Some(started),
        }
    }
}

/// A failed refresh keeps the source's last good data and its time.
fn fail_slot<T>(slot: &mut Option<Slot<T>>, error: String, attempted_at: f64) {
    let (data, fetched_at) = slot.take().map_or((None, None), |previous| {
        (previous.data, previous.fetched_at)
    });
    *slot = Some(Slot {
        data,
        fetched_at,
        error: Some(error),
        duration_s: None,
        attempted_at: Some(attempted_at),
    });
}

/// A source's age, error and duration, whatever its data.
#[derive(Clone, Debug, Default)]
pub struct Meta {
    pub fetched_at: Option<f64>,
    pub error: Option<String>,
    pub duration_s: Option<f64>,
}

fn meta_of<T>(slot: Option<&Slot<T>>) -> Option<Meta> {
    slot.map(|slot| Meta {
        fetched_at: slot.fetched_at,
        error: slot.error.clone(),
        duration_s: slot.duration_s,
    })
}

/// The dashboard's sources, by the names the page shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Procs,
    Lease,
    Local,
    Paused,
    Remote,
    Github,
    ClaimableSh,
    Fleet,
    Activity,
    LocalHistory,
    GithubCache,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Self::Procs => "procs",
            Self::Lease => "lease",
            Self::Local => "local",
            Self::Paused => "paused",
            Self::Remote => "remote",
            Self::Github => "github",
            Self::ClaimableSh => "claimable_sh",
            Self::Fleet => "fleet",
            Self::Activity => "activity",
            Self::LocalHistory => "local_history",
            Self::GithubCache => "github_cache",
        }
    }
}

/// Whether the harness is paused, and why.
#[derive(Clone, Debug, Default)]
pub struct Paused {
    pub paused: bool,
    pub reason: String,
}

/// The last GitHub read.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GithubPayload {
    #[serde(default)]
    pub issues: Vec<IssueNode>,
    #[serde(default)]
    pub first_only_issues: Vec<IssueNode>,
    #[serde(default)]
    pub dependency_states: BTreeMap<i64, String>,
    #[serde(default)]
    pub landings: Vec<Value>,
    #[serde(default)]
    pub closed_issues: Vec<ClosedNode>,
}

pub type Activity = (Vec<RunQueue>, Vec<BuildLane>);

#[derive(Default)]
pub struct Results {
    pub procs: Option<Slot<Vec<TickCandidate>>>,
    pub lease: Option<Slot<LeaseStatus>>,
    pub local: Option<Slot<MachineProbe>>,
    pub paused: Option<Slot<Paused>>,
    pub remote: Option<Slot<HashMap<String, Slot<MachineProbe>>>>,
    pub github: Option<Slot<GithubPayload>>,
    pub claimable_sh: Option<Slot<i64>>,
    pub fleet: Option<Slot<FleetReport>>,
    pub activity: Option<Slot<Activity>>,
    local_history: Option<Slot<()>>,
    github_cache: Option<Slot<()>>,
}

impl Results {
    pub fn fail(&mut self, source: Source, error: String, attempted_at: f64) {
        match source {
            Source::Procs => fail_slot(&mut self.procs, error, attempted_at),
            Source::Lease => fail_slot(&mut self.lease, error, attempted_at),
            Source::Local => fail_slot(&mut self.local, error, attempted_at),
            Source::Paused => fail_slot(&mut self.paused, error, attempted_at),
            Source::Remote => fail_slot(&mut self.remote, error, attempted_at),
            Source::Github => fail_slot(&mut self.github, error, attempted_at),
            Source::ClaimableSh => fail_slot(&mut self.claimable_sh, error, attempted_at),
            Source::Fleet => fail_slot(&mut self.fleet, error, attempted_at),
            Source::Activity => fail_slot(&mut self.activity, error, attempted_at),
            Source::LocalHistory => fail_slot(&mut self.local_history, error, attempted_at),
            Source::GithubCache => fail_slot(&mut self.github_cache, error, attempted_at),
        }
    }

    pub fn meta(&self, source: Source) -> Option<Meta> {
        match source {
            Source::Procs => meta_of(self.procs.as_ref()),
            Source::Lease => meta_of(self.lease.as_ref()),
            Source::Local => meta_of(self.local.as_ref()),
            Source::Paused => meta_of(self.paused.as_ref()),
            Source::Remote => meta_of(self.remote.as_ref()),
            Source::Github => meta_of(self.github.as_ref()),
            Source::ClaimableSh => meta_of(self.claimable_sh.as_ref()),
            Source::Fleet => meta_of(self.fleet.as_ref()),
            Source::Activity => meta_of(self.activity.as_ref()),
            Source::LocalHistory => meta_of(self.local_history.as_ref()),
            Source::GithubCache => meta_of(self.github_cache.as_ref()),
        }
    }
}

/// When the dashboard's own GitHub read was failing.
#[derive(Default)]
struct Outages {
    spans: Vec<(f64, Option<f64>)>,
    last_success: Option<f64>,
}

type HistoryRow = [Option<f64>; 8];
type HostHealthCache = Option<((SystemTime, u64), Arc<Vec<HostHealthRow>>)>;
type Refresh = fn(&Collector);

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn epoch_of(time: SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0.0, |since| since.as_secs_f64())
}

pub fn wall_clock() -> f64 {
    epoch_of(SystemTime::now())
}

fn seconds(value: f64) -> Duration {
    Duration::try_from_secs_f64(value.max(0.0)).unwrap_or(Duration::MAX)
}

/// An io error worded for the page: its kind, the message and the path.
pub fn describe_io(error: &io::Error, path: &Path) -> String {
    let kind = match error.kind() {
        io::ErrorKind::NotFound => "FileNotFoundError",
        io::ErrorKind::PermissionDenied => "PermissionError",
        _ => "OSError",
    };
    format!("{kind}: {error}: '{}'", path.display())
}

/// Keeps the last result of every source and builds snapshots from them.
pub struct Collector {
    pub config: Config,
    pub github: Option<GitHubClient>,
    pub clock: Box<dyn Fn() -> f64 + Send + Sync>,
    pub scanner: ProcessScanner,
    pub queue: IssueQueue,
    pub results: Mutex<Results>,
    outages: Mutex<Outages>,
    fast_lock: Mutex<()>,
    pub history: Mutex<Vec<HistoryRow>>,
    activity_lock: Mutex<()>,
    host_health_cache: Mutex<HostHealthCache>,
    dependency_cache: Mutex<HashMap<i64, (String, f64)>>,
    pub github_issue_by_pid: Mutex<HashMap<i64, i64>>,
    pid_lookup_attempts: Mutex<HashMap<i64, f64>>,
    pub processes: Mutex<Arc<ProcessTable>>,
    stopped: Mutex<bool>,
    stop_signal: Condvar,
    threads: Mutex<Vec<JoinHandle<()>>>,
    pub run_queue_reader: RunQueueReader,
    pub build_lock_reader: BuildLockReader,
    pub flow_builder: FlowBuilder,
    pub flow_tracker: FlowTracker,
    pub local_history_path: PathBuf,
    pub github_cache_path: PathBuf,
    pub remote_probe_script: String,
    /// Every command this collector ran, from any of its threads.
    #[cfg(test)]
    pub recorded: Mutex<Vec<Vec<String>>>,
    /// Every error it logged.
    #[cfg(test)]
    pub logged: Mutex<Vec<String>>,
}

impl Collector {
    pub fn new(
        config: Config,
        github: Option<GitHubClient>,
        clock: Box<dyn Fn() -> f64 + Send + Sync>,
    ) -> Self {
        let state_dir = config.state_dir();
        // ssh hands its command to the remote user's shell, so a remote probe cannot avoid one.
        // The argv is joined with each word quoted: that shell runs the same words this host runs,
        // and expands nothing in them.
        let remote_device = if config.device_probe_command.is_empty() {
            String::new()
        } else {
            format!(
                "device=$(timeout {} {} 2>/dev/null){DEVICE_REPORT}",
                float_g(config.device_probe_timeout_s),
                shlex::join(&config.device_probe_command)
            )
        };
        let names: HashMap<String, String> = config.build_lock_names.iter().cloned().collect();
        Self {
            github,
            clock,
            scanner: ProcessScanner::new(&state_dir),
            queue: IssueQueue {
                labels: config.labels.clone(),
            },
            results: Mutex::new(Results::default()),
            outages: Mutex::new(Outages::default()),
            fast_lock: Mutex::new(()),
            history: Mutex::new(Vec::new()),
            activity_lock: Mutex::new(()),
            host_health_cache: Mutex::new(None),
            dependency_cache: Mutex::new(HashMap::new()),
            github_issue_by_pid: Mutex::new(HashMap::new()),
            pid_lookup_attempts: Mutex::new(HashMap::new()),
            processes: Mutex::new(Arc::new(HashMap::new())),
            stopped: Mutex::new(false),
            stop_signal: Condvar::new(),
            threads: Mutex::new(Vec::new()),
            run_queue_reader: RunQueueReader {
                folder_glob: config.run_queue_glob.clone(),
                holder_name: config.run_queue_holder.clone(),
                stop_words: StopWords::new(&config.run_queue_stop_words),
                window_s: config.run_queue_window_s,
                log_name: config.run_queue_log.clone(),
                worktree_root: config.worktree_root.clone(),
            },
            build_lock_reader: BuildLockReader {
                lock_names: names.clone(),
                lock_dir: config.build_lock_dir.clone(),
                local_file: config.build_lock_local.clone(),
                remote_file: config.build_lock_remote.clone(),
                timeout: seconds(config.command_timeout_s),
                worktree_root: config.worktree_root.clone(),
            },
            flow_builder: FlowBuilder {
                labels: config.labels.clone(),
                device_tags: names,
                done_window_s: config.done_window_s,
                device_lanes: !config.resource_status_command.is_empty()
                    || !config.run_queue_glob.is_empty(),
                build_lanes: config.build_locks_enabled(),
            },
            flow_tracker: FlowTracker::new(
                config.data_dir.join("flow_state.json"),
                config.flow_animate_s,
            ),
            local_history_path: config.data_dir.join("local_history.tsv"),
            github_cache_path: config.data_dir.join("github_cache.json"),
            remote_probe_script: format!("{MEMORY_PROBE}{remote_device}{CONTAINER_PROBE}"),
            #[cfg(test)]
            recorded: Mutex::new(Vec::new()),
            #[cfg(test)]
            logged: Mutex::new(Vec::new()),
            config,
        }
    }

    pub fn now(&self) -> f64 {
        (self.clock)()
    }

    /// Creates the data directory, owner-only, loads the saved history and GitHub cache, starts
    /// the source threads, and publishes now and every `publish_interval_s`.
    pub fn start(
        self: &Arc<Self>,
        publish: Option<Box<dyn Fn() + Send>>,
        publish_interval_s: f64,
    ) -> Result<(), String> {
        files::prepare_private_dir(&self.config.data_dir)?;
        self.load_local_history_in_place();
        self.load_github_cache_in_place();
        self.flow_tracker.load();
        let mut sources: Vec<(Source, f64, Refresh)> = vec![
            (
                Source::Local,
                self.config.local_interval_s,
                Self::refresh_local_sample_in_place,
            ),
            (
                Source::Remote,
                self.config.remote_interval_s,
                Self::refresh_remote_probes_in_place,
            ),
            (
                Source::Github,
                self.config.github_interval_s,
                Self::refresh_github_in_place,
            ),
            (
                Source::ClaimableSh,
                self.config.claimable_interval_s,
                Self::refresh_claimable_in_place,
            ),
        ];
        if !self.config.fleet_status_command.is_empty() {
            sources.push((
                Source::Fleet,
                self.config.fleet_interval_s,
                Self::refresh_fleet_in_place,
            ));
        }
        let mut threads = lock(&self.threads);
        for (source, interval_s, refresh) in sources {
            if interval_s <= 0.0 {
                continue;
            }
            let collector = Arc::clone(self);
            threads.push(thread::spawn(move || {
                collector.run_periodically(source, interval_s, &|| refresh(&collector));
            }));
        }
        if let Some(publish) = publish {
            publish_guarded(&*publish);
            let collector = Arc::clone(self);
            threads.push(thread::spawn(move || {
                while !collector.wait_for_stop(publish_interval_s.max(1.0)) {
                    publish_guarded(&*publish);
                }
            }));
        }
        Ok(())
    }

    /// Asks the source threads to end and waits briefly for them.
    pub fn stop(&self) {
        *lock(&self.stopped) = true;
        self.stop_signal.notify_all();
        let deadline = Instant::now() + Duration::from_secs(2);
        let threads = std::mem::take(&mut *lock(&self.threads));
        while Instant::now() < deadline && !threads.iter().all(JoinHandle::is_finished) {
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits up to `seconds`; true once a stop was asked.
    fn wait_for_stop(&self, wait_s: f64) -> bool {
        let stopped = lock(&self.stopped);
        let (stopped, _) = self
            .stop_signal
            .wait_timeout_while(stopped, seconds(wait_s), |stopped| !*stopped)
            .unwrap_or_else(PoisonError::into_inner);
        *stopped
    }

    pub fn ask_stop(&self) {
        *lock(&self.stopped) = true;
        self.stop_signal.notify_all();
    }

    /// Calls a refresh at once and then every interval until stopped; a refresh that panics is
    /// logged and stored as the source's failure, and the loop goes on.
    pub fn run_periodically(&self, source: Source, interval_s: f64, refresh: &dyn Fn()) {
        while !*lock(&self.stopped) {
            let started = self.now();
            if let Err(panic) = catch_unwind(AssertUnwindSafe(refresh)) {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_owned())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown".to_owned());
                self.log_error(&format!(
                    "Dashboard refresh of {} failed: {message}",
                    source.name()
                ));
                self.store_failure(source, self.describe(&format!("panic: {message}")), started);
            }
            if self.wait_for_stop(interval_s) {
                break;
            }
        }
    }

    /// Runs one of the collector's commands; the tests read what it ran.
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn run(
        &self,
        argv: &[String],
        environment: Environment,
        timeout: Duration,
        max_stdout: usize,
    ) -> Result<Output, RunError> {
        #[cfg(test)]
        lock(&self.recorded).push(argv.to_vec());
        command::run(argv, environment, timeout, max_stdout)
    }

    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn log_error(&self, message: &str) {
        eprintln!("{message}");
        #[cfg(test)]
        lock(&self.logged).push(message.to_owned());
    }

    pub fn store_failure(&self, source: Source, error: String, attempted_at: f64) {
        lock(&self.results).fail(source, error, attempted_at);
    }

    /// An error message for the page: the token, the environment's GitHub tokens and every
    /// secret-looking value masked.
    pub fn describe(&self, message: &str) -> String {
        match &self.github {
            Some(github) => github.redact(message),
            None => super::github::scrub(&super::github::environment_secrets(), message),
        }
    }

    /// The end of a command's output for a message, masked before it is cut.
    fn output_tail(&self, text: &str, limit: usize) -> String {
        tail_chars(&self.describe(text.trim()), limit).to_owned()
    }

    /// Re-reads the resource status, when the instance has a command for it, and the process
    /// table, keeping the last good resource read on failure.
    pub fn refresh_fast_sources_in_place(&self) {
        if !self.config.resource_status_command.is_empty() {
            self.refresh_resource_status_in_place();
        }
        let started = self.now();
        let states = if self.config.tick_states.is_empty() {
            vec![self.config.state_dir().display().to_string()]
        } else {
            self.config.tick_states.clone()
        };
        let found = self.scanner.scan_processes().and_then(|processes| {
            let ticks = self.scanner.find_ticks(&processes, &states, self.now())?;
            Ok((processes, ticks))
        });
        match found {
            Ok((processes, ticks)) => {
                *lock(&self.processes) = Arc::new(processes);
                let duration = self.now() - started;
                lock(&self.results).procs = Some(Slot::fresh(ticks, started, Some(duration)));
            }
            Err(error) => self.store_failure(
                Source::Procs,
                self.describe(&describe_io(&error, Path::new("/proc"))),
                started,
            ),
        }
    }

    /// Refreshes the fast sources when due.
    pub fn refresh_fast_sources_when_due(&self) {
        let _fast = lock(&self.fast_lock);
        let attempted = lock(&self.results)
            .procs
            .as_ref()
            .and_then(|slot| slot.attempted_at);
        if attempted.is_none_or(|attempted| self.now() - attempted >= self.config.fast_ttl_s) {
            self.refresh_fast_sources_in_place();
        }
    }

    fn refresh_resource_status_in_place(&self) {
        let started = self.now();
        let environment = Environment::Inherit(vec![
            (
                "HARNESS_HOME".to_owned(),
                self.config.harness_home.display().to_string(),
            ),
            (
                "HARNESS_STATE".to_owned(),
                self.config.state_dir().display().to_string(),
            ),
        ]);
        let outcome = self
            .run(
                &self.config.resource_status_command,
                environment,
                seconds(self.config.command_timeout_s),
                MAX_OUTPUT_BYTES,
            )
            .map_err(|error| error.to_string())
            .and_then(|output| {
                if output.success() {
                    Ok(output)
                } else {
                    Err(format!(
                        "RuntimeError: resource status exit {}: {}",
                        output.status,
                        self.output_tail(&output.stderr, 200)
                    ))
                }
            });
        match outcome {
            Ok(output) => {
                let status = parsers::parse_lease_status(&output.stdout);
                let duration = self.now() - started;
                lock(&self.results).lease = Some(Slot::fresh(status, started, Some(duration)));
            }
            Err(error) => self.store_failure(Source::Lease, self.describe(&error), started),
        }
    }

    /// Samples this host (load, memory, device and its processes when probed, containers),
    /// appends it to the history file, and re-reads whether the harness is paused.
    pub fn refresh_local_sample_in_place(&self) {
        let started = self.now();
        let probe = parsers::parse_machine_probe(&self.local_probe_text());
        let duration = self.now() - started;
        self.append_local_history_in_place(started, &probe);
        lock(&self.results).local = Some(Slot::fresh(probe, started, Some(duration)));
        let paused = self.read_paused_state();
        lock(&self.results).paused = Some(Slot::fresh(paused, started, None));
    }

    /// This host's reading in the machine probe's format, read directly: /proc for load and
    /// memory, the device probe and `docker ps` as argvs.
    fn local_probe_text(&self) -> String {
        let mut text = String::new();
        let load = read_text_limited(Path::new("/proc/loadavg"), SMALL_FILE_BYTES)
            .ok()
            .and_then(|loadavg| loadavg.split(' ').next().map(str::to_owned))
            .unwrap_or_default();
        let cores = thread::available_parallelism().map_or(1, std::num::NonZero::get);
        writeln!(text, "load {load} {cores}").ok();
        if let Ok(meminfo) = read_text_limited(Path::new("/proc/meminfo"), SMALL_FILE_BYTES) {
            for line in meminfo
                .lines()
                .filter(|line| line.starts_with("MemTotal:") || line.starts_with("MemAvailable:"))
            {
                writeln!(text, "{line}").ok();
            }
        }
        if !self.config.device_probe_command.is_empty() {
            let device = self.run_device_probe();
            let mut lines = device.trim_end_matches('\n').split('\n');
            writeln!(
                text,
                "--- device\n{}\n--- processes",
                lines.next().unwrap_or_default()
            )
            .ok();
            let apps: Vec<&str> = lines.collect();
            for app in &apps {
                writeln!(text, "{app}").ok();
            }
            text.push_str("--- cgroup\n");
            for app in &apps {
                let pid = app.split(',').next().unwrap_or_default().trim();
                if let Some(pid) = optional_int(pid) {
                    let cgroup = read_text_limited(
                        Path::new(&format!("/proc/{pid}/cgroup")),
                        SMALL_FILE_BYTES,
                    )
                    .unwrap_or_default();
                    writeln!(text, "{pid} {}", cgroup.replace('\n', " ")).ok();
                }
            }
        }
        text.push_str("--- docker\n");
        let docker = ["docker", "ps", "--format", DOCKER_FORMAT].map(str::to_owned);
        if let Ok(output) = self.run(
            &docker,
            Environment::Inherit(Vec::new()),
            Duration::from_secs(5),
            MAX_OUTPUT_BYTES,
        ) {
            text.push_str(&output.stdout);
        }
        text
    }

    /// The device probe's stdout on this host, or an empty string when there is no probe or it
    /// fails or times out; the device row then shows no reading.
    pub fn run_device_probe(&self) -> String {
        if self.config.device_probe_command.is_empty() {
            return String::new();
        }
        self.run(
            &self.config.device_probe_command,
            Environment::Inherit(Vec::new()),
            seconds(self.config.device_probe_timeout_s),
            MAX_OUTPUT_BYTES,
        )
        .map(|output| output.stdout)
        .unwrap_or_default()
    }

    /// Probes each remote host once over ssh, read-only, all hosts at once.
    pub fn refresh_remote_probes_in_place(&self) {
        let started = self.now();
        let hosts = self.remote_hosts();
        let previous: HashMap<String, Slot<MachineProbe>> = lock(&self.results)
            .remote
            .as_ref()
            .and_then(|slot| slot.data.clone())
            .unwrap_or_default();
        let latest: HashMap<String, HostHealthRow> = self
            .read_host_health()
            .iter()
            .map(|row| (row.host.clone(), row.clone()))
            .collect();
        let failed = |host: &str, error: String, attempted_at: f64| {
            let previous = previous.get(host);
            Slot {
                data: previous.and_then(|slot| slot.data.clone()),
                fetched_at: previous.and_then(|slot| slot.fetched_at),
                error: Some(error),
                duration_s: None,
                attempted_at: Some(attempted_at),
            }
        };
        let (sender, receiver) = mpsc::channel();
        thread::scope(|scope| {
            for host in &hosts {
                let row = latest.get(host);
                let stale = row.is_none_or(|row| {
                    row.load1.is_none()
                        || row.nproc.is_none()
                        || started - row.epoch > self.config.host_health_stale_s
                });
                let script = if stale {
                    format!("{LOAD_PROBE}{}", self.remote_probe_script)
                } else {
                    self.remote_probe_script.clone()
                };
                let sender = sender.clone();
                let host = host.clone();
                scope.spawn(move || {
                    let host_started = self.now();
                    let outcome = self.probe_host(&host, &script).map(|probe| {
                        let duration = self.now() - host_started;
                        Slot::fresh(probe, host_started, Some(duration))
                    });
                    sender.send((host, outcome, host_started)).ok();
                });
            }
            drop(sender);
            let deadline = Instant::now() + seconds(self.config.remote_timeout_s + 5.0);
            let mut probes: HashMap<String, Slot<MachineProbe>> = HashMap::new();
            while probes.len() < hosts.len() {
                let Ok((host, outcome, host_started)) =
                    receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                else {
                    break;
                };
                let slot = outcome
                    .unwrap_or_else(|error| failed(&host, self.describe(&error), host_started));
                probes.insert(host, slot);
            }
            for host in &hosts {
                if !probes.contains_key(host) {
                    probes.insert(
                        host.clone(),
                        failed(host, "probe gave no result in time".to_owned(), started),
                    );
                }
            }
            let duration = self.now() - started;
            lock(&self.results).remote = Some(Slot::fresh(probes, started, Some(duration)));
        });
    }

    pub fn probe_host(&self, host: &str, script: &str) -> Result<MachineProbe, String> {
        // A host read from a state file must not be taken by ssh for an option.
        if host.starts_with('-') {
            return Err(format!("RuntimeError: refusing host {host:?}"));
        }
        let argv: Vec<String> = [
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            &format!("ConnectTimeout={}", self.config.ssh_connect_timeout_s),
            "-o",
            "ControlMaster=no",
            host,
            script,
        ]
        .iter()
        .map(|word| (*word).to_owned())
        .collect();
        let output = self
            .run(
                &argv,
                Environment::Inherit(Vec::new()),
                seconds(self.config.remote_timeout_s),
                MAX_OUTPUT_BYTES,
            )
            .map_err(|error| error.to_string())?;
        if !output.success() {
            return Err(format!(
                "RuntimeError: ssh exit {}: {}",
                output.status,
                self.output_tail(&output.stderr, 160)
            ));
        }
        Ok(parsers::parse_machine_probe(&output.stdout))
    }

    /// Reads the open issues, their dependencies' states, the last day's merges and any unmapped
    /// tick's issue.
    pub fn refresh_github_in_place(&self) {
        let started = self.now();
        let Some(github) = &self.github else {
            self.store_failure(Source::Github, "GH_REPO is not set".to_owned(), started);
            return;
        };
        match self.read_github(github, started) {
            Ok(payload) => {
                let duration = self.now() - started;
                self.save_github_cache(&payload, started);
                lock(&self.results).github = Some(Slot::fresh(payload, started, Some(duration)));
                self.record_github_outcome_in_place(started, true);
            }
            Err(error) => {
                self.store_failure(Source::Github, self.describe(&error.to_string()), started);
                self.record_github_outcome_in_place(started, false);
            }
        }
    }

    fn read_github(
        &self,
        github: &GitHubClient,
        started: f64,
    ) -> Result<GithubPayload, GitHubError> {
        let labels = &self.config.labels;
        let issues = github.fetch_open_issues(&labels.all_issues)?;
        let open_numbers: HashSet<i64> = issues.iter().map(|issue| issue.number).collect();
        let first_only_issues: Vec<IssueNode> = if labels.first == labels.all_issues {
            Vec::new()
        } else {
            github
                .fetch_open_issues(&labels.first)?
                .into_iter()
                .filter(|issue| !open_numbers.contains(&issue.number))
                .collect()
        };
        let mut wanted: Vec<i64> = issues
            .iter()
            .chain(&first_only_issues)
            .flat_map(|issue| parse_dependencies(issue.body.as_deref().unwrap_or_default()))
            .filter(|dependency| !open_numbers.contains(dependency))
            .collect();
        wanted.sort_unstable();
        wanted.dedup();
        let stale: Vec<i64> = {
            let cache = lock(&self.dependency_cache);
            wanted
                .iter()
                .copied()
                .filter(|dependency| {
                    started - cache.get(dependency).map_or(0.0, |(_, at)| *at)
                        >= self.config.dependency_ttl_s
                })
                .collect()
        };
        if !stale.is_empty() {
            let states = github.fetch_states(&stale)?;
            let mut cache = lock(&self.dependency_cache);
            for (number, state) in states {
                cache.insert(number, (state, started));
            }
        }
        let landings = github.fetch_merged_pulls(&iso(started - 86400.0))?;
        let closed_issues = github.fetch_closed_issues(
            &labels.all_issues,
            &iso(started - self.config.done_window_s),
        )?;
        self.look_up_unmapped_ticks_in_place(github, started)?;
        let dependency_states = {
            let cache = lock(&self.dependency_cache);
            wanted
                .iter()
                .filter_map(|number| Some((*number, cache.get(number)?.0.clone())))
                .collect()
        };
        Ok(GithubPayload {
            issues,
            first_only_issues,
            dependency_states,
            landings,
            closed_issues,
        })
    }

    /// Runs `QUEUE_DEPTH_CMD`, else `harness claimable`, for the queue label with the labels the
    /// launcher passes it, and keeps its count.
    pub fn refresh_claimable_in_place(&self) {
        let started = self.now();
        let Some(github) = &self.github else {
            self.store_failure(
                Source::ClaimableSh,
                "GH_REPO is not set".to_owned(),
                started,
            );
            return;
        };
        let labels = &self.config.labels;
        let home = self.config.harness_home.display().to_string();
        let environment = github.child_environment(&[
            ("GH_REPO", &github.repo),
            ("HARNESS_HOME", &home),
            ("ISSUE_LABEL", &labels.queue),
            ("WIP_LABEL", &labels.wip),
            ("SIGNOFF_LABEL", &labels.signoff),
            ("OPERATOR_LABEL", &labels.operator),
            ("READY_LABEL", &labels.ready),
            ("FIRST_LABEL", &labels.first),
            ("LOW_LABEL", &labels.low),
            ("LANE_LABEL", &labels.lane),
        ]);
        let argv = if self.config.queue_depth_command.is_empty() {
            vec![self.config.harness_bin.clone(), "claimable".to_owned()]
        } else {
            self.config.queue_depth_command.clone()
        };
        let outcome = self
            .run(
                &argv,
                Environment::Inherit(environment),
                seconds(self.config.claimable_timeout_s),
                MAX_OUTPUT_BYTES,
            )
            .map_err(|error| error.to_string())
            .and_then(|output: Output| {
                let printed = output.stdout.trim();
                if let Some(count) = optional_int(printed).filter(|_| output.success()) {
                    Ok(count)
                } else {
                    let detail = if output.stderr.is_empty() {
                        printed
                    } else {
                        output.stderr.as_str()
                    };
                    Err(format!(
                        "RuntimeError: queue count exit {}: {}",
                        output.status,
                        self.output_tail(detail, 200)
                    ))
                }
            });
        match outcome {
            Ok(count) => {
                let duration = self.now() - started;
                lock(&self.results).claimable_sh =
                    Some(Slot::fresh(count, started, Some(duration)));
            }
            Err(error) => self.store_failure(Source::ClaimableSh, self.describe(&error), started),
        }
    }

    /// Runs the fleet status command, keeping the last good read on failure. fleet exits 1 when
    /// any device failed, its document still complete, so the document decides whether the read
    /// worked.
    pub fn refresh_fleet_in_place(&self) {
        let started = self.now();
        let mut environment: Vec<(String, String)> = std::env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .filter(|(name, value)| !secret_variable(name, value))
            .collect();
        for (name, value) in &self.config.fleet_environment {
            environment.retain(|(existing, _)| existing != name);
            environment.push((name.clone(), value.clone()));
        }
        let outcome = self
            .run(
                &self.config.fleet_status_command,
                Environment::Exactly(environment),
                seconds(self.config.fleet_timeout_s),
                FLEET_OUTPUT_MAX_BYTES,
            )
            .map_err(|error: RunError| error.to_string())
            .and_then(|output| {
                fleet::parse(&output.stdout).map_err(|error| {
                    self.describe(&format!(
                        "RuntimeError: fleet exit {}: {error}; {}",
                        output.status,
                        self.output_tail(&output.stderr, 200)
                    ))
                })
            });
        match outcome {
            Ok(report) => {
                let duration = self.now() - started;
                lock(&self.results).fleet = Some(Slot::fresh(report, started, Some(duration)));
            }
            Err(error) => self.store_failure(Source::Fleet, self.describe(&error), started),
        }
    }

    /// Searches GitHub claim comments for live ticks old enough to have claimed but with no
    /// claim or lease.
    fn look_up_unmapped_ticks_in_place(
        &self,
        github: &GitHubClient,
        now: f64,
    ) -> Result<(), GitHubError> {
        let (candidates, lease_slots) = {
            let results = lock(&self.results);
            let candidates = results
                .procs
                .as_ref()
                .and_then(|slot| slot.data.clone())
                .unwrap_or_default();
            let lease_slots = results
                .lease
                .as_ref()
                .and_then(|slot| slot.data.as_ref())
                .map(LeaseStatus::slots_and_device)
                .unwrap_or_default();
            (candidates, lease_slots)
        };
        if candidates.is_empty() {
            return Ok(());
        }
        let mapped = map_pids_to_issues(&self.read_claims(now), &lease_slots, &HashMap::new());
        let mut lookups = 0;
        let mut live_pids = HashSet::new();
        for candidate in &candidates {
            let pid = candidate.process.pid;
            live_pids.insert(pid);
            if mapped.contains_key(&pid) || lock(&self.github_issue_by_pid).contains_key(&pid) {
                continue;
            }
            if now - candidate.process.start_epoch < self.config.claim_grace_s {
                continue;
            }
            if now
                - lock(&self.pid_lookup_attempts)
                    .get(&pid)
                    .copied()
                    .unwrap_or(0.0)
                < self.config.pid_lookup_retry_s
            {
                continue;
            }
            // Past the cap the loop goes on, so every live pid keeps its cached issue.
            if lookups >= PID_LOOKUPS_PER_CYCLE {
                continue;
            }
            lookups += 1;
            lock(&self.pid_lookup_attempts).insert(pid, now);
            if let Some(issue) = github.find_issue_for_pid(pid)? {
                lock(&self.github_issue_by_pid).insert(pid, issue);
            }
        }
        lock(&self.github_issue_by_pid).retain(|pid, _| live_pids.contains(pid));
        Ok(())
    }

    /// The remote hosts: the resource status's remote hosts, else the hosts `host_health.tsv`
    /// names, in the harness's order.
    pub fn remote_hosts(&self) -> Vec<String> {
        let from_lease: Vec<String> = lock(&self.results)
            .lease
            .as_ref()
            .and_then(|slot| slot.data.as_ref())
            .map(|status| {
                status
                    .remote_cards
                    .iter()
                    .map(|card| card.host.clone())
                    .collect()
            })
            .unwrap_or_default();
        let hosts = if from_lease.is_empty() {
            self.read_host_health()
                .iter()
                .map(|row| row.host.clone())
                .collect()
        } else {
            from_lease
        };
        let mut unique = Vec::new();
        for host in hosts {
            if !unique.contains(&host) {
                unique.push(host);
            }
        }
        unique
    }

    /// `host_health.tsv`'s rows, re-read only when it changed.
    pub fn read_host_health(&self) -> Arc<Vec<HostHealthRow>> {
        let path = self.config.state_dir().join("host_health.tsv");
        let Ok(metadata) = std::fs::metadata(&path) else {
            return Arc::new(Vec::new());
        };
        let signature = (metadata.modified().unwrap_or(UNIX_EPOCH), metadata.len());
        let mut cache = lock(&self.host_health_cache);
        if let Some((cached, rows)) = cache.as_ref()
            && *cached == signature
        {
            return Arc::clone(rows);
        }
        let rows = Arc::new(parsers::parse_host_health_tsv(&read_text_or_empty(&path)));
        *cache = Some((signature, Arc::clone(&rows)));
        rows
    }

    /// The claim files written recently enough to belong to a live tick.
    pub fn read_claims(&self, now: f64) -> Vec<ClaimRecord> {
        let Ok(entries) = std::fs::read_dir(self.config.state_dir().join("locks")) else {
            return Vec::new();
        };
        let mut claims = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("claim.") || name == "claim.guard" {
                continue;
            }
            let Ok(modified) =
                std::fs::metadata(entry.path()).and_then(|metadata| metadata.modified())
            else {
                continue;
            };
            if now - epoch_of(modified) > self.config.claim_file_max_age_s {
                continue;
            }
            let Ok(text) = read_text_limited(&entry.path(), SMALL_FILE_BYTES) else {
                continue;
            };
            claims.extend(parse_claim(&name, &text));
        }
        claims
    }

    /// Whether the harness is paused, by the launcher's rule: `state/paused` exists, or no
    /// uncommented crontab line runs bin/tick.sh.
    fn read_paused_state(&self) -> Paused {
        if self.config.state_dir().join("paused").exists() {
            return Paused {
                paused: true,
                reason: "state/paused present".to_owned(),
            };
        }
        let Ok(output) = self.run(
            &self.config.crontab_command,
            Environment::Inherit(Vec::new()),
            seconds(self.config.command_timeout_s),
            MAX_OUTPUT_BYTES,
        ) else {
            return Paused {
                paused: false,
                reason: "crontab unreadable".to_owned(),
            };
        };
        let active = split_lines(&output.stdout).iter().any(|line| {
            let line = line.trim();
            !line.is_empty() && !line.starts_with('#') && line.contains("bin/tick.sh")
        });
        if active {
            Paused::default()
        } else {
            Paused {
                paused: true,
                reason: "no active bin/tick.sh cron line".to_owned(),
            }
        }
    }

    /// A setting as the launcher hands it to `harness usage`: harness.env's, else the
    /// environment's; the model list is `state/tick-model` when that is set, the lane counts only
    /// while `LANE_LABEL` turns it on, and the state directory and the claude cache are the
    /// dashboard's own.
    pub fn usage_setting(&self, key: &str) -> Option<String> {
        match key {
            "HARNESS_STATE" => Some(self.config.state_dir().display().to_string()),
            "USAGE_CACHE" => Some(self.config.usage_cache.display().to_string()),
            "TICK_MODELS" => {
                let listed = read_text_or_empty(&self.config.state_dir().join("tick-model"));
                let listed = listed.split_whitespace().collect::<Vec<_>>().join(" ");
                if listed.is_empty() {
                    self.setting("TICK_MODELS")
                } else {
                    Some(listed)
                }
            }
            "LANE_MODEL" if self.config.labels.lane.is_empty() => None,
            _ => self.setting(key),
        }
    }

    /// harness.env's value of a model list, usage or agent key, else the environment's.
    fn setting(&self, key: &str) -> Option<String> {
        self.config
            .usage_settings
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var(key).ok())
            .filter(|value| !value.is_empty())
    }

    /// A model list, usage or agent key's value, or an empty string.
    pub fn setting_or_empty(&self, key: &str) -> String {
        self.setting(key).unwrap_or_default()
    }

    /// The model list, best first, each entry `agent:model`: `state/tick-model`, else
    /// `TICK_MODELS`, a bare model run by `TICK_AGENT`.
    pub fn read_tick_models(&self) -> Vec<String> {
        let text = self
            .usage_setting("TICK_MODELS")
            .unwrap_or_else(|| "claude-opus-5-5".to_owned());
        let agent = self
            .usage_setting("TICK_AGENT")
            .unwrap_or_else(|| "claude".to_owned());
        match crate::usage::Entry::list(&text, &agent) {
            Ok(entries) => entries.iter().map(ToString::to_string).collect(),
            Err(_) => text.split_whitespace().map(str::to_owned).collect(),
        }
    }

    /// Entries still cooling down after a usage limit, stamped `<agent>:<model>`: the entry, its
    /// agent and model, a label and the seconds left.
    pub fn cooling_models(&self, now: f64) -> Vec<Value> {
        let Ok(entries) = std::fs::read_dir(self.config.state_dir().join("model-cooldown")) else {
            return Vec::new();
        };
        let mut cooling: Vec<(String, f64)> = entries
            .flatten()
            .filter_map(|entry| {
                let modified = std::fs::metadata(entry.path())
                    .and_then(|metadata| metadata.modified())
                    .ok()?;
                let remaining = self.config.model_cooldown_s - (now - epoch_of(modified));
                (remaining > 0.0)
                    .then(|| (entry.file_name().to_string_lossy().into_owned(), remaining))
            })
            .collect();
        cooling.sort_by(|a, b| a.0.cmp(&b.0));
        #[allow(clippy::cast_possible_truncation)]
        cooling
            .into_iter()
            .map(|(entry, remaining)| {
                let (agent, model) = entry.split_once(':').unwrap_or(("", entry.as_str()));
                serde_json::json!({
                    "entry": entry, "agent": agent, "model": model, "remaining_s": remaining as i64,
                    "label": entry_label(&entry),
                })
            })
            .collect()
    }

    /// Adds one local sample to the in-memory history and the history file, trimming both.
    fn append_local_history_in_place(&self, epoch: f64, probe: &MachineProbe) {
        #[allow(clippy::cast_precision_loss)]
        let row: HistoryRow = [
            Some(epoch),
            probe.load1,
            probe
                .nproc
                .filter(|nproc| *nproc != 0)
                .map(|nproc| nproc as f64),
            probe.mem_used_mb,
            probe.mem_total_mb,
            probe.gpu.as_ref().and_then(|gpu| gpu.util_pct),
            probe.gpu.as_ref().and_then(|gpu| gpu.mem_used_mb),
            probe.gpu.as_ref().and_then(|gpu| gpu.mem_total_mb),
        ];
        let keep = self.config.local_history_keep_rows;
        let mut history = lock(&self.history);
        history.push(row);
        let excess = history.len().saturating_sub(keep);
        history.drain(..excess);
        let header = format!("{}\n", HISTORY_COLUMNS.join("\t"));
        let written = (|| -> io::Result<()> {
            std::fs::create_dir_all(&self.config.data_dir)?;
            if !self.local_history_path.exists() {
                files::write_replacing(&self.local_history_path, &header)?;
            }
            files::append(&self.local_history_path, &history_line(&row))?;
            #[allow(clippy::cast_precision_loss)]
            let limit = keep as f64 * 90.0 * 1.25;
            #[allow(clippy::cast_precision_loss)]
            if std::fs::metadata(&self.local_history_path)?.len() as f64 > limit {
                let body: String = history.iter().map(history_line).collect();
                files::write_replacing(&self.local_history_path, &format!("{header}{body}"))?;
            }
            Ok(())
        })();
        if let Err(error) = written {
            self.store_failure(
                Source::LocalHistory,
                describe_io(&error, &self.local_history_path),
                epoch,
            );
        }
    }

    /// Loads the saved local history so the sparklines survive a restart.
    pub fn load_local_history_in_place(&self) {
        let text = read_text_or_empty(&self.local_history_path);
        let mut rows: Vec<HistoryRow> = Vec::new();
        for line in split_lines(&text).into_iter().skip(1) {
            let columns: Vec<&str> = line.split('\t').collect();
            if columns.len() != HISTORY_COLUMNS.len() {
                continue;
            }
            let Some(epoch) = parse_iso(columns[0]) else {
                continue;
            };
            let mut row: HistoryRow = [None; 8];
            row[0] = Some(epoch);
            let mut valid = true;
            for (index, column) in columns.iter().enumerate().skip(1) {
                if column.is_empty() || *column == "-" {
                    continue;
                }
                match column.trim().parse() {
                    Ok(value) => row[index] = Some(value),
                    Err(_) => valid = false,
                }
            }
            if valid {
                rows.push(row);
            }
        }
        let excess = rows
            .len()
            .saturating_sub(self.config.local_history_keep_rows);
        rows.drain(..excess);
        *lock(&self.history) = rows;
    }

    /// Saves the last GitHub read so a restart serves it at once, marked with its age.
    fn save_github_cache(&self, payload: &GithubPayload, fetched_at: f64) {
        let document = serde_json::json!({"fetched_at": fetched_at, "payload": payload});
        let saved = std::fs::create_dir_all(&self.config.data_dir)
            .and_then(|()| files::write_replacing(&self.github_cache_path, &document.to_string()));
        if let Err(error) = saved {
            self.store_failure(
                Source::GithubCache,
                describe_io(&error, &self.github_cache_path),
                fetched_at,
            );
        }
    }

    /// Serves the saved GitHub read until the first fetch replaces it.
    pub fn load_github_cache_in_place(&self) {
        #[derive(Deserialize)]
        struct Cached {
            fetched_at: f64,
            payload: GithubPayload,
        }
        let Ok(text) = read_text(&self.github_cache_path) else {
            return;
        };
        let Ok(cached) = serde_json::from_str::<Cached>(&text) else {
            return;
        };
        lock(&self.results).github = Some(Slot::fresh(cached.payload, cached.fetched_at, None));
        lock(&self.outages).last_success = Some(cached.fetched_at);
    }

    /// Tracks when the dashboard's own GitHub read was failing. An outage runs from the last
    /// read that worked (or one cadence before the first failure, when none has) to the next read
    /// that works; while it lasts, a count another reader took from GitHub may be the queue
    /// count's fallback rather than a real count.
    fn record_github_outcome_in_place(&self, attempted_at: f64, succeeded: bool) {
        let mut outages = lock(&self.outages);
        let open = outages.spans.last().is_some_and(|(_, end)| end.is_none());
        if succeeded {
            if open && let Some(last) = outages.spans.last_mut() {
                last.1 = Some(attempted_at);
            }
            outages.last_success = Some(attempted_at);
        } else if !open {
            let start = outages
                .last_success
                .unwrap_or(attempted_at - self.config.github_interval_s);
            outages.spans.push((start, None));
            let excess = outages.spans.len().saturating_sub(20);
            outages.spans.drain(..excess);
        }
    }

    /// True when the dashboard's own GitHub read was failing at that time.
    pub fn during_github_outage(&self, epoch: f64, now: f64) -> bool {
        lock(&self.outages)
            .spans
            .iter()
            .any(|(start, end)| *start < epoch && epoch < end.unwrap_or(now + 1.0))
    }

    /// The last five outages.
    pub fn github_outages(&self) -> Vec<(f64, Option<f64>)> {
        let outages = lock(&self.outages);
        let skip = outages.spans.len().saturating_sub(5);
        outages.spans[skip..].to_vec()
    }

    /// The run queues and compile lanes the instance configures, read at most every
    /// `activity_ttl_s`, keeping the last good read.
    pub fn activity(&self, issue_by_tick_pid: &HashMap<i64, i64>, now: f64) -> Activity {
        if self.config.run_queue_glob.is_empty() && !self.config.build_locks_enabled() {
            return (Vec::new(), Vec::new());
        }
        let _activity = lock(&self.activity_lock);
        if let Some(slot) = lock(&self.results).activity.as_ref()
            && let (Some(attempted), Some(data)) = (slot.attempted_at, slot.data.as_ref())
            && now - attempted < self.config.activity_ttl_s
        {
            return data.clone();
        }
        let started = self.now();
        let processes = Arc::clone(&lock(&self.processes));
        let run_queues = if self.config.run_queue_glob.is_empty() {
            Vec::new()
        } else {
            self.run_queue_reader.read_queues(&processes, now)
        };
        let lanes = if self.config.build_locks_enabled() {
            match self.build_lock_reader.read_lanes(
                &self.remote_hosts(),
                &processes,
                issue_by_tick_pid,
            ) {
                Ok(lanes) => lanes,
                Err(error) => {
                    let mut results = lock(&self.results);
                    results.fail(Source::Activity, self.describe(&error.to_string()), started);
                    let kept = results
                        .activity
                        .as_ref()
                        .and_then(|slot| slot.data.as_ref())
                        .map(|(_, lanes)| lanes.clone())
                        .unwrap_or_default();
                    return (run_queues, kept);
                }
            }
        } else {
            Vec::new()
        };
        let duration = self.now() - started;
        let mut slot = Slot::fresh((run_queues.clone(), lanes.clone()), started, Some(duration));
        slot.attempted_at = Some(now);
        lock(&self.results).activity = Some(slot);
        (run_queues, lanes)
    }

    pub fn process_table(&self) -> Arc<ProcessTable> {
        Arc::clone(&lock(&self.processes))
    }

    pub fn process(&self, pid: i64) -> Option<ProcessInfo> {
        lock(&self.processes).get(&pid).cloned()
    }

    pub fn device_probe_basename(&self) -> &str {
        self.config
            .device_probe_command
            .first()
            .map_or("", |program| basename(program))
    }
}

/// An `agent:model` entry for people: `claude Opus 5.5`; a bare model reads as its label.
pub fn entry_label(entry: &str) -> String {
    match entry.split_once(':') {
        Some((agent, model)) => format!("{agent} {}", model_label(model)),
        None => model_label(entry),
    }
}

fn publish_guarded(publish: &dyn Fn()) {
    if catch_unwind(AssertUnwindSafe(publish)).is_err() {
        eprintln!("Dashboard publication failed");
    }
}

/// One local history row as a tab-separated line.
fn history_line(row: &HistoryRow) -> String {
    let mut columns = vec![row[0].map_or_else(|| "-".to_owned(), iso)];
    for value in &row[1..] {
        columns.push(match value {
            None => "-".to_owned(),
            Some(value) => {
                let fixed = format!("{value:.2}");
                fixed.trim_end_matches('0').trim_end_matches('.').to_owned()
            }
        });
    }
    format!("{}\n", columns.join("\t"))
}

#[cfg(test)]
mod tests {
    use super::secret_variable;

    #[test]
    fn secret_looking_variables_are_kept_from_fleet() {
        for name in [
            "OPENAI_KEY",
            "GITHUB_PAT",
            "NPM_AUTH",
            "SENTRY_DSN",
            "my_api_token",
            "DB_PASSWORD",
            "SESSIONID",
            "OAUTH",
        ] {
            assert!(secret_variable(name, "x"), "{name}");
        }
        for name in ["PATH", "KEYBOARD", "HOME", "SSH_AUTH_SOCK", "LANG"] {
            assert!(!secret_variable(name, "x"), "{name}");
        }
        assert!(secret_variable(
            "DATABASE_URL",
            "postgres://user:pw@db.example.com/app"
        ));
        assert!(!secret_variable("SITE", "https://example.com/a:b"));
        assert!(secret_variable("KUBE_BEARER", "x"));
        assert!(secret_variable(
            "SLACK_WEBHOOK_URL",
            "https://hooks.slack.com/services/T/B/x"
        ));
        assert!(secret_variable("PUSH", "https://user@example.com/hook"));
        assert!(secret_variable("PUSH", "https://example.com/hook?sig=abc"));
    }
}
