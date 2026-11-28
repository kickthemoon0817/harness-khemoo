//! `harness directive`: the supervisor's one way to steer the harness.
//!
//! A directive narrows or reorders what the configuration allows, never widens it, and lasts until
//! its `expires_at`. `apply` validates one strictly (an unknown field, a model outside the model
//! list, a pid that is not a live tick of this installation are refused), clamps what has a bound,
//! writes `<state>/directive.json` atomically and owner-only, and logs the change to
//! `<state>/logs/directive.log`. The launcher reads it back through `effective` on each firing and
//! falls back to the configuration's own formulas when there is none, or it expired, or it no
//! longer validates. Every text in it goes through the dashboard's secret masks, and one that
//! carries a secret is refused, so the directive and its log never hold one.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write as _};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::dashboard::files;
use crate::dashboard::redact::redact;
use crate::dashboard::text::{invisible, sha256_hex};
use crate::supervisor::{self, EventKind, Paths, epoch_of, iso_of, now_s};
use crate::ticks::{self, Kind};
use crate::usage::{self, Entry, Lookup};

/// The most `apply` reads of its input.
const INPUT_LIMIT: u64 = 64 * 1024;
const REASON_CHARS: usize = 200;
const NOTE_CHARS: usize = 500;
const MAX_ISSUES: usize = 50;
const MAX_NOTES: usize = 20;
const MAX_STOPS: usize = 8;
const STAT_LIMIT: u64 = 4096;

#[derive(clap::Args)]
#[command(after_help = "\
Reads TICK_MODELS, TICK_AGENT, MAX_SLOTS, TICK_WIDTH, SUPERVISOR_MAX_DIRECTIVE_S and
SUPERVISOR_STOP_GRACE_S from the environment; the directive is $HARNESS_STATE/directive.json and its
log $HARNESS_STATE/logs/directive.log.")]
pub struct Args {
    #[command(subcommand)]
    command: Sub,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Validate a directive (a file, or - for stdin), clamp it, write it and log the change.
    Apply { file: String },
    /// Print the directive and whether it is in force.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Remove the directive, so the configuration's own formulas hold.
    Clear {
        #[arg(long, default_value = "by hand")]
        reason: String,
    },
    /// Print what the launcher honours now, as key=value lines.
    Effective,
    /// Print each note and stop notice for the tick of `--tick` working `--issue` it has not seen.
    Notes {
        #[arg(long)]
        issue: u64,
        #[arg(long)]
        tick: u32,
    },
    /// Send TERM to each stopped tick whose grace has passed, once it is verified.
    Enforce,
}

pub fn run(args: &Args) -> ExitCode {
    let env = supervisor::config_lookup();
    let result = Paths::from_env(&env).and_then(|paths| match &args.command {
        Sub::Apply { file } => apply_command(&env, &paths, file),
        Sub::Show { json } => show(&env, &paths, *json),
        Sub::Clear { reason } => clear(&paths, reason),
        Sub::Effective => effective(&env, &paths),
        Sub::Notes { issue, tick } => notes(&env, &paths, *issue, *tick),
        Sub::Enforce => enforce(&env, &paths),
    });
    result.unwrap_or_else(|message| {
        eprintln!("harness directive: {message}");
        ExitCode::from(2)
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    version: u32,
    reason: String,
    expires_at: String,
    pause: Option<PauseInput>,
    width: Option<u32>,
    models: Option<Vec<String>>,
    wake_at: Option<String>,
    focus: Option<Vec<u64>>,
    avoid: Option<Vec<u64>>,
    notes: Option<BTreeMap<String, String>>,
    stop: Option<Vec<StopInput>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PauseInput {
    until: String,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StopInput {
    pid: u32,
    reason: String,
}

/// A directive as `apply` stored it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Directive {
    pub version: u32,
    pub reason: String,
    pub applied_at: String,
    pub expires_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<Pause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub focus: Vec<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub avoid: Vec<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub notes: BTreeMap<u64, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<Stop>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pause {
    pub until: String,
    pub reason: String,
}

/// A tick to stop: its pid, the start time that proves it is still that process, and when the
/// grace its note gave it ends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stop {
    pub pid: u32,
    pub reason: String,
    pub start: u64,
    pub due_at: String,
}

/// A live tick of this installation, as `harness ticks` finds it, with its start time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveTick {
    pub pid: u32,
    pub start: u64,
    pub supervisor: bool,
}

/// What a directive is validated against.
#[derive(Clone, Debug)]
pub struct Context {
    pub now: u64,
    pub max_directive_s: u64,
    pub stop_grace_s: u64,
    pub width: u32,
    pub models: Vec<Entry>,
    pub default_agent: String,
}

impl Context {
    pub fn from_env(env: Lookup, paths: &Paths, now: u64) -> Result<Self, String> {
        let settings = supervisor::Settings::from_env(env)?;
        Ok(Self {
            now,
            max_directive_s: settings.max_directive_s,
            stop_grace_s: settings.stop_grace_s,
            width: usage::Settings::from_env(env)?.width,
            models: supervisor::configured_models(env, &paths.state)?,
            default_agent: env("TICK_AGENT").unwrap_or_else(|| "claude".to_owned()),
        })
    }
}

/// One line of text for the record: trimmed, not empty, at most `limit` characters, without a
/// control or invisible character, and holding nothing the secret masks would cover.
fn text(field: &str, value: &str, limit: usize) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{field}: empty"));
    }
    if value.chars().count() > limit {
        return Err(format!("{field}: more than {limit} characters"));
    }
    if value.chars().any(|c| c.is_control() || invisible(c)) {
        return Err(format!(
            "{field}: holds a control or invisible character; one line of plain text"
        ));
    }
    if redact(value) != value {
        return Err(format!("{field}: looks like it holds a secret"));
    }
    Ok(value.to_owned())
}

fn time(field: &str, value: &str) -> Result<u64, String> {
    epoch_of(value).ok_or_else(|| format!("{field}: not an ISO-8601 time"))
}

fn issues(field: &str, list: &[u64]) -> Result<Vec<u64>, String> {
    if list.len() > MAX_ISSUES {
        return Err(format!("{field}: more than {MAX_ISSUES} issues"));
    }
    let mut seen = Vec::new();
    for issue in list {
        if *issue == 0 {
            return Err(format!("{field}: 0 is not an issue number"));
        }
        if seen.contains(issue) {
            return Err(format!("{field}: #{issue} is listed twice"));
        }
        seen.push(*issue);
    }
    Ok(seen)
}

/// The models, each an entry of the configured list, none twice.
fn models(list: &[String], context: &Context) -> Result<Vec<String>, String> {
    if list.is_empty() {
        return Err("models: empty; leave it out to keep the configured list".to_owned());
    }
    let mut chosen: Vec<String> = Vec::new();
    for text in list {
        let entry = Entry::parse(text.trim(), &context.default_agent)
            .map_err(|message| format!("models: {message}"))?;
        if !context.models.contains(&entry) {
            return Err(format!(
                "models: {entry} is not in the configured model list"
            ));
        }
        let entry = entry.to_string();
        if chosen.contains(&entry) {
            return Err(format!("models: {entry} is listed twice"));
        }
        chosen.push(entry);
    }
    Ok(chosen)
}

/// Validates and clamps `bytes`, a directive's JSON. Returns the directive to store and what was
/// clamped. `previous` keeps a stop's grace from restarting when the same tick is named again.
pub fn validate(
    bytes: &[u8],
    context: &Context,
    live: &[LiveTick],
    previous: Option<&Directive>,
) -> Result<(Directive, Vec<String>), String> {
    let input: Input =
        serde_json::from_slice(bytes).map_err(|error| format!("not a directive: {error}"))?;
    if input.version != 1 {
        return Err(format!("version: {} is not 1", input.version));
    }
    let now = context.now;
    let mut clamped = Vec::new();
    let reason = text("reason", &input.reason, REASON_CHARS)?;
    let mut expires = time("expires_at", &input.expires_at)?;
    if expires <= now {
        return Err("expires_at: not in the future".to_owned());
    }
    let latest = now.saturating_add(context.max_directive_s);
    if expires > latest {
        expires = latest;
        clamped.push(format!(
            "expires_at to {} (SUPERVISOR_MAX_DIRECTIVE_S)",
            iso_of(expires)
        ));
    }
    let bounded = |field: &str, value: &str, clamped: &mut Vec<String>| {
        let at = time(field, value)?;
        if at <= now {
            return Err(format!("{field}: not in the future"));
        }
        if at > expires {
            clamped.push(format!("{field} to expires_at"));
            return Ok(expires);
        }
        Ok(at)
    };
    let pause = match input.pause {
        Some(pause) => Some(Pause {
            until: iso_of(bounded("pause.until", &pause.until, &mut clamped)?),
            reason: text("pause.reason", &pause.reason, REASON_CHARS)?,
        }),
        None => None,
    };
    let wake_at = match input.wake_at {
        Some(at) => Some(iso_of(bounded("wake_at", &at, &mut clamped)?)),
        None => None,
    };
    let width = input.width.map(|width| {
        if width > context.width {
            clamped.push(format!("width to {} (the configured width)", context.width));
            context.width
        } else {
            width
        }
    });
    let models = input
        .models
        .as_deref()
        .map(|list| models(list, context))
        .transpose()?;
    let focus = issues("focus", input.focus.as_deref().unwrap_or_default())?;
    let avoid = issues("avoid", input.avoid.as_deref().unwrap_or_default())?;
    if let Some(issue) = focus.iter().find(|issue| avoid.contains(issue)) {
        return Err(format!("focus and avoid both name #{issue}"));
    }
    let mut notes = BTreeMap::new();
    let given = input.notes.unwrap_or_default();
    if given.len() > MAX_NOTES {
        return Err(format!("notes: more than {MAX_NOTES}"));
    }
    for (issue, note) in &given {
        let number: u64 = issue
            .parse()
            .ok()
            .filter(|number| *number > 0)
            .ok_or_else(|| "notes: a key is not an issue number".to_owned())?;
        notes.insert(number, text(&format!("notes.{number}"), note, NOTE_CHARS)?);
    }
    let given = input.stop.unwrap_or_default();
    if given.len() > MAX_STOPS {
        return Err(format!("stop: more than {MAX_STOPS}"));
    }
    let mut stop: Vec<Stop> = Vec::new();
    for entry in given {
        let reason = text(
            &format!("stop.{}.reason", entry.pid),
            &entry.reason,
            REASON_CHARS,
        )?;
        if stop.iter().any(|known| known.pid == entry.pid) {
            return Err(format!("stop: pid {} is listed twice", entry.pid));
        }
        let tick = live
            .iter()
            .find(|tick| tick.pid == entry.pid)
            .ok_or_else(|| {
                format!(
                    "stop: pid {} is not a live tick of this installation",
                    entry.pid
                )
            })?;
        if tick.supervisor {
            return Err(format!("stop: pid {} is the supervisor itself", entry.pid));
        }
        let due = previous
            .and_then(|previous| {
                previous
                    .stop
                    .iter()
                    .find(|known| known.pid == tick.pid && known.start == tick.start)
            })
            .map_or_else(
                || Ok(iso_of(now.saturating_add(context.stop_grace_s))),
                |known| Ok::<_, String>(known.due_at.clone()),
            )?;
        if time("stop.due_at", &due)? > expires {
            return Err(format!(
                "stop: pid {}'s grace (SUPERVISOR_STOP_GRACE_S) ends after expires_at",
                entry.pid
            ));
        }
        stop.push(Stop {
            pid: entry.pid,
            reason,
            start: tick.start,
            due_at: due,
        });
    }
    Ok((
        Directive {
            version: 1,
            reason,
            applied_at: iso_of(now),
            expires_at: iso_of(expires),
            pause,
            width,
            models,
            wake_at,
            focus,
            avoid,
            notes,
            stop,
        },
        clamped,
    ))
}

/// Whether a directive is in force.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    None,
    Active(Directive),
    Expired(Directive),
    Invalid(String),
}

fn read_stored(paths: &Paths) -> Result<Option<Directive>, String> {
    let path = paths.directive();
    if !path.exists() {
        return Ok(None);
    }
    let text = files::read_text_limited(&path, INPUT_LIMIT)
        .map_err(|error| format!("directive.json: {error}"))?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| format!("directive.json: {error}"))
}

/// The stored directive checked again against the configuration as it is now: a hand-edited file,
/// or a model list changed since, makes it invalid.
pub fn check_stored(directive: &Directive, context: &Context) -> Result<(), String> {
    if directive.version != 1 {
        return Err(format!("version: {} is not 1", directive.version));
    }
    text("reason", &directive.reason, REASON_CHARS)?;
    let applied = time("applied_at", &directive.applied_at)?;
    let expires = time("expires_at", &directive.expires_at)?;
    if expires > applied.saturating_add(context.max_directive_s) {
        return Err("expires_at: past SUPERVISOR_MAX_DIRECTIVE_S".to_owned());
    }
    if let Some(pause) = &directive.pause {
        time("pause.until", &pause.until)?;
        text("pause.reason", &pause.reason, REASON_CHARS)?;
    }
    if let Some(at) = &directive.wake_at {
        time("wake_at", at)?;
    }
    if directive.width.is_some_and(|width| width > context.width) {
        return Err("width: over the configured width".to_owned());
    }
    if let Some(list) = &directive.models {
        models(list, context)?;
    }
    issues("focus", &directive.focus)?;
    issues("avoid", &directive.avoid)?;
    for (issue, note) in &directive.notes {
        text(&format!("notes.{issue}"), note, NOTE_CHARS)?;
    }
    for stop in &directive.stop {
        text("stop.reason", &stop.reason, REASON_CHARS)?;
        time("stop.due_at", &stop.due_at)?;
    }
    Ok(())
}

pub fn status(paths: &Paths, context: &Context) -> Status {
    match read_stored(paths) {
        Ok(None) => Status::None,
        Err(error) => Status::Invalid(error),
        Ok(Some(directive)) => match check_stored(&directive, context) {
            Err(error) => Status::Invalid(error),
            Ok(()) if epoch_of(&directive.expires_at).is_some_and(|at| at <= context.now) => {
                Status::Expired(directive)
            }
            Ok(()) => Status::Active(directive),
        },
    }
}

/// The stored directive's expiry, for the supervisor's events; nothing when it does not read.
pub fn stored_expiry(paths: &Paths) -> Option<u64> {
    epoch_of(&read_stored(paths).ok()??.expires_at)
}

/// Appends lines to the directive log, each masked once more.
fn log(paths: &Paths, now: u64, lines: &[String]) -> Result<(), String> {
    let path = paths.directive_log();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    }
    let stamp = iso_of(now);
    let mut text = String::new();
    for line in lines {
        text.push_str(&stamp);
        text.push(' ');
        text.push_str(&redact(line));
        text.push('\n');
    }
    files::append(&path, &text).map_err(|error| format!("{}: {error}", path.display()))
}

/// Holds `<state>/locks/directive.lock` while it lives, so two writers never interleave.
struct Lock {
    _held: File,
}

impl Lock {
    fn take(paths: &Paths) -> Result<Self, String> {
        let dir = paths.state.join("locks");
        std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        let path = dir.join("directive.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(Self { _held: file })
    }
}

/// A field's value as the log shows it.
fn field_text(value: &Value) -> String {
    match value {
        Value::Null => "none".to_owned(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// One `<field>: <old> -> <new>` line per field that changed, the reason and times left out.
fn diff(previous: Option<&Directive>, next: Option<&Directive>) -> Vec<String> {
    let as_map = |directive: Option<&Directive>| match directive.map(serde_json::to_value) {
        Some(Ok(Value::Object(map))) => map,
        _ => serde_json::Map::new(),
    };
    let (old, new) = (as_map(previous), as_map(next));
    let mut keys: Vec<&String> = old.keys().chain(new.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|key| !matches!(key.as_str(), "version" | "reason" | "applied_at"))
        .filter_map(|key| {
            let (before, after) = (
                old.get(key).unwrap_or(&Value::Null),
                new.get(key).unwrap_or(&Value::Null),
            );
            (before != after)
                .then(|| format!("  {key}: {} -> {}", field_text(before), field_text(after)))
        })
        .collect()
}

/// The live ticks of this installation with their start times.
pub fn live_ticks(paths: &Paths) -> Vec<LiveTick> {
    ticks::scan(&paths.state.to_string_lossy())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|tick| {
            let (start, _) = stat(tick.pid)?;
            Some(LiveTick {
                pid: tick.pid,
                start,
                supervisor: tick.kind == Kind::Supervisor,
            })
        })
        .collect()
}

/// A process's start time in clock ticks and its process group, from `/proc/<pid>/stat`. The
/// command name may hold spaces and parentheses, so the fields are read after the last `)`.
fn stat(pid: u32) -> Option<(u64, u32)> {
    let mut bytes = Vec::new();
    File::open(format!("/proc/{pid}/stat"))
        .ok()?
        .take(STAT_LIMIT)
        .read_to_end(&mut bytes)
        .ok()?;
    parse_stat(&String::from_utf8_lossy(&bytes))
}

fn parse_stat(text: &str) -> Option<(u64, u32)> {
    let (_, rest) = text.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After the name: state, ppid, pgrp, ..., starttime as the 20th.
    Some((fields.get(19)?.parse().ok()?, fields.get(2)?.parse().ok()?))
}

fn read_input(file: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let read = if file == "-" {
        std::io::stdin()
            .take(INPUT_LIMIT + 1)
            .read_to_end(&mut bytes)
    } else {
        File::open(file).and_then(|handle| handle.take(INPUT_LIMIT + 1).read_to_end(&mut bytes))
    };
    read.map_err(|error| format!("{file}: {error}"))?;
    if bytes.len() as u64 > INPUT_LIMIT {
        return Err(format!("{file}: over {INPUT_LIMIT} bytes"));
    }
    Ok(bytes)
}

/// Validates, writes and logs `bytes`; a refusal is logged too, without the input.
pub fn apply(
    paths: &Paths,
    bytes: &[u8],
    context: &Context,
    live: &[LiveTick],
) -> Result<(Directive, Vec<String>), String> {
    let _lock = Lock::take(paths)?;
    let previous = read_stored(paths).ok().flatten();
    let (directive, clamped) = match validate(bytes, context, live, previous.as_ref()) {
        Ok(done) => done,
        Err(error) => {
            log(paths, context.now, &[format!("refused: {error}")])?;
            return Err(error);
        }
    };
    let mut body = serde_json::to_string_pretty(&directive).map_err(|error| error.to_string())?;
    body.push('\n');
    files::write_replacing(&paths.directive(), &body)
        .map_err(|error| format!("{}: {error}", paths.directive().display()))?;
    let mut lines = vec![format!(
        "apply: {} (until {})",
        directive.reason, directive.expires_at
    )];
    lines.extend(diff(previous.as_ref(), Some(&directive)));
    lines.extend(clamped.iter().map(|what| format!("  clamped {what}")));
    log(paths, context.now, &lines)?;
    Ok((directive, clamped))
}

fn apply_command(env: Lookup, paths: &Paths, file: &str) -> Result<ExitCode, String> {
    let bytes = read_input(file)?;
    let context = Context::from_env(env, paths, now_s())?;
    match apply(paths, &bytes, &context, &live_ticks(paths)) {
        Ok((directive, clamped)) => {
            println!("applied until {}", directive.expires_at);
            for what in clamped {
                println!("clamped {what}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            eprintln!("harness directive: refused: {error}");
            Ok(ExitCode::from(1))
        }
    }
}

fn status_name(status: &Status) -> &'static str {
    match status {
        Status::None => "none",
        Status::Active(_) => "active",
        Status::Expired(_) => "expired",
        Status::Invalid(_) => "invalid",
    }
}

/// The directive and its state, as `show --json`, `harness health` and the dashboard give it.
pub fn view(env: Lookup, paths: &Paths, now: u64) -> Value {
    let status = match Context::from_env(env, paths, now) {
        Ok(context) => status(paths, &context),
        Err(error) => Status::Invalid(error),
    };
    let directive = match &status {
        Status::Active(directive) | Status::Expired(directive) => {
            serde_json::to_value(directive).unwrap_or(Value::Null)
        }
        _ => Value::Null,
    };
    let stopped = |stop: &Stop| {
        paths
            .file(&format!("stopped/{}.{}", stop.pid, stop.start))
            .exists()
    };
    let stops: Vec<Value> = match &status {
        Status::Active(directive) => directive
            .stop
            .iter()
            .map(|stop| json!({"pid": stop.pid, "due_at": stop.due_at, "reason": stop.reason, "sent": stopped(stop)}))
            .collect(),
        _ => Vec::new(),
    };
    json!({
        "status": status_name(&status),
        "error": match &status { Status::Invalid(error) => Some(error.clone()), _ => None },
        "directive": directive,
        "stops": stops,
    })
}

fn show(env: Lookup, paths: &Paths, as_json: bool) -> Result<ExitCode, String> {
    let view = view(env, paths, now_s());
    if as_json {
        println!("{view}");
        return Ok(ExitCode::SUCCESS);
    }
    match view["status"].as_str() {
        Some("none") => println!("no directive: the configuration's own formulas hold"),
        Some("invalid") => println!(
            "invalid, ignored: {}",
            view["error"].as_str().unwrap_or_default()
        ),
        Some(state) => {
            println!(
                "{state}: {}",
                serde_json::to_string_pretty(&view["directive"])
                    .map_err(|error| error.to_string())?
            );
        }
        None => {}
    }
    Ok(ExitCode::SUCCESS)
}

fn clear(paths: &Paths, reason: &str) -> Result<ExitCode, String> {
    let reason = text("--reason", reason, REASON_CHARS)?;
    let _lock = Lock::take(paths)?;
    let previous = read_stored(paths).ok().flatten();
    match std::fs::remove_file(paths.directive()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("no directive");
            return Ok(ExitCode::SUCCESS);
        }
        Err(error) => return Err(format!("{}: {error}", paths.directive().display())),
    }
    let mut lines = vec![format!("clear: {reason}")];
    lines.extend(diff(previous.as_ref(), None));
    log(paths, now_s(), &lines)?;
    println!("cleared");
    Ok(ExitCode::SUCCESS)
}

/// Logs an expired or invalid directive the first time the launcher meets it, not at each firing.
fn log_once(paths: &Paths, now: u64, status: &Status) -> Result<(), String> {
    let (mark, line) = match status {
        Status::Invalid(error) => (
            format!("invalid {}", &sha256_hex(error.as_bytes())[..16]),
            Some(format!("ignored, defaults in force: {error}")),
        ),
        Status::Expired(directive) => (
            format!("expired {}", directive.expires_at),
            Some(format!(
                "expired at {}, defaults in force",
                directive.expires_at
            )),
        ),
        Status::Active(directive) => (format!("active {}", directive.applied_at), None),
        Status::None => ("none".to_owned(), None),
    };
    paths.prepare()?;
    let stamp = paths.file("directive-status");
    if files::read_text_limited(&stamp, 4096)
        .unwrap_or_default()
        .trim()
        == mark
    {
        return Ok(());
    }
    if let Some(line) = line {
        log(paths, now, &[line])?;
    }
    files::write_replacing(&stamp, &format!("{mark}\n"))
        .map_err(|error| format!("{}: {error}", stamp.display()))
}

/// A value for a key=value line: one line, whatever it held.
fn one_line(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// What the launcher honours, for `effective` to print. `woken` names the wake events since the
/// directive was applied, which end its pause or its quiet before `wake_at`.
pub fn effective_lines(status: &Status, woken: &[EventKind], now: u64) -> Vec<String> {
    let mut lines = vec![format!("status={}", status_name(status))];
    match status {
        Status::Invalid(error) => lines.push(format!("error={}", one_line(error))),
        Status::Active(directive) => {
            lines.push(format!("reason={}", directive.reason));
            lines.push(format!("expires_at={}", directive.expires_at));
            let pause = directive
                .pause
                .as_ref()
                .filter(|pause| epoch_of(&pause.until).is_some_and(|at| at > now));
            let wake = directive
                .wake_at
                .as_ref()
                .filter(|at| epoch_of(at).is_some_and(|at| at > now));
            let quiet = match (pause, wake) {
                (Some(pause), _) => Some(("pause", pause.until.clone(), pause.reason.clone())),
                (None, Some(at)) => Some(("wake", at.clone(), directive.reason.clone())),
                (None, None) => None,
            };
            if let Some((kind, until, why)) = quiet {
                if woken.is_empty() {
                    lines.push(format!("quiet={kind}"));
                    lines.push(format!("until={until}"));
                    lines.push(format!("quiet_reason={why}"));
                } else {
                    let names: Vec<&str> = woken.iter().map(|kind| kind.name()).collect();
                    lines.push(format!("woken={}", names.join(",")));
                }
            }
            if let Some(width) = directive.width {
                lines.push(format!("width={width}"));
            }
            if let Some(models) = &directive.models {
                lines.push(format!("models={}", models.join(" ")));
            }
            let numbers = |list: &[u64]| {
                list.iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            if !directive.focus.is_empty() {
                lines.push(format!("focus={}", numbers(&directive.focus)));
            }
            if !directive.avoid.is_empty() {
                lines.push(format!("avoid={}", numbers(&directive.avoid)));
            }
        }
        Status::None | Status::Expired(_) => {}
    }
    lines
}

fn effective(env: Lookup, paths: &Paths) -> Result<ExitCode, String> {
    let now = now_s();
    let status = match Context::from_env(env, paths, now) {
        Ok(context) => status(paths, &context),
        Err(error) => Status::Invalid(error),
    };
    log_once(paths, now, &status)?;
    let mut woken = Vec::new();
    if let Status::Active(directive) = &status
        && let Some(applied) = epoch_of(&directive.applied_at)
        && (directive.pause.is_some() || directive.wake_at.is_some())
    {
        let settings = supervisor::Settings::from_env(env)?;
        let found =
            supervisor::events(&supervisor::gather(paths, applied), &settings, applied, now);
        woken = found
            .iter()
            .map(|event| event.kind)
            .filter(|kind| kind.wakes())
            .collect();
        woken.sort();
        woken.dedup();
    }
    for line in effective_lines(&status, &woken, now) {
        println!("{line}");
    }
    Ok(ExitCode::SUCCESS)
}

/// The notes and stop notice for the tick `tick` working `issue`, each once.
pub fn pending_notes(directive: &Directive, issue: u64, tick: u32) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(note) = directive.notes.get(&issue) {
        lines.push(format!("SUPERVISOR: on #{issue}: {note}"));
    }
    for stop in directive.stop.iter().filter(|stop| stop.pid == tick) {
        lines.push(format!(
            "SUPERVISOR: this tick is stopped at {}: {}. Before then start nothing new, tear down, post the pause with the resume recipe and end.",
            stop.due_at, stop.reason
        ));
    }
    lines
}

fn notes(env: Lookup, paths: &Paths, issue: u64, tick: u32) -> Result<ExitCode, String> {
    let context = Context::from_env(env, paths, now_s())?;
    let Status::Active(directive) = status(paths, &context) else {
        return Ok(ExitCode::SUCCESS);
    };
    let dir = paths.dir().join("notes-seen");
    std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let seen_path = dir.join(format!("{issue}.{tick}"));
    let seen = files::read_text_limited(&seen_path, 1 << 16).unwrap_or_default();
    let mut fresh = String::new();
    for line in pending_notes(&directive, issue, tick) {
        let mark = &sha256_hex(line.as_bytes())[..16];
        if seen.lines().any(|known| known == mark) {
            continue;
        }
        println!("{line}");
        fresh.push_str(mark);
        fresh.push('\n');
    }
    if !fresh.is_empty() {
        files::append(&seen_path, &fresh)
            .map_err(|error| format!("{}: {error}", seen_path.display()))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// What became of one stop whose grace ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Verified: TERM goes to this process group.
    Signal(u32),
    Skip(String),
}

/// Whether `stop` may be sent: the pid is still a live tick of this installation, not the
/// supervisor, with the start time recorded when it was named, in a process group of its own.
pub fn verify(stop: &Stop, live: &[LiveTick], group: Option<u32>, own_group: u32) -> Verdict {
    let Some(tick) = live.iter().find(|tick| tick.pid == stop.pid) else {
        return Verdict::Skip("no longer a live tick of this installation".to_owned());
    };
    if tick.supervisor {
        return Verdict::Skip("the supervisor itself".to_owned());
    }
    if tick.start != stop.start {
        return Verdict::Skip("its pid now names another process".to_owned());
    }
    match group {
        Some(group) if group > 1 && group != own_group => Verdict::Signal(group),
        _ => Verdict::Skip("its process group is not its own".to_owned()),
    }
}

fn enforce(env: Lookup, paths: &Paths) -> Result<ExitCode, String> {
    let now = now_s();
    let context = Context::from_env(env, paths, now)?;
    let Status::Active(directive) = status(paths, &context) else {
        return Ok(ExitCode::SUCCESS);
    };
    let due: Vec<&Stop> = directive
        .stop
        .iter()
        .filter(|stop| epoch_of(&stop.due_at).is_some_and(|at| at <= now))
        .filter(|stop| {
            !paths
                .file(&format!("stopped/{}.{}", stop.pid, stop.start))
                .exists()
        })
        .collect();
    if due.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    let dir = paths.dir().join("stopped");
    std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let live = live_ticks(paths);
    let own_group = u32::try_from(rustix::process::getpgrp().as_raw_nonzero().get()).unwrap_or(0);
    for stop in due {
        let line = match verify(
            stop,
            &live,
            stat(stop.pid).map(|(_, group)| group),
            own_group,
        ) {
            Verdict::Signal(group) => {
                let sent = i32::try_from(group)
                    .ok()
                    .and_then(rustix::process::Pid::from_raw)
                    .map(|group| {
                        rustix::process::kill_process_group(group, rustix::process::Signal::TERM)
                    });
                match sent {
                    Some(Ok(())) => format!(
                        "stop: TERM to pid {} (process group {group}) after its grace: {}",
                        stop.pid, stop.reason
                    ),
                    Some(Err(error)) => format!("stop: pid {} not signalled: {error}", stop.pid),
                    None => format!("stop: pid {} not signalled: no such group", stop.pid),
                }
            }
            Verdict::Skip(why) => format!("stop: pid {} skipped: {why}", stop.pid),
        };
        let mark = dir.join(format!("{}.{}", stop.pid, stop.start));
        let _ = File::create(&mark).and_then(|mut file| file.write_all(b"\n"));
        log(paths, now, &[line])?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::testkit::TempDir;

    const NOW: u64 = 1_800_000_000;

    fn context() -> Context {
        Context {
            now: NOW,
            max_directive_s: 21_600,
            stop_grace_s: 600,
            width: 4,
            models: Entry::list("claude:opus codex:gpt gemini:pro", "claude").unwrap(),
            default_agent: "claude".to_owned(),
        }
    }

    fn at(offset: u64) -> String {
        iso_of(NOW + offset)
    }

    fn live() -> Vec<LiveTick> {
        vec![
            LiveTick {
                pid: 4242,
                start: 99,
                supervisor: false,
            },
            LiveTick {
                pid: 4343,
                start: 7,
                supervisor: true,
            },
        ]
    }

    fn check(directive: &Value) -> Result<(Directive, Vec<String>), String> {
        validate(directive.to_string().as_bytes(), &context(), &live(), None)
    }

    fn base() -> Value {
        json!({"version": 1, "reason": "ticks loop on one issue", "expires_at": at(3600)})
    }

    fn with(key: &str, value: Value) -> Value {
        let mut directive = base();
        directive[key] = value;
        directive
    }

    fn refused(directive: &Value, needle: &str) {
        match check(directive) {
            Err(error) => assert!(error.contains(needle), "{error:?} lacks {needle:?}"),
            Ok(done) => panic!("accepted {directive}: {done:?}"),
        }
    }

    #[test]
    fn the_smallest_directive_is_accepted_as_it_is() {
        let (directive, clamped) = check(&base()).unwrap();
        assert!(clamped.is_empty());
        assert_eq!(directive.applied_at, iso_of(NOW));
        assert_eq!(directive.expires_at, at(3600));
        assert_eq!(directive.width, None);
    }

    #[test]
    fn unknown_fields_are_refused_at_every_level() {
        refused(&with("speed", json!(2)), "unknown field");
        refused(
            &with("pause", json!({"until": at(60), "reason": "x", "extra": 1})),
            "unknown field",
        );
        refused(
            &with("stop", json!([{"pid": 4242, "reason": "x", "signal": 9}])),
            "unknown field",
        );
        // A stored-only field is not the supervisor's to set.
        refused(&with("applied_at", json!(at(0))), "unknown field");
    }

    #[test]
    fn version_reason_and_expiry_are_required_and_bounded() {
        refused(&with("version", json!(2)), "version");
        refused(&json!({"version": 1, "expires_at": at(60)}), "reason");
        refused(&json!({"version": 1, "reason": "r"}), "expires_at");
        refused(&with("reason", json!("  ")), "reason: empty");
        refused(&with("reason", json!("two\nlines")), "one line");
        refused(&with("reason", json!("x".repeat(201))), "more than 200");
        refused(&with("reason", json!("hidden\u{200b}text")), "invisible");
        refused(
            &with(
                "reason",
                json!("GH_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
            ),
            "secret",
        );
        refused(&with("expires_at", json!("tomorrow")), "ISO-8601");
        refused(&with("expires_at", json!(iso_of(NOW))), "not in the future");
        let (directive, clamped) = check(&with("expires_at", json!(at(7 * 3600)))).unwrap();
        assert_eq!(directive.expires_at, at(21_600));
        assert_eq!(clamped.len(), 1);
    }

    #[test]
    fn pause_and_wake_at_are_bounded_by_the_expiry() {
        refused(
            &with("pause", json!({"until": iso_of(NOW - 1), "reason": "x"})),
            "pause.until: not in the future",
        );
        refused(
            &with("pause", json!({"until": at(60), "reason": ""})),
            "pause.reason",
        );
        refused(&with("pause", json!({"until": at(60)})), "reason");
        refused(
            &with("wake_at", json!(iso_of(NOW))),
            "wake_at: not in the future",
        );
        let (directive, clamped) =
            check(&with("pause", json!({"until": at(9000), "reason": "hold"}))).unwrap();
        assert_eq!(directive.pause.unwrap().until, at(3600));
        assert_eq!(clamped, ["pause.until to expires_at"]);
        let (directive, _) = check(&with("wake_at", json!(at(9000)))).unwrap();
        assert_eq!(directive.wake_at.unwrap(), at(3600));
        let (directive, clamped) = check(&with("wake_at", json!(at(60)))).unwrap();
        assert_eq!(directive.wake_at.unwrap(), at(60));
        assert!(clamped.is_empty());
    }

    #[test]
    fn width_is_clamped_to_the_configured_width() {
        assert_eq!(check(&with("width", json!(0))).unwrap().0.width, Some(0));
        assert_eq!(check(&with("width", json!(3))).unwrap().0.width, Some(3));
        let (directive, clamped) = check(&with("width", json!(40))).unwrap();
        assert_eq!(directive.width, Some(4));
        assert_eq!(clamped.len(), 1);
        refused(&with("width", json!(-1)), "not a directive");
        refused(&with("width", json!(1.5)), "not a directive");
        refused(&with("width", json!("2")), "not a directive");
    }

    #[test]
    fn models_are_a_subset_or_reordering_of_the_configured_list() {
        let (directive, _) = check(&with("models", json!(["codex:gpt", "opus"]))).unwrap();
        assert_eq!(directive.models.unwrap(), ["codex:gpt", "claude:opus"]);
        refused(
            &with("models", json!(["claude:sonnet"])),
            "not in the configured model list",
        );
        refused(
            &with("models", json!(["other:opus"])),
            "not in the configured model list",
        );
        refused(
            &with("models", json!(["opus", "claude:opus"])),
            "listed twice",
        );
        refused(&with("models", json!([])), "empty");
        refused(&with("models", json!(["bad agent:x"])), "models");
    }

    #[test]
    fn focus_avoid_and_notes_name_issues() {
        let (directive, _) = check(&json!({
            "version": 1, "reason": "r", "expires_at": at(60),
            "focus": [12, 15], "avoid": [7], "notes": {"7": "this loops; pause it"}
        }))
        .unwrap();
        assert_eq!(directive.focus, [12, 15]);
        assert_eq!(directive.notes[&7], "this loops; pause it");
        refused(&with("focus", json!([0])), "0 is not an issue");
        refused(&with("avoid", json!([3, 3])), "listed twice");
        refused(
            &with("focus", json!((1..=51).collect::<Vec<u64>>())),
            "more than 50",
        );
        refused(
            &json!({"version": 1, "reason": "r", "expires_at": at(60), "focus": [3], "avoid": [3]}),
            "both name #3",
        );
        refused(&with("notes", json!({"x": "y"})), "not an issue number");
        refused(&with("notes", json!({"0": "y"})), "not an issue number");
        refused(
            &with("notes", json!({"5": "x".repeat(501)})),
            "more than 500",
        );
        let many: serde_json::Map<String, Value> =
            (1..=21).map(|n| (n.to_string(), json!("n"))).collect();
        refused(&with("notes", Value::Object(many)), "more than 20");
    }

    #[test]
    fn stop_names_only_a_live_tick_of_this_installation() {
        let (directive, _) = check(&with(
            "stop",
            json!([{"pid": 4242, "reason": "loops on #7"}]),
        ))
        .unwrap();
        assert_eq!(
            directive.stop,
            [Stop {
                pid: 4242,
                reason: "loops on #7".to_owned(),
                start: 99,
                due_at: at(600)
            }]
        );
        refused(
            &with("stop", json!([{"pid": 999, "reason": "x"}])),
            "not a live tick",
        );
        refused(
            &with("stop", json!([{"pid": 4343, "reason": "x"}])),
            "the supervisor itself",
        );
        refused(
            &with(
                "stop",
                json!([{"pid": 4242, "reason": "x"}, {"pid": 4242, "reason": "y"}]),
            ),
            "twice",
        );
        refused(
            &with("stop", json!([{"pid": -1, "reason": "x"}])),
            "not a directive",
        );
        let short = json!({"version": 1, "reason": "r", "expires_at": at(300), "stop": [{"pid": 4242, "reason": "x"}]});
        refused(&short, "grace");
    }

    #[test]
    fn naming_a_tick_again_keeps_its_grace() {
        let (first, _) = check(&with("stop", json!([{"pid": 4242, "reason": "x"}]))).unwrap();
        let later = Context {
            now: NOW + 300,
            ..context()
        };
        let again = with("stop", json!([{"pid": 4242, "reason": "still"}]));
        let (second, _) =
            validate(again.to_string().as_bytes(), &later, &live(), Some(&first)).unwrap();
        assert_eq!(second.stop[0].due_at, first.stop[0].due_at);
        // The same pid with another start time is another process: its grace starts now.
        let reused = vec![LiveTick {
            pid: 4242,
            start: 100,
            supervisor: false,
        }];
        let (third, _) =
            validate(again.to_string().as_bytes(), &later, &reused, Some(&first)).unwrap();
        assert_eq!(third.stop[0].due_at, iso_of(NOW + 900));
    }

    #[test]
    fn a_stop_is_sent_only_to_a_verified_tick() {
        let stop = Stop {
            pid: 4242,
            reason: "x".to_owned(),
            start: 99,
            due_at: at(0),
        };
        assert_eq!(verify(&stop, &live(), Some(4242), 1), Verdict::Signal(4242));
        assert!(
            matches!(verify(&stop, &[], Some(4242), 1), Verdict::Skip(why) if why.contains("no longer"))
        );
        let reused = [LiveTick {
            start: 100,
            ..live()[0].clone()
        }];
        assert!(
            matches!(verify(&stop, &reused, Some(4242), 1), Verdict::Skip(why) if why.contains("another process"))
        );
        assert!(matches!(
            verify(&stop, &live(), Some(1), 1),
            Verdict::Skip(_)
        ));
        assert!(matches!(
            verify(&stop, &live(), Some(77), 77),
            Verdict::Skip(_)
        ));
        assert!(matches!(verify(&stop, &live(), None, 1), Verdict::Skip(_)));
        let own = Stop {
            pid: 4343,
            start: 7,
            ..stop
        };
        assert!(
            matches!(verify(&own, &live(), Some(4343), 1), Verdict::Skip(why) if why.contains("supervisor"))
        );
    }

    #[test]
    fn stat_fields_are_read_after_the_last_parenthesis() {
        let stat = "42 (a) b (c) S 7 40 42 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 123456 0 0";
        assert_eq!(parse_stat(stat), Some((123_456, 40)));
        assert_eq!(parse_stat("42 (short) S 7"), None);
    }

    fn paths(dir: &TempDir) -> Paths {
        Paths {
            state: dir.path().to_path_buf(),
        }
    }

    #[test]
    fn apply_writes_atomically_owner_only_and_logs_the_diff() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = TempDir::new();
        let paths = paths(&dir);
        apply(&paths, base().to_string().as_bytes(), &context(), &[]).unwrap();
        let second = json!({"version": 1, "reason": "narrow", "expires_at": at(7200), "width": 9, "models": ["codex:gpt"]});
        apply(&paths, second.to_string().as_bytes(), &context(), &[]).unwrap();
        let mode = std::fs::metadata(paths.directive())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!paths.state.join("directive.json.new").exists());
        let stored: Directive =
            serde_json::from_str(&std::fs::read_to_string(paths.directive()).unwrap()).unwrap();
        assert_eq!(stored.width, Some(4));
        let log = std::fs::read_to_string(paths.directive_log()).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert!(lines[0].ends_with(&format!(
            "apply: ticks loop on one issue (until {})",
            at(3600)
        )));
        assert!(log.contains(&format!("  expires_at: {} -> {}", at(3600), at(7200))));
        assert!(log.contains("  width: none -> 4"));
        assert!(log.contains("  models: none -> [\"codex:gpt\"]"));
        assert!(log.contains("  clamped width to 4"));
        assert!(!log.contains("reason: ticks loop"));
        let refused = apply(&paths, b"{\"version\": 1, \"reason\": \"GH_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789\", \"expires_at\": \"x\"}", &context(), &[]);
        assert!(refused.is_err());
        let log = std::fs::read_to_string(paths.directive_log()).unwrap();
        assert!(
            log.lines()
                .last()
                .unwrap()
                .contains("refused: reason: looks like it holds a secret")
        );
        assert!(!log.contains("ghp_"));
        // The refusal leaves the directive in force as it was.
        assert_eq!(status(&paths, &context()), Status::Active(stored));
    }

    #[test]
    fn status_falls_back_on_expiry_and_on_a_file_that_no_longer_validates() {
        let dir = TempDir::new();
        let paths = paths(&dir);
        assert_eq!(status(&paths, &context()), Status::None);
        apply(
            &paths,
            with("models", json!(["gemini:pro"])).to_string().as_bytes(),
            &context(),
            &[],
        )
        .unwrap();
        assert!(matches!(status(&paths, &context()), Status::Active(_)));
        let later = Context {
            now: NOW + 3600,
            ..context()
        };
        assert!(matches!(status(&paths, &later), Status::Expired(_)));
        let narrowed = Context {
            models: Entry::list("claude:opus", "claude").unwrap(),
            ..context()
        };
        assert!(
            matches!(status(&paths, &narrowed), Status::Invalid(error) if error.contains("gemini:pro"))
        );
        std::fs::write(paths.directive(), "{\"version\": 1, \"surprise\": true}").unwrap();
        assert!(matches!(status(&paths, &context()), Status::Invalid(_)));
        std::fs::write(paths.directive(), "not json").unwrap();
        assert!(matches!(status(&paths, &context()), Status::Invalid(_)));
        let mut edited: Directive = check(&base()).unwrap().0;
        edited.expires_at = at(30 * 3600);
        std::fs::write(paths.directive(), serde_json::to_string(&edited).unwrap()).unwrap();
        assert!(
            matches!(status(&paths, &context()), Status::Invalid(error) if error.contains("MAX_DIRECTIVE"))
        );
    }

    #[test]
    fn an_invalid_or_expired_directive_is_logged_once() {
        let dir = TempDir::new();
        let paths = paths(&dir);
        let invalid = Status::Invalid("directive.json: oops".to_owned());
        for _ in 0..3 {
            log_once(&paths, NOW, &invalid).unwrap();
        }
        let (directive, _) = check(&base()).unwrap();
        for _ in 0..3 {
            log_once(&paths, NOW, &Status::Expired(directive.clone())).unwrap();
        }
        log_once(&paths, NOW, &Status::Active(directive)).unwrap();
        log_once(&paths, NOW, &invalid).unwrap();
        let log = std::fs::read_to_string(paths.directive_log()).unwrap();
        assert_eq!(log.matches("ignored, defaults in force").count(), 2);
        assert_eq!(log.matches("expired at").count(), 1);
    }

    #[test]
    fn effective_names_the_quiet_until_a_wake_event() {
        let directive = check(&json!({
            "version": 1, "reason": "every window is full", "expires_at": at(3600),
            "wake_at": at(1800), "width": 2, "models": ["codex:gpt"], "focus": [3], "avoid": [4, 5]
        }))
        .unwrap()
        .0;
        let lines = effective_lines(&Status::Active(directive.clone()), &[], NOW);
        assert_eq!(
            lines,
            [
                "status=active".to_owned(),
                "reason=every window is full".to_owned(),
                format!("expires_at={}", at(3600)),
                "quiet=wake".to_owned(),
                format!("until={}", at(1800)),
                "quiet_reason=every window is full".to_owned(),
                "width=2".to_owned(),
                "models=codex:gpt".to_owned(),
                "focus=3".to_owned(),
                "avoid=4 5".to_owned(),
            ]
        );
        let woken = effective_lines(&Status::Active(directive.clone()), &[EventKind::Reset], NOW);
        assert!(woken.contains(&"woken=reset".to_owned()));
        assert!(!woken.iter().any(|line| line.starts_with("quiet=")));
        let after = effective_lines(&Status::Active(directive), &[], NOW + 1800);
        assert!(!after.iter().any(|line| line.starts_with("quiet=")));
        assert_eq!(effective_lines(&Status::None, &[], NOW), ["status=none"]);
        assert_eq!(
            effective_lines(&Status::Invalid("a\nb".to_owned()), &[], NOW),
            ["status=invalid", "error=a b"]
        );
    }

    #[test]
    fn notes_reach_the_tick_on_their_issue_and_its_stop_notice_reaches_it() {
        let directive = check(&json!({
            "version": 1, "reason": "r", "expires_at": at(3600),
            "notes": {"7": "rebase first"}, "stop": [{"pid": 4242, "reason": "loops on #7"}]
        }))
        .unwrap()
        .0;
        let lines = pending_notes(&directive, 7, 4242);
        assert_eq!(lines[0], "SUPERVISOR: on #7: rebase first");
        assert!(lines[1].starts_with(&format!(
            "SUPERVISOR: this tick is stopped at {}: loops on #7.",
            at(600)
        )));
        assert!(pending_notes(&directive, 8, 1).is_empty());
    }
}
