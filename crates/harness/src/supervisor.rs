//! `harness supervisor`: when the launcher starts a supervisor tick, and what it records of each run.
//!
//! The supervisor is a tick of its own kind that reads `harness health` and may apply a directive
//! (`harness directive`). It runs when an event happened since its last run, or when
//! `SUPERVISOR_INTERVAL_S` has passed; never sooner than `SUPERVISOR_MIN_INTERVAL_S` after its last
//! start, never while a failed run's backoff lasts (the minimum interval, doubled for each failure in
//! a row), and only while its agent has room for one more tick. Everything it remembers is a plain
//! file under `<state>/supervisor/`, so a human can read and reset it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};

use crate::dashboard::files;
use crate::dashboard::text::{iso, parse_iso};
use crate::home::harness_home;
use crate::operator::{Authors, OperatorComment, operator_comments};
use crate::usage::{self, Entry, Lookup};

/// A failed run waits the minimum interval doubled this many times at most.
const MAX_BACKOFF_DOUBLINGS: u32 = 6;
/// A directive this close to its expiry is an event, so the supervisor can renew or let it go.
pub const EXPIRING_S: u64 = 600;
/// An issue claimed by this many ticks within [`LOOP_WINDOW_S`] is looping.
pub const LOOP_CLAIMS: usize = 3;
pub const LOOP_WINDOW_S: u64 = 6 * 3600;
/// What is read of the end of one tick log, for its exit trailer.
const TRAILER_TAIL: u64 = 4096;
/// What is read of the end of `claims.log`.
const CLAIMS_TAIL: u64 = 64 * 1024;
/// The runs log keeps this many lines.
const RUNS_KEPT: usize = 100;
const SMALL_FILE: u64 = 4096;
/// The marks of notes shown and stops sent are kept this long.
const MARKS_KEPT_S: u64 = 2 * 86_400;
/// The operators' comments kept for `harness health`.
const OPERATOR_COMMENTS_KEPT: usize = 10;

/// Where things are: the environment's value wins over harness.env's, as the scripts read them.
const PLACE_KEYS: [&str; 5] = [
    "HARNESS_STATE",
    "GH_REPO",
    "WORKTREE_ROOT",
    "LONG_LABEL",
    "HOLD_TIMEOUT_S",
];

/// The rails: what the supervisor and its directive may do. Read from harness.env only, never from
/// the environment, which a process of this user could set to loosen them.
const RAIL_KEYS: [&str; 22] = [
    "SUPERVISOR_MAX_PARKS_PER_DAY",
    "OPERATOR_LOGINS",
    "ISSUE_LABEL",
    "OPERATOR_LABEL",
    "FIRST_LABEL",
    "LOW_LABEL",
    "LANE_LABEL",
    "WIP_LABEL",
    "TICK_MODELS",
    "TICK_AGENT",
    "MAX_SLOTS",
    "TICK_WIDTH",
    "MODEL_COOLDOWN_S",
    "TICK_BUDGET_MIN",
    "LONG_TICK_BUDGET_MIN",
    "SUPERVISOR",
    "SUPERVISOR_MODEL",
    "SUPERVISOR_BUDGET_MIN",
    "SUPERVISOR_INTERVAL_S",
    "SUPERVISOR_MIN_INTERVAL_S",
    "SUPERVISOR_MAX_DIRECTIVE_S",
    "SUPERVISOR_STOP_GRACE_S",
];

/// A key as the supervisor's and the directive's commands read it: a rail ([`RAIL_KEYS`]) from
/// harness.env only; a place ([`PLACE_KEYS`]) from the environment, else harness.env; anything else,
/// the usage keys among them, from the environment, as `harness usage` reads it. harness.env is
/// sourced, not exported, so a supervisor tick has none of it in its environment.
pub fn config_lookup() -> impl Fn(&str) -> Option<String> {
    let file = harness_home()
        .map(|home| {
            crate::dashboard::env::read_keys(
                &crate::dashboard::env_path(&home, &crate::dashboard::env_nonempty),
                |key| RAIL_KEYS.contains(&key) || PLACE_KEYS.contains(&key),
            )
        })
        .unwrap_or_default();
    move |key: &str| {
        let from_file = || file.get(key).cloned().filter(|value| !value.is_empty());
        if RAIL_KEYS.contains(&key) {
            return from_file();
        }
        std::env::var(key)
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| PLACE_KEYS.contains(&key).then(from_file).flatten())
    }
}

#[derive(clap::Args)]
#[command(after_help = "\
Reads the SUPERVISOR_* keys, TICK_MODELS, TICK_AGENT, the label keys and OPERATOR_LOGINS from
harness.env only, GH_REPO and HARNESS_STATE from the environment, else harness.env, the usage keys
from the environment, and its own state from $HARNESS_STATE/supervisor/.")]
pub struct Args {
    #[command(subcommand)]
    command: Sub,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Exit 0 and print `due <events>` when a supervisor tick should start now, else exit 1 and say why not.
    Due,
    /// Record the start of a run.
    Begin {
        /// What made it due, as `due` printed it.
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Apply the last JSON answer of the supervisor's output: its directive and its GitHub actions.
    Answer { output: PathBuf },
    /// Record the end of a run.
    End {
        #[arg(long)]
        exit: i32,
        #[arg(long)]
        duration: u64,
    },
}

pub fn run(args: &Args) -> ExitCode {
    let env = config_lookup();
    let result = Settings::from_env(&env).and_then(|settings| {
        let paths = Paths::from_env(&env)?;
        match &args.command {
            Sub::Due => due_command(&env, &settings, &paths),
            Sub::Begin { reason } => begin(&env, &paths, reason).map(|()| ExitCode::SUCCESS),
            Sub::Answer { output } => crate::answer::run(&env, &paths, output).map(ExitCode::from),
            Sub::End { exit, duration } => {
                end(&paths, *exit, *duration, now_s()).map(|()| ExitCode::SUCCESS)
            }
        }
    });
    result.unwrap_or_else(|message| {
        eprintln!("harness supervisor: {message}");
        ExitCode::from(2)
    })
}

pub fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// An ISO time as Unix seconds; a time before the epoch is refused.
pub fn epoch_of(text: &str) -> Option<u64> {
    let seconds = parse_iso(text)?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (seconds >= 0.0).then(|| seconds.floor() as u64)
}

#[allow(clippy::cast_precision_loss)]
pub fn iso_of(epoch: u64) -> String {
    iso(epoch as f64)
}

fn number(env: Lookup, key: &str, default: u64) -> Result<u64, String> {
    env(key).map_or(Ok(default), |text| {
        text.trim()
            .parse()
            .map_err(|_| format!("{key}: '{text}' is not a whole number"))
    })
}

/// The model list the configuration allows: `<state>/tick-model` when it holds any, else
/// `TICK_MODELS`, a bare model run by `TICK_AGENT`. A directive may only narrow or reorder it.
pub fn configured_models(env: Lookup, state: &Path) -> Result<Vec<Entry>, String> {
    let agent = env("TICK_AGENT").unwrap_or_else(|| "claude".to_owned());
    let listed = files::read_text_limited(&state.join("tick-model"), SMALL_FILE)
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let text = if listed.is_empty() {
        env("TICK_MODELS").unwrap_or_else(|| "claude-opus-5-5".to_owned())
    } else {
        listed
    };
    Entry::list(&text, &agent).map_err(|message| format!("TICK_MODELS: {message}"))
}

/// The supervisor's settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub budget_min: u64,
    pub interval_s: u64,
    pub min_interval_s: u64,
    pub max_directive_s: u64,
    pub stop_grace_s: u64,
    /// The longest a tick may run before it counts as stalled.
    pub stall_after_s: u64,
}

impl Settings {
    pub fn from_env(env: Lookup) -> Result<Self, String> {
        let settings = Self {
            budget_min: number(env, "SUPERVISOR_BUDGET_MIN", 10)?,
            interval_s: number(env, "SUPERVISOR_INTERVAL_S", 1800)?,
            min_interval_s: number(env, "SUPERVISOR_MIN_INTERVAL_S", 600)?,
            max_directive_s: number(env, "SUPERVISOR_MAX_DIRECTIVE_S", 6 * 3600)?,
            stop_grace_s: number(env, "SUPERVISOR_STOP_GRACE_S", 1200)?,
            stall_after_s: number(env, "LONG_TICK_BUDGET_MIN", 240)?.saturating_mul(60),
        };
        if settings.budget_min == 0 {
            return Err("SUPERVISOR_BUDGET_MIN: must be at least 1".to_owned());
        }
        Ok(settings)
    }
}

/// `SUPERVISOR_MODEL`, else the last entry of the configured model list: the cheapest, by the
/// list's order.
pub fn supervisor_entry(env: Lookup, models: &[Entry]) -> Result<Entry, String> {
    let agent = env("TICK_AGENT").unwrap_or_else(|| "claude".to_owned());
    match env("SUPERVISOR_MODEL") {
        Some(text) => Entry::parse(text.trim(), &agent)
            .map_err(|message| format!("SUPERVISOR_MODEL: {message}")),
        None => models
            .last()
            .cloned()
            .ok_or_else(|| "TICK_MODELS: no entry".to_owned()),
    }
}

/// Where the supervisor and the directive keep their files.
#[derive(Clone, Debug)]
pub struct Paths {
    pub state: PathBuf,
}

impl Paths {
    pub fn from_env(env: Lookup) -> Result<Self, String> {
        let state = env("HARNESS_STATE")
            .map(PathBuf::from)
            .or_else(|| harness_home().map(|home| home.join("state")))
            .ok_or("no state directory: set HARNESS_STATE")?;
        Ok(Self { state })
    }

    pub fn dir(&self) -> PathBuf {
        self.state.join("supervisor")
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.dir().join(name)
    }

    pub fn directive(&self) -> PathBuf {
        self.state.join("directive.json")
    }

    pub fn directive_log(&self) -> PathBuf {
        self.state.join("logs").join("directive.log")
    }

    /// Creates the supervisor's directory, owner-only.
    pub fn prepare(&self) -> Result<(), String> {
        files::prepare_private_dir(&self.dir())
    }
}

/// A small plain file's first word as a number.
pub fn read_number(path: &Path) -> Option<u64> {
    files::read_text_limited(path, SMALL_FILE)
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn read_line(path: &Path) -> Option<String> {
    let text = files::read_text_limited(path, SMALL_FILE).ok()?;
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_owned())
}

fn write_value(path: &Path, value: &str) -> Result<(), String> {
    files::write_replacing(path, &format!("{value}\n"))
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn modified_s(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

/// The end of a file, at most `bytes` of it.
fn read_tail(path: &Path, bytes: u64) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let Ok(size) = file.seek(SeekFrom::End(0)) else {
        return String::new();
    };
    if file
        .seek(SeekFrom::Start(size.saturating_sub(bytes)))
        .is_err()
    {
        return String::new();
    }
    let mut data = Vec::new();
    if file.take(bytes).read_to_end(&mut data).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&data).into_owned()
}

/// What the supervisor remembers of its runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunState {
    pub last_start: Option<u64>,
    pub last_end: Option<u64>,
    pub last_exit: Option<i64>,
    pub failures: u32,
}

impl RunState {
    pub fn read(paths: &Paths) -> Self {
        Self {
            last_start: read_number(&paths.file("last_start")),
            last_end: read_number(&paths.file("last_end")),
            last_exit: read_line(&paths.file("last_exit")).and_then(|text| text.parse().ok()),
            failures: read_number(&paths.file("failures"))
                .and_then(|count| u32::try_from(count).ok())
                .unwrap_or(0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// A tick ended with a non-zero exit.
    TickFailed,
    /// A model hit its usage limit.
    Limit,
    /// A usage window reset.
    Reset,
    /// The claimable count changed.
    Claimable,
    /// A tick ran past the longest budget, or the instance logged a stall.
    Stall,
    /// One issue was claimed again and again.
    Loop,
    /// An Operator comment landed.
    Operator,
    /// The human removed a pause.
    Unpaused,
    /// The directive expires soon.
    Expiring,
}

impl EventKind {
    /// The events that end a directive's pause early: only a person's.
    pub fn ends_pause(self) -> bool {
        matches!(self, Self::Operator | Self::Unpaused)
    }

    /// The events that end the quiet before a directive's `wake_at`.
    pub fn ends_wake(self) -> bool {
        matches!(self, Self::Reset | Self::Operator | Self::Unpaused)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::TickFailed => "tick-failed",
            Self::Limit => "limit",
            Self::Reset => "reset",
            Self::Claimable => "claimable",
            Self::Stall => "stall",
            Self::Loop => "loop",
            Self::Operator => "operator",
            Self::Unpaused => "unpaused",
            Self::Expiring => "expiring",
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub kind: EventKind,
    pub at: u64,
}

/// One claim from `claims.log`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub at: u64,
    pub issue: u64,
    pub pid: u64,
}

/// What the events are read from, gathered from the state directory.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inputs {
    /// Each tick log's last modification and the exit its last trailer names.
    pub tick_ends: Vec<(u64, i64)>,
    /// When each cooling entry hit its limit.
    pub limits: Vec<u64>,
    /// The earliest window reset known at the last run's start.
    pub next_reset: Option<u64>,
    /// The queue: claimable issues and issues a live tick holds, now and at the last run's start.
    /// A claim moves an issue from one to the other, so only new, finished or parked work changes it.
    pub queue_now: Option<u64>,
    pub queue_at_run: Option<u64>,
    /// Each live tick's age, supervisors left out.
    pub tick_ages: Vec<u64>,
    /// When each looping issue's deciding claim was made ([`loops`]).
    pub loops: Vec<u64>,
    pub operator: Vec<u64>,
    pub unpaused: Option<u64>,
    /// When the instance's stall log last changed.
    pub stall_log: Option<u64>,
    pub directive_expires: Option<u64>,
}

/// The events of `inputs` after `since`, up to `now`, oldest first.
pub fn events(inputs: &Inputs, settings: &Settings, since: u64, now: u64) -> Vec<Event> {
    let after = |at: u64| at > since && at <= now;
    let mut found = Vec::new();
    let mut push = |kind: EventKind, at: u64| {
        if after(at) {
            found.push(Event { kind, at });
        }
    };
    for (at, exit) in &inputs.tick_ends {
        if *exit != 0 {
            push(EventKind::TickFailed, *at);
        }
    }
    for at in &inputs.limits {
        push(EventKind::Limit, *at);
    }
    if let Some(at) = inputs.next_reset {
        push(EventKind::Reset, at);
    }
    if let (Some(current), Some(before)) = (inputs.queue_now, inputs.queue_at_run)
        && current != before
    {
        push(EventKind::Claimable, now);
    }
    for age in &inputs.tick_ages {
        if *age > settings.stall_after_s {
            push(EventKind::Stall, now - age + settings.stall_after_s);
        }
    }
    if let Some(at) = inputs.stall_log {
        push(EventKind::Stall, at);
    }
    for at in &inputs.loops {
        push(EventKind::Loop, *at);
    }
    for at in &inputs.operator {
        push(EventKind::Operator, *at);
    }
    if let Some(at) = inputs.unpaused {
        push(EventKind::Unpaused, at);
    }
    if let Some(expires) = inputs.directive_expires {
        push(EventKind::Expiring, expires.saturating_sub(EXPIRING_S));
    }
    found.sort_by_key(|event| (event.at, event.kind));
    found
}

/// Each issue claimed by [`LOOP_CLAIMS`] or more distinct ticks within [`LOOP_WINDOW_S`] before
/// `now`, with its first claim of the window and the claim that made the count.
pub fn loop_candidates(claims: &[Claim], now: u64) -> BTreeMap<u64, (u64, u64)> {
    let mut by_issue: BTreeMap<u64, Vec<&Claim>> = BTreeMap::new();
    for claim in claims {
        if claim.at + LOOP_WINDOW_S >= now && claim.at <= now {
            by_issue.entry(claim.issue).or_default().push(claim);
        }
    }
    let mut candidates = BTreeMap::new();
    for (issue, mut list) in by_issue {
        list.sort_by_key(|claim| claim.at);
        let mut pids = BTreeSet::new();
        for claim in &list {
            pids.insert(claim.pid);
            if pids.len() >= LOOP_CLAIMS {
                candidates.insert(issue, (list[0].at, claim.at));
                break;
            }
        }
    }
    candidates
}

/// The looping issues, each with its deciding claim's time: candidates ([`loop_candidates`]) that
/// showed no progress between their first and their deciding claim. `progress` holds, for each
/// issue whose progress could be read, the times of its label changes (the claim label's left
/// out), of PRs that name it and of commits that reference it. Three claims with progress between
/// them are the work, review and landing cycle, not a loop; one whose progress is unknown is none.
pub fn loops(
    candidates: &BTreeMap<u64, (u64, u64)>,
    progress: &BTreeMap<u64, Vec<u64>>,
) -> BTreeMap<u64, u64> {
    candidates
        .iter()
        .filter(|(issue, (first, last))| {
            progress
                .get(issue)
                .is_some_and(|times| !times.iter().any(|at| at > first && at <= last))
        })
        .map(|(issue, (_, last))| (*issue, *last))
        .collect()
}

/// A candidate's progress, from GitHub: the issue's timeline since `since`.
fn fetch_progress(env: Lookup, issue: u64, since: u64) -> Option<Vec<u64>> {
    const QUERY: &str = "query($owner: String!, $name: String!, $number: Int!, $since: DateTime!) { repository(owner: $owner, name: $name) { issue(number: $number) { timelineItems(last: 100, since: $since, itemTypes: [LABELED_EVENT, UNLABELED_EVENT, CROSS_REFERENCED_EVENT, REFERENCED_EVENT, CONNECTED_EVENT]) { nodes { __typename ... on LabeledEvent { createdAt label { name } } ... on UnlabeledEvent { createdAt label { name } } ... on CrossReferencedEvent { createdAt } ... on ReferencedEvent { createdAt } ... on ConnectedEvent { createdAt } } } } } }";
    let repo = env("GH_REPO")?;
    let (host, owner, name) = match repo.split('/').collect::<Vec<_>>()[..] {
        [host, owner, name] => (Some(host.to_owned()), owner.to_owned(), name.to_owned()),
        [owner, name] => (None, owner.to_owned(), name.to_owned()),
        _ => return None,
    };
    let wip = env("WIP_LABEL").unwrap_or_else(|| "ai:wip".to_owned());
    let fields = [
        format!("query={QUERY}"),
        format!("owner={owner}"),
        format!("name={name}"),
        format!("since={}", iso_of(since)),
    ];
    let number = format!("number={issue}");
    let mut args = vec!["api", "graphql"];
    if let Some(host) = &host {
        args.extend(["--hostname", host]);
    }
    for field in &fields {
        args.extend(["-f", field]);
    }
    args.extend(["-F", &number]);
    let output = crate::gh::run(&args, crate::gh::Stderr::Discard).ok()?;
    if !output.status.success() {
        return None;
    }
    let doc: Value = serde_json::from_slice(&output.stdout).ok()?;
    let nodes = doc["data"]["repository"]["issue"]["timelineItems"]["nodes"].as_array()?;
    Some(
        nodes
            .iter()
            .filter(|node| node["label"]["name"].as_str() != Some(wip.as_str()))
            .filter_map(|node| epoch_of(node["createdAt"].as_str()?))
            .collect(),
    )
}

/// The looping issues now, their progress read from GitHub once per deciding claim and kept in
/// `loops.json`.
pub fn loop_signals(env: Lookup, paths: &Paths, now: u64) -> BTreeMap<u64, u64> {
    const MAX_LOOKS: usize = 5;
    let candidates = loop_candidates(&read_claims(&paths.state), now);
    if candidates.is_empty() {
        return BTreeMap::new();
    }
    let cache_path = paths.file("loops.json");
    let mut cache: BTreeMap<u64, (u64, Option<Vec<u64>>)> =
        files::read_text_limited(&cache_path, 1 << 16)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
    cache.retain(|issue, _| candidates.contains_key(issue));
    let mut looked = 0;
    for (issue, (first, last)) in &candidates {
        if cache.get(issue).is_some_and(|(through, _)| through == last) || looked >= MAX_LOOKS {
            continue;
        }
        looked += 1;
        cache.insert(*issue, (*last, fetch_progress(env, *issue, *first)));
    }
    if looked > 0
        && let Ok(text) = serde_json::to_string(&cache)
    {
        let _ = files::write_replacing(&cache_path, &text);
    }
    let progress: BTreeMap<u64, Vec<u64>> = cache
        .into_iter()
        .filter_map(|(issue, (_, times))| Some((issue, times?)))
        .collect();
    loops(&candidates, &progress)
}

/// Whether a supervisor tick starts now, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Due(Vec<String>),
    NotDue(String),
}

/// Why no run may start now whatever happened: too soon after the last start, or inside a failed
/// run's backoff.
pub fn throttled(settings: &Settings, run: &RunState, now: u64) -> Option<String> {
    if let Some(start) = run.last_start
        && now < start.saturating_add(settings.min_interval_s)
    {
        return Some(format!(
            "the last run started {}s ago, under SUPERVISOR_MIN_INTERVAL_S",
            now.saturating_sub(start)
        ));
    }
    let (wait, until) = backoff(settings, run);
    if now < until {
        return Some(format!(
            "backing off {wait}s after {} failed runs",
            run.failures
        ));
    }
    None
}

/// The backoff after the failed runs in a row, and when it ends: the minimum interval doubled for
/// each, from the last run's end.
fn backoff(settings: &Settings, run: &RunState) -> (u64, u64) {
    if run.failures == 0 {
        return (0, 0);
    }
    let wait = settings
        .min_interval_s
        .saturating_mul(1 << run.failures.min(MAX_BACKOFF_DOUBLINGS));
    let from = run.last_end.or(run.last_start).unwrap_or(0);
    (wait, from.saturating_add(wait))
}

/// The decision, given the events since the last start and whether the supervisor's agent has room.
pub fn decide(
    settings: &Settings,
    run: &RunState,
    events: &[Event],
    room: &dyn Fn() -> Result<(), String>,
    now: u64,
) -> Decision {
    if let Some(why) = throttled(settings, run, now) {
        return Decision::NotDue(why);
    }
    let mut reasons: Vec<String> = events
        .iter()
        .map(|event| event.kind.name().to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    match run.last_start {
        None => reasons.push("first".to_owned()),
        Some(start) if now >= start.saturating_add(settings.interval_s) => {
            reasons.push("interval".to_owned());
        }
        Some(_) => {}
    }
    if reasons.is_empty() {
        return Decision::NotDue("no event since the last run".to_owned());
    }
    match room() {
        Ok(()) => Decision::Due(reasons),
        Err(why) => Decision::NotDue(why),
    }
}

/// The tick logs' last trailers: `[tick.sh] exit=<n> ...`.
fn tick_ends(logs: &Path, since: u64) -> Vec<(u64, i64)> {
    let Ok(entries) = std::fs::read_dir(logs) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.starts_with("tick-")
                    && Path::new(name).extension().is_some_and(|ext| ext == "log")
            })
        })
        .filter_map(|entry| {
            let at = modified_s(&entry.path())?;
            (at > since).then_some(())?;
            let exit = last_exit(&read_tail(&entry.path(), TRAILER_TAIL))?;
            Some((at, exit))
        })
        .collect()
}

/// The exit the last `[tick.sh] exit=<n>` line names.
pub fn last_exit(text: &str) -> Option<i64> {
    text.lines()
        .rev()
        .find_map(|line| line.strip_prefix("[tick.sh] exit="))
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
}

/// `claims.log`: `<issue> <pid> <UTC time>` a line, as libexec/claim.sh appends it.
pub fn read_claims(state: &Path) -> Vec<Claim> {
    read_tail(&state.join("claims.log"), CLAIMS_TAIL)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let issue = words.next()?.parse().ok()?;
            let pid = words.next()?.parse().ok()?;
            let at = epoch_of(words.next()?)?;
            Some(Claim { at, issue, pid })
        })
        .collect()
}

fn modified_in(dir: &Path) -> Vec<u64> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| modified_s(&entry.path()))
        .collect()
}

/// Turns the launcher's mark of a pause it saw into the moment the human lifted it: the launcher
/// asks only once the harness runs again.
fn note_unpause(paths: &Paths, now: u64) {
    if paths.file("paused").exists() && std::fs::remove_file(paths.file("paused")).is_ok() {
        let _ = write_value(&paths.file("unpaused"), &now.to_string());
    }
}

/// The queue as the supervisor weighs it: the open issues carrying the queue's label, claimed ones
/// included, as the launcher's last queue count listed them (`queue`). A claim, and the claim
/// label a tick adds or takes off, leaves this number as it was; a new, closed or relabelled issue
/// changes it.
fn queue_total(paths: &Paths) -> Option<u64> {
    read_number(&paths.file("queue"))
}

/// The inputs of the events after `since`, but the loops, which [`loop_signals`] reads from GitHub.
pub fn gather(paths: &Paths, since: u64) -> Inputs {
    let state = &paths.state;
    let tick_ages = crate::ticks::scan(&state.to_string_lossy())
        .unwrap_or_default()
        .into_iter()
        .filter(|tick| tick.kind != crate::ticks::Kind::Supervisor)
        .map(|tick| tick.age_s)
        .collect();
    let operator = read_number(&paths.file("operator")).into_iter().collect();
    Inputs {
        tick_ends: tick_ends(&state.join("logs"), since),
        limits: modified_in(&state.join("model-cooldown")),
        next_reset: read_number(&paths.file("next_reset")),
        queue_now: queue_total(paths),
        queue_at_run: read_number(&paths.file("queue_at_run")),
        tick_ages,
        loops: Vec::new(),
        operator,
        unpaused: read_number(&paths.file("unpaused")),
        stall_log: modified_s(&state.join("stalls.log")),
        directive_expires: crate::directive::stored_expiry(paths),
    }
}

/// Whether the supervisor's agent may run one short supervisor tick: its entry is not cooling down
/// and every window of the agent is under the point where a running tick stands down.
///
/// It needs no room under the agent's cap, an exception to the rule every tick meets: it runs
/// outside the slots, once, within its budget, and holding it back while the work ticks fill the cap
/// would leave the harness unsteered when it most needs it. The exception covers only the cap. It
/// does not lift the ceiling: a window at its stand-down point, a reading that is not fresh or an
/// entry cooling down still keeps it from starting. It does not let two run: the lock and the
/// minimum interval still hold. And it adds no work: its tick file counts toward the agent's cap from
/// the moment it starts, so the launcher starts that many fewer work ticks, and the agent runs at
/// most one tick past its cap for at most `SUPERVISOR_BUDGET_MIN`.
pub fn room(env: Lookup, paths: &Paths, entry: &Entry) -> Result<(), String> {
    let settings = usage::Settings::from_env(env)?;
    let cooldown = Duration::from_secs(number(env, "MODEL_COOLDOWN_S", 1800)?);
    if usage::cooling(&paths.state, entry, cooldown) {
        return Err(format!("{entry} is cooling down on its usage limit"));
    }
    let agent = usage::agent_usage(env, &settings, &entry.agent, false)?;
    match usage::verdict(&settings, &agent) {
        usage::Verdict::Go => Ok(()),
        usage::Verdict::Stop => Err(format!(
            "{}'s windows are at their stand-down point",
            entry.agent
        )),
        usage::Verdict::Unknown => Err(format!("{} has no fresh usage reading", entry.agent)),
    }
}

/// The operators' comments the last looks found, newest last, for `harness health`.
pub fn read_operator_comments(paths: &Paths) -> Vec<OperatorComment> {
    files::read_text_limited(&paths.file("operator-comments.json"), 1 << 16)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Looks for an operator's Operator comment on the repository since the last look, at most once a
/// minimum interval: stamps `operator` with the newest one's time and keeps the newest few for
/// `harness health`. A failed look is left for the next; an `OPERATOR_LOGINS` that names no logins
/// reads none.
fn look_for_operator(env: Lookup, settings: &Settings, paths: &Paths, now: u64) {
    let Some(repo) = env("GH_REPO") else { return };
    let authors = match Authors::from_env(env) {
        Ok(authors) => authors,
        Err(message) => {
            eprintln!("harness supervisor: {message}; no Operator comment read");
            return;
        }
    };
    let checked = read_number(&paths.file("github_checked"));
    if checked.is_some_and(|at| now < at.saturating_add(settings.min_interval_s)) {
        return;
    }
    let since = checked.unwrap_or(now.saturating_sub(settings.interval_s));
    let (host, path) = match repo.split('/').collect::<Vec<_>>()[..] {
        [host, owner, name] => (Some(host), format!("{owner}/{name}")),
        _ => (None, repo.clone()),
    };
    let url = format!(
        "repos/{path}/issues/comments?since={}&sort=created&direction=desc&per_page=100",
        iso_of(since)
    );
    let mut args = vec!["api"];
    if let Some(host) = host {
        args.extend(["--hostname", host]);
    }
    args.push(&url);
    let Ok(output) = crate::gh::run(&args, crate::gh::Stderr::Discard) else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let _ = write_value(&paths.file("github_checked"), &now.to_string());
    let Ok(Value::Array(comments)) = serde_json::from_slice::<Value>(&output.stdout) else {
        return;
    };
    let found = operator_comments(&comments, &authors, since);
    let Some(newest) = found.last() else { return };
    let _ = write_value(&paths.file("operator"), &newest.at.to_string());
    let mut kept = read_operator_comments(paths);
    kept.extend(found);
    let kept = &kept[kept.len().saturating_sub(OPERATOR_COMMENTS_KEPT)..];
    if let Ok(text) = serde_json::to_string(kept) {
        let _ = files::write_replacing(&paths.file("operator-comments.json"), &text);
    }
}

fn due_command(env: Lookup, settings: &Settings, paths: &Paths) -> Result<ExitCode, String> {
    paths.prepare()?;
    let now = now_s();
    note_unpause(paths, now);
    let run = RunState::read(paths);
    if let Some(why) = throttled(settings, &run, now) {
        println!("not due: {why}");
        return Ok(ExitCode::from(1));
    }
    let models = configured_models(env, &paths.state)?;
    let entry = supervisor_entry(env, &models)?;
    look_for_operator(env, settings, paths, now);
    let since = run.last_start.unwrap_or(0);
    let mut inputs = gather(paths, since);
    inputs.loops = loop_signals(env, paths, now).into_values().collect();
    let found = events(&inputs, settings, since, now);
    match decide(settings, &run, &found, &|| room(env, paths, &entry), now) {
        Decision::Due(reasons) => {
            println!("due {}", reasons.join(","));
            Ok(ExitCode::SUCCESS)
        }
        Decision::NotDue(why) => {
            println!("not due: {why}");
            Ok(ExitCode::from(1))
        }
    }
}

/// The earliest window reset still ahead, a number of Unix seconds or an ISO time, across every agent of the model list.
fn next_reset(env: Lookup, now: u64) -> Option<u64> {
    let pool = usage::pool(env).ok()?;
    pool.agents
        .iter()
        .flat_map(|agent| agent.windows.iter())
        .filter_map(|window| match window.resets_at.as_ref()? {
            Value::String(text) => epoch_of(text),
            other => other.as_u64(),
        })
        .filter(|at| *at > now)
        .min()
}

fn begin(env: Lookup, paths: &Paths, reason: &str) -> Result<(), String> {
    paths.prepare()?;
    let now = now_s();
    write_value(&paths.file("last_start"), &now.to_string())?;
    let reason: String = reason
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ',' || *c == '-')
        .take(200)
        .collect();
    write_value(&paths.file("last_reason"), &reason)?;
    match queue_total(paths) {
        Some(count) => write_value(&paths.file("queue_at_run"), &count.to_string())?,
        None => {
            let _ = std::fs::remove_file(paths.file("queue_at_run"));
        }
    }
    if let Some(at) = next_reset(env, now) {
        write_value(&paths.file("next_reset"), &at.to_string())
    } else {
        let _ = std::fs::remove_file(paths.file("next_reset"));
        Ok(())
    }
}

pub fn end(paths: &Paths, exit: i32, duration: u64, now: u64) -> Result<(), String> {
    paths.prepare()?;
    let run = RunState::read(paths);
    let failures = if exit == 0 {
        0
    } else {
        run.failures.saturating_add(1)
    };
    write_value(&paths.file("last_end"), &now.to_string())?;
    write_value(&paths.file("last_exit"), &exit.to_string())?;
    write_value(&paths.file("failures"), &failures.to_string())?;
    let reason = read_line(&paths.file("last_reason")).unwrap_or_default();
    let line = format!(
        "{} exit={exit} duration={duration}s reason={reason}",
        iso_of(now)
    );
    let runs = paths.file("runs.log");
    let old = files::read_text_limited(&runs, 1 << 20).unwrap_or_default();
    let mut lines: Vec<&str> = old.lines().collect();
    lines.push(&line);
    let keep = &lines[lines.len().saturating_sub(RUNS_KEPT)..];
    files::write_replacing(&runs, &format!("{}\n", keep.join("\n")))
        .map_err(|error| format!("{}: {error}", runs.display()))?;
    for dir in ["notes-seen", "stopped"] {
        prune(&paths.file(dir), now);
    }
    Ok(())
}

/// Removes the marks under `dir` older than [`MARKS_KEPT_S`]: each names a tick, and no directive
/// outlives that.
fn prune(dir: &Path, now: u64) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if modified_s(&entry.path()).is_some_and(|at| at.saturating_add(MARKS_KEPT_S) < now) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The supervisor's runs, its next due time and the directive, as `harness health` and the
/// dashboard show them.
pub fn view(env: Lookup, paths: &Paths, now: u64) -> Value {
    let settings = Settings::from_env(env);
    let run = RunState::read(paths);
    let runs: Vec<String> = read_tail(&paths.file("runs.log"), 8192)
        .lines()
        .rev()
        .take(10)
        .map(str::to_owned)
        .collect();
    let next_due = settings.as_ref().ok().map(|settings| {
        let earliest = run
            .last_start
            .map_or(now, |start| start.saturating_add(settings.min_interval_s))
            .max(backoff(settings, &run).1);
        let latest = run
            .last_start
            .map_or(now, |start| start.saturating_add(settings.interval_s));
        json!({"earliest": iso_of(earliest.max(now)), "latest": iso_of(latest.max(earliest).max(now))})
    });
    let directive_log: Vec<String> = read_tail(&paths.directive_log(), 16384)
        .lines()
        .rev()
        .take(12)
        .map(str::to_owned)
        .collect();
    json!({
        "enabled": env("SUPERVISOR").as_deref() == Some("1"),
        "last_start": run.last_start.map(iso_of),
        "last_end": run.last_end.map(iso_of),
        "last_exit": run.last_exit,
        "last_reason": read_line(&paths.file("last_reason")),
        "failures": run.failures,
        "next_due": next_due,
        "runs": runs,
        "directive": crate::directive::view(env, paths, now),
        "directive_log": directive_log,
        "error": settings.err(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            budget_min: 10,
            interval_s: 1800,
            min_interval_s: 600,
            max_directive_s: 21_600,
            stop_grace_s: 600,
            stall_after_s: 14_400,
        }
    }

    const NOW: u64 = 1_800_000_000;
    #[allow(clippy::unnecessary_wraps)]
    fn ok() -> Result<(), String> {
        Ok(())
    }

    fn event(kind: EventKind, at: u64) -> Event {
        Event { kind, at }
    }

    #[test]
    fn the_first_run_is_due() {
        let decision = decide(&settings(), &RunState::default(), &[], &ok, NOW);
        assert_eq!(decision, Decision::Due(vec!["first".to_owned()]));
    }

    #[test]
    fn an_event_makes_it_due_past_the_minimum_interval() {
        let run = RunState {
            last_start: Some(NOW - 700),
            last_end: Some(NOW - 650),
            ..RunState::default()
        };
        let found = [
            event(EventKind::TickFailed, NOW - 10),
            event(EventKind::Limit, NOW - 5),
        ];
        assert_eq!(
            decide(&settings(), &run, &found, &ok, NOW),
            Decision::Due(vec!["limit".to_owned(), "tick-failed".to_owned()])
        );
    }

    #[test]
    fn no_event_waits_for_the_interval() {
        let run = RunState {
            last_start: Some(NOW - 1000),
            ..RunState::default()
        };
        assert!(matches!(
            decide(&settings(), &run, &[], &ok, NOW),
            Decision::NotDue(_)
        ));
        let run = RunState {
            last_start: Some(NOW - 1800),
            ..RunState::default()
        };
        assert_eq!(
            decide(&settings(), &run, &[], &ok, NOW),
            Decision::Due(vec!["interval".to_owned()])
        );
    }

    #[test]
    fn the_minimum_interval_holds_whatever_happened() {
        let run = RunState {
            last_start: Some(NOW - 599),
            ..RunState::default()
        };
        let found = [event(EventKind::Operator, NOW - 1)];
        assert!(
            matches!(decide(&settings(), &run, &found, &ok, NOW), Decision::NotDue(why) if why.contains("MIN_INTERVAL"))
        );
    }

    #[test]
    fn backoff_doubles_after_each_failure() {
        let found = [event(EventKind::Operator, NOW - 1)];
        for (failures, wait) in [(1, 1200), (2, 2400), (3, 4800), (9, 600 * 64)] {
            let run = RunState {
                last_start: Some(NOW - wait),
                last_end: Some(NOW - wait + 1),
                last_exit: Some(1),
                failures,
            };
            assert!(
                matches!(decide(&settings(), &run, &found, &ok, NOW), Decision::NotDue(why) if why.contains("backing off")),
                "{failures}"
            );
            let run = RunState {
                last_end: Some(NOW - wait),
                ..run
            };
            assert!(
                matches!(
                    decide(&settings(), &run, &found, &ok, NOW),
                    Decision::Due(_)
                ),
                "{failures}"
            );
        }
    }

    #[test]
    fn no_room_for_its_agent_is_not_due() {
        let full = || Err("claude runs 2 ticks, at its cap of 2".to_owned());
        assert_eq!(
            decide(&settings(), &RunState::default(), &[], &full, NOW),
            Decision::NotDue("claude runs 2 ticks, at its cap of 2".to_owned())
        );
    }

    #[test]
    fn events_count_only_after_the_last_run() {
        let since = NOW - 1000;
        let inputs = Inputs {
            tick_ends: vec![(NOW - 10, 1), (NOW - 20, 0), (NOW - 2000, 1)],
            limits: vec![NOW - 5, NOW - 5000],
            next_reset: Some(NOW - 30),
            queue_now: Some(4),
            queue_at_run: Some(3),
            tick_ages: vec![14_400 + 100, 14_400 + 5000, 60],
            loops: vec![NOW - 2000],
            operator: vec![NOW - 40],
            unpaused: Some(NOW - 50),
            stall_log: Some(NOW - 2000),
            directive_expires: Some(NOW + 300),
        };
        let kinds: Vec<EventKind> = events(&inputs, &settings(), since, NOW)
            .iter()
            .map(|event| event.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                EventKind::Expiring,
                EventKind::Stall,
                EventKind::Unpaused,
                EventKind::Operator,
                EventKind::Reset,
                EventKind::TickFailed,
                EventKind::Limit,
                EventKind::Claimable,
            ]
        );
        let quiet = Inputs {
            queue_now: Some(3),
            queue_at_run: Some(3),
            next_reset: Some(NOW + 60),
            directive_expires: Some(NOW + 3600),
            ..Inputs::default()
        };
        assert!(events(&quiet, &settings(), since, NOW).is_empty());
    }

    #[test]
    fn three_ticks_on_one_issue_within_the_window_loop() {
        let claim = |at, issue, pid| Claim { at, issue, pid };
        let claims = [
            claim(NOW - 3000, 7, 11),
            claim(NOW - 2000, 7, 12),
            claim(NOW - 1000, 7, 13),
            claim(NOW - 900, 8, 21),
            claim(NOW - 800, 8, 21),
            claim(NOW - 700, 8, 21),
            claim(NOW - LOOP_WINDOW_S - 10, 9, 31),
            claim(NOW - 500, 9, 32),
            claim(NOW - 400, 9, 33),
        ];
        let candidates = loop_candidates(&claims, NOW);
        assert_eq!(candidates, BTreeMap::from([(7, (NOW - 3000, NOW - 1000))]));
        // Three claims with no progress between them loop; a label change, a PR or a commit
        // between the first and the third is the work, review and landing cycle; an issue whose
        // progress could not be read is no loop.
        let none = BTreeMap::from([(7, vec![NOW - 4000, NOW - 500])]);
        assert_eq!(loops(&candidates, &none), BTreeMap::from([(7, NOW - 1000)]));
        let progressed = BTreeMap::from([(7, vec![NOW - 2500])]);
        assert!(loops(&candidates, &progressed).is_empty());
        assert!(loops(&candidates, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn a_known_progress_is_read_once_per_deciding_claim() {
        let dir = crate::dashboard::testkit::TempDir::new();
        let paths = Paths {
            state: dir.path().to_path_buf(),
        };
        paths.prepare().unwrap();
        let at = |offset: u64| iso_of(NOW - offset);
        std::fs::write(
            dir.path().join("claims.log"),
            format!("7 11 {}\n7 12 {}\n7 13 {}\n", at(3000), at(2000), at(1000)),
        )
        .unwrap();
        // GitHub is not asked again for a deciding claim already read.
        std::fs::write(
            paths.file("loops.json"),
            serde_json::to_string(&BTreeMap::from([(
                7,
                (NOW - 1000, Some(Vec::<u64>::new())),
            )]))
            .unwrap(),
        )
        .unwrap();
        let no_github = |_: &str| None;
        assert_eq!(
            loop_signals(&no_github, &paths, NOW),
            BTreeMap::from([(7, NOW - 1000)])
        );
        // A fourth claim leaves the deciding claim as it was, so the cache still answers.
        std::fs::write(
            dir.path().join("claims.log"),
            format!(
                "7 11 {}\n7 12 {}\n7 13 {}\n7 14 {}\n",
                at(3000),
                at(2000),
                at(1000),
                at(10)
            ),
        )
        .unwrap();
        assert_eq!(
            loop_signals(&no_github, &paths, NOW),
            BTreeMap::from([(7, NOW - 1000)])
        );
    }

    #[test]
    fn the_last_trailer_names_the_exit() {
        let log = "work\n[tick.sh] exit=1 duration=3s agent=a model=m\nretry\n[tick.sh] exit=0 duration=900s agent=b model=n\n";
        assert_eq!(last_exit(log), Some(0));
        assert_eq!(last_exit("no trailer"), None);
    }

    #[test]
    fn run_state_and_runs_log_round_trip() {
        let dir = crate::dashboard::testkit::TempDir::new();
        let paths = Paths {
            state: dir.path().to_path_buf(),
        };
        end(&paths, 3, 42, NOW).unwrap();
        end(&paths, 1, 7, NOW + 10).unwrap();
        let run = RunState::read(&paths);
        assert_eq!(run.failures, 2);
        assert_eq!(run.last_exit, Some(1));
        assert_eq!(run.last_end, Some(NOW + 10));
        end(&paths, 0, 7, NOW + 20).unwrap();
        assert_eq!(RunState::read(&paths).failures, 0);
        let runs = std::fs::read_to_string(paths.file("runs.log")).unwrap();
        assert_eq!(runs.lines().count(), 3);
        assert!(
            runs.lines()
                .next()
                .unwrap()
                .ends_with("exit=3 duration=42s reason=")
        );
    }

    #[test]
    fn claims_log_reads_its_lines_and_skips_the_rest() {
        let dir = crate::dashboard::testkit::TempDir::new();
        std::fs::write(
            dir.path().join("claims.log"),
            "7 100 2026-11-28T10:00:00Z\ngarbage\n8 x 2026-11-28T10:00:00Z\n9 101 2026-11-28T11:00:00Z\n",
        )
        .unwrap();
        let claims = read_claims(dir.path());
        assert_eq!(claims.len(), 2);
        assert_eq!(claims[1].issue, 9);
        assert_eq!(claims[1].at, epoch_of("2026-11-28T11:00:00Z").unwrap());
    }

    #[test]
    fn gather_reads_the_state_files() {
        let dir = crate::dashboard::testkit::TempDir::new();
        let paths = Paths {
            state: dir.path().to_path_buf(),
        };
        paths.prepare().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        std::fs::write(
            dir.path().join("logs/tick-20261128T100000Z-slot1.log"),
            "x\n[tick.sh] exit=2 duration=5s agent=a model=m\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("logs/allocator.log"), "[tick.sh] exit=9\n").unwrap();
        std::fs::write(paths.file("queue"), "4\n").unwrap();
        std::fs::write(paths.file("queue_at_run"), "2\n").unwrap();
        std::fs::write(paths.file("paused"), "").unwrap();
        note_unpause(&paths, NOW);
        let inputs = gather(&paths, 0);
        assert_eq!(inputs.tick_ends.len(), 1);
        assert_eq!(inputs.tick_ends[0].1, 2);
        assert_eq!((inputs.queue_now, inputs.queue_at_run), (Some(4), Some(2)));
        assert_eq!(inputs.unpaused, Some(NOW));
        assert!(!paths.file("paused").exists());
    }
}

#[cfg(test)]
mod room_tests {
    use super::*;

    #[test]
    fn the_supervisor_may_run_under_the_stand_down_point_however_full_the_cap() {
        let dir = crate::dashboard::testkit::TempDir::new();
        let paths = Paths {
            state: dir.path().to_path_buf(),
        };
        let reading = dir.path().join("usage.json");
        let source = format!("json:{}", reading.display());
        let state = dir.path().display().to_string();
        let env = |key: &str| match key {
            "USAGE_SOURCE_claude" => Some(source.clone()),
            "HARNESS_STATE" | "HOME" => Some(state.clone()),
            "MAX_SLOTS" => Some("3".to_owned()),
            _ => None,
        };
        let entry = Entry::parse("claude:opus", "claude").unwrap();
        let write = |used: f64| {
            let doc = json!({"timestamp": now_s() * 1000, "windows": [{"name": "five_hour", "used_pct": used, "window_minutes": 300}]});
            std::fs::write(&reading, doc.to_string()).unwrap();
        };
        // At 75 % the window's cap is one tick or none; the supervisor still runs, under 88 %.
        write(75.0);
        assert_eq!(room(&env, &paths, &entry), Ok(()));
        write(88.0);
        assert!(room(&env, &paths, &entry).is_err_and(|why| why.contains("stand-down")));
        std::fs::remove_file(&reading).unwrap();
        assert!(
            room(&env, &paths, &entry).is_err_and(|why| why.contains("no fresh usage reading"))
        );
        write(10.0);
        std::fs::create_dir_all(dir.path().join("model-cooldown")).unwrap();
        std::fs::write(dir.path().join("model-cooldown/claude:opus"), "").unwrap();
        assert!(room(&env, &paths, &entry).is_err_and(|why| why.contains("cooling down")));
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;

    #[test]
    fn a_claim_and_its_label_between_two_runs_are_no_queue_event() {
        let dir = crate::dashboard::testkit::TempDir::new();
        let paths = Paths {
            state: dir.path().to_path_buf(),
        };
        paths.prepare().unwrap();
        let settings = Settings::from_env(&|_| None).unwrap();
        let queue_event = |total: &str| {
            std::fs::write(paths.file("queue"), total).unwrap();
            let mut inputs = gather(&paths, 0);
            inputs.queue_at_run = Some(5);
            events(&inputs, &settings, 0, now_s())
                .iter()
                .any(|event| event.kind == EventKind::Claimable)
        };
        // Five open queue issues at the last run. A tick claims one and labels it: the claimable
        // count falls to four, the open issues stay five.
        assert!(!queue_event("5\n"));
        // An issue landed and closed, or a new one was queued.
        assert!(queue_event("4\n"));
        assert!(queue_event("6\n"));
        // A record that does not read is no event.
        assert!(!queue_event("five\n"));
    }
}
