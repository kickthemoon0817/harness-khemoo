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
/// The operators' comments kept for `harness health`.
const OPERATOR_COMMENTS_KEPT: usize = 10;

/// The keys the supervisor's and the directive's commands read from harness.env when the
/// environment does not set them: harness.env is sourced, not exported, so a supervisor tick that
/// runs `harness directive apply` has none of it in its environment.
const CONFIG_KEYS: [&str; 20] = [
    "HARNESS_STATE",
    "GH_REPO",
    "OPERATOR_LOGINS",
    "WORKTREE_ROOT",
    "TICK_MODELS",
    "TICK_AGENT",
    "MAX_SLOTS",
    "TICK_WIDTH",
    "MODEL_COOLDOWN_S",
    "TICK_BUDGET_MIN",
    "LONG_TICK_BUDGET_MIN",
    "LONG_LABEL",
    "HOLD_TIMEOUT_S",
    "SUPERVISOR",
    "SUPERVISOR_MODEL",
    "SUPERVISOR_BUDGET_MIN",
    "SUPERVISOR_INTERVAL_S",
    "SUPERVISOR_MIN_INTERVAL_S",
    "SUPERVISOR_MAX_DIRECTIVE_S",
    "SUPERVISOR_STOP_GRACE_S",
];

/// The environment's value of a key, else harness.env's for the keys [`CONFIG_KEYS`] names.
pub fn config_lookup() -> impl Fn(&str) -> Option<String> {
    let file = harness_home()
        .map(|home| {
            crate::dashboard::env::read_keys(
                &crate::dashboard::env_path(&home, &crate::dashboard::env_nonempty),
                |key| CONFIG_KEYS.contains(&key),
            )
        })
        .unwrap_or_default();
    move |key: &str| {
        std::env::var(key)
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| file.get(key).cloned().filter(|value| !value.is_empty()))
    }
}

#[derive(clap::Args)]
#[command(after_help = "\
Reads SUPERVISOR_MODEL, SUPERVISOR_BUDGET_MIN, SUPERVISOR_INTERVAL_S, SUPERVISOR_MIN_INTERVAL_S,
TICK_MODELS, TICK_AGENT, LONG_TICK_BUDGET_MIN and GH_REPO from the environment, else harness.env,
the usage keys from the environment, and its own state from $HARNESS_STATE/supervisor/.")]
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
            stop_grace_s: number(env, "SUPERVISOR_STOP_GRACE_S", 600)?,
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
    /// The events that end a directive's pause or its quiet before `wake_at`.
    pub fn wakes(self) -> bool {
        matches!(
            self,
            Self::Reset | Self::Operator | Self::Unpaused | Self::Expiring
        )
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
    pub claimable_now: Option<u64>,
    pub claimable_at_run: Option<u64>,
    /// Each live tick's age, supervisors left out.
    pub tick_ages: Vec<u64>,
    pub claims: Vec<Claim>,
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
    if let (Some(current), Some(before)) = (inputs.claimable_now, inputs.claimable_at_run)
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
    for at in loops(&inputs.claims, now).into_values() {
        push(EventKind::Loop, at);
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
/// `now`, with the time of the claim that made it so.
pub fn loops(claims: &[Claim], now: u64) -> BTreeMap<u64, u64> {
    let mut by_issue: BTreeMap<u64, Vec<&Claim>> = BTreeMap::new();
    for claim in claims {
        if claim.at + LOOP_WINDOW_S >= now && claim.at <= now {
            by_issue.entry(claim.issue).or_default().push(claim);
        }
    }
    let mut looping = BTreeMap::new();
    for (issue, mut list) in by_issue {
        list.sort_by_key(|claim| claim.at);
        let mut pids = BTreeSet::new();
        for claim in list {
            pids.insert(claim.pid);
            if pids.len() >= LOOP_CLAIMS {
                looping.insert(issue, claim.at);
                break;
            }
        }
    }
    looping
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
    if run.failures > 0 {
        let wait = settings
            .min_interval_s
            .saturating_mul(1 << run.failures.min(MAX_BACKOFF_DOUBLINGS));
        let from = run.last_end.or(run.last_start).unwrap_or(0);
        if now < from.saturating_add(wait) {
            return Some(format!(
                "backing off {wait}s after {} failed runs",
                run.failures
            ));
        }
    }
    None
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
        claimable_now: read_number(&paths.file("claimable")),
        claimable_at_run: read_number(&paths.file("claimable_at_run")),
        tick_ages,
        claims: read_claims(state),
        operator,
        unpaused: read_number(&paths.file("unpaused")),
        stall_log: modified_s(&state.join("stalls.log")),
        directive_expires: crate::directive::stored_expiry(paths),
    }
}

/// Room for one more tick of the supervisor's agent: its entry is not cooling down and the agent
/// runs fewer ticks than its usage windows allow.
fn room(env: Lookup, paths: &Paths, entry: &Entry) -> Result<(), String> {
    let settings = usage::Settings::from_env(env)?;
    let cooldown = Duration::from_secs(number(env, "MODEL_COOLDOWN_S", 1800)?);
    if usage::cooling(&paths.state, entry, cooldown) {
        return Err(format!("{entry} is cooling down on its usage limit"));
    }
    let cap = usage::agent_usage(env, &settings, &entry.agent, false)?.cap;
    let running = crate::ticks::scan(&paths.state.to_string_lossy())
        .map_err(|error| format!("cannot count the running ticks: {error}"))?
        .iter()
        .filter(|tick| tick.agent == entry.agent)
        .count();
    if u32::try_from(running).unwrap_or(u32::MAX) < cap {
        Ok(())
    } else {
        Err(format!(
            "{} runs {running} ticks, at its cap of {cap}",
            entry.agent
        ))
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
        "repos/{path}/issues/comments?since={}&per_page=100",
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
    let found = events(&gather(paths, since), settings, since, now);
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
    match read_number(&paths.file("claimable")) {
        Some(count) => write_value(&paths.file("claimable_at_run"), &count.to_string())?,
        None => {
            let _ = std::fs::remove_file(paths.file("claimable_at_run"));
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
        .map_err(|error| format!("{}: {error}", runs.display()))
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
            .map_or(now, |start| start.saturating_add(settings.min_interval_s));
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
            claimable_now: Some(4),
            claimable_at_run: Some(3),
            tick_ages: vec![14_400 + 100, 14_400 + 5000, 60],
            claims: Vec::new(),
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
            claimable_now: Some(3),
            claimable_at_run: Some(3),
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
        assert_eq!(loops(&claims, NOW), BTreeMap::from([(7, NOW - 1000)]));
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
        std::fs::write(paths.file("claimable"), "4\n").unwrap();
        std::fs::write(paths.file("claimable_at_run"), "2\n").unwrap();
        std::fs::write(paths.file("paused"), "").unwrap();
        note_unpause(&paths, NOW);
        let inputs = gather(&paths, 0);
        assert_eq!(inputs.tick_ends.len(), 1);
        assert_eq!(inputs.tick_ends[0].1, 2);
        assert_eq!(
            (inputs.claimable_now, inputs.claimable_at_run),
            (Some(4), Some(2))
        );
        assert_eq!(inputs.unpaused, Some(NOW));
        assert!(!paths.file("paused").exists());
    }
}
