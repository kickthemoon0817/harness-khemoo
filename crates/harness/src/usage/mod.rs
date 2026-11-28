//! `harness usage`: how many ticks each agent's usage windows allow, and which model runs next.
//!
//! Each agent of the model list has its own windows, read from the source `USAGE_SOURCE_<agent>`
//! names. A window under a day long steps down by `USAGE_STEPS_SHORT`, a longer one by
//! `USAGE_STEPS_LONG`: past the first point the agent may run half the width, past the second one
//! tick, past the third none. Ticks already running keep spending after spawning stops, so the steps
//! sit well under `USAGE_CEILING_PCT`, which no window may reach. An agent's cap is its tightest
//! window's; with no fresh reading it is 0, since the ceiling cannot be kept blind. The pool's cap is
//! the sum of the agents' caps, at most the width.
//!
//! An entry may name an account, `agent@account:model` (`crate::accounts`): each account then has
//! its own reading, cap, running count and cooldowns, and counts toward the pool as an agent of its
//! own would. An account found signed out has a cap of 0.
//!
//! Settings come from the environment, as `bin/tick.sh` and `libexec/usage-ok.sh` export them from
//! harness.env.

pub mod sources;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use crate::accounts::{Account, SignIn, SignState};
use crate::home::harness_home;
pub use sources::{Reading, Source, Window};

/// A window this long or longer is a long one.
const LONG_WINDOW_MINUTES: u64 = 24 * 60;
/// A fetch that hangs must not stall the launcher past its next firing.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// A reading dated further ahead than this is not one a clock skew explains.
const FUTURE_SLACK_MS: u64 = 300_000;

/// How the environment is read: the value of a name, `None` when it is unset or empty.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

#[derive(clap::Args)]
#[command(
    args_conflicts_with_subcommands = true,
    after_help = "\
With no subcommand, prints each agent and account of TICK_MODELS (and LANE_MODEL) with its windows
and cap, and the pool's cap. Reads TICK_MODELS, TICK_AGENT, LANE_MODEL, MAX_SLOTS, TICK_WIDTH,
AGENT_ACCOUNTS_DIR and the USAGE_* keys docs/INSTANCE.md gives from the environment."
)]
pub struct Args {
    #[command(subcommand)]
    command: Option<Sub>,
    /// Print a JSON document instead of lines.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Subcommand)]
enum Sub {
    /// Exit 0 while every window of the agent is under its stand-down point, 1 at or past one, and
    /// 2 without a fresh reading.
    Check {
        /// The agent, or `agent@account`.
        #[arg(long)]
        agent: String,
    },
    /// Print the best entry of the model list that is not cooling down and whose agent (or account)
    /// runs fewer ticks than its cap; exit 1 when there is none.
    Pick {
        /// Consider only this agent's entries, or with `agent@account` only that account's.
        #[arg(long)]
        agent: Option<String>,
        /// Entries already tried by this tick, space-separated.
        #[arg(long, default_value = "")]
        tried: String,
        /// Pick for a landing tick: its agent's cap must be above 0, however many of its ticks run.
        #[arg(long)]
        landing: bool,
        /// Consider only this entry, as the model lane does.
        #[arg(long)]
        entry: Option<String>,
    },
    /// Run the fetch command of each agent of the model list and the lane whose reading is missing
    /// or older than its refresh age. Nothing else refreshes a reading but `check`.
    Refresh,
}

pub fn run(args: &Args) -> ExitCode {
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let result = match &args.command {
        None => report(&env, args.json),
        Some(Sub::Check { agent }) => check(&env, agent),
        Some(Sub::Refresh) => refresh(&env),
        Some(Sub::Pick {
            agent,
            tried,
            landing,
            entry,
        }) => pick_command(
            &env,
            &PickArgs {
                agent: agent.as_deref(),
                tried,
                landing: *landing,
                entry: entry.as_deref(),
            },
        ),
    };
    result.unwrap_or_else(|message| {
        eprintln!("harness usage: {message}");
        ExitCode::from(2)
    })
}

/// The three points where a window drops to half the width, to one tick and to none.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Steps([u8; 3]);

impl Steps {
    pub fn parse(key: &str, text: &str) -> Result<Self, String> {
        let points: Vec<u8> = text
            .split_whitespace()
            .map(|word| word.parse().ok().filter(|point| *point <= 100))
            .collect::<Option<_>>()
            .ok_or_else(|| format!("{key}: '{text}' is not three percentages"))?;
        match points[..] {
            [half, one, none] if half <= one && one <= none => Ok(Self([half, one, none])),
            _ => Err(format!(
                "{key}: '{text}' is not three percentages in rising order"
            )),
        }
    }
}

/// The pool-wide settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub width: u32,
    pub ceiling: f64,
    pub stop_short: f64,
    pub stop_long: f64,
    pub steps_short: Steps,
    pub steps_long: Steps,
    pub max_age_s: u64,
    pub refresh_after_s: u64,
}

fn number<T: std::str::FromStr>(env: Lookup, key: &str, default: T) -> Result<T, String> {
    env(key).map_or(Ok(default), |text| {
        text.trim()
            .parse()
            .map_err(|_| format!("{key}: '{text}' is not a whole number"))
    })
}

impl Settings {
    pub fn from_env(env: Lookup) -> Result<Self, String> {
        let max_slots = number(env, "MAX_SLOTS", 8)?;
        let ceiling: u8 = number(env, "USAGE_CEILING_PCT", 90)?;
        let stop_short = number(env, "USAGE_TICK_STOP_PCT", ceiling.saturating_sub(2))?;
        let stop_long = number(env, "USAGE_TICK_STOP_LONG_PCT", stop_short)?;
        let steps = |key: &str, default: &str| {
            Steps::parse(key, &env(key).unwrap_or_else(|| default.to_owned()))
        };
        Ok(Self {
            width: number(env, "TICK_WIDTH", max_slots)?,
            ceiling: f64::from(ceiling),
            stop_short: f64::from(stop_short),
            stop_long: f64::from(stop_long),
            steps_short: steps("USAGE_STEPS_SHORT", "60 70 80")?,
            steps_long: steps("USAGE_STEPS_LONG", "80 85 88")?,
            max_age_s: number(env, "USAGE_MAX_AGE", 1800)?,
            refresh_after_s: number(env, "USAGE_REFRESH_AFTER", 120)?,
        })
    }
}

/// The ticks one window allows: the width under its first step, half the width under its second,
/// one under its third, and none from there or at the ceiling.
pub fn window_cap(used_pct: f64, steps: Steps, width: u32, ceiling: f64) -> u32 {
    let [half, one, none] = steps.0.map(f64::from);
    if used_pct >= ceiling || used_pct >= none {
        0
    } else if used_pct < half {
        width
    } else if used_pct < one {
        width.div_ceil(2)
    } else {
        width.min(1)
    }
}

/// The pool's cap: the agents' caps summed, at most the width.
pub fn pool_cap(width: u32, caps: impl IntoIterator<Item = u32>) -> u32 {
    caps.into_iter().fold(0_u32, u32::saturating_add).min(width)
}

/// How many more ticks may start: each agent's `(cap, running)` headroom summed, at most the width.
/// An agent past its cap, its windows having filled while its ticks ran, takes no other's room.
pub fn room(width: u32, agents: impl IntoIterator<Item = (u32, u32)>) -> u32 {
    pool_cap(
        width,
        agents
            .into_iter()
            .map(|(cap, running)| cap.saturating_sub(running)),
    )
}

/// One entry of the model list: an agent, the account it runs on when it names one, and the model.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Entry {
    pub agent: String,
    pub account: Option<String>,
    pub model: String,
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.key(), self.model)
    }
}

/// What a cap, a reading, a running count and a cooldown belong to: `agent`, or `agent@account`.
pub fn key_of(agent: &str, account: Option<&str>) -> String {
    match account {
        Some(account) => format!("{agent}@{account}"),
        None => agent.to_owned(),
    }
}

/// Splits `agent` or `agent@account`, each part checked.
pub fn split_key(text: &str) -> Result<(String, Option<String>), String> {
    let (agent, account) = match text.split_once('@') {
        Some((agent, account)) => (agent, Some(account)),
        None => (text, None),
    };
    if !valid_agent(agent) {
        return Err(format!(
            "'{text}': agent '{agent}' is not 1 to 32 of a-z, 0-9 and _"
        ));
    }
    if let Some(account) = account {
        if crate::accounts::variable(agent).is_none() {
            return Err(format!(
                "'{text}': only claude, codex and gemini have accounts"
            ));
        }
        if !crate::accounts::valid_name(account) {
            return Err(format!(
                "'{text}': account '{account}' is not 1 to 32 of a-z, 0-9 and -, not starting with -"
            ));
        }
    }
    Ok((agent.to_owned(), account.map(str::to_owned)))
}

/// An agent name, as it appears in setting names: 1 to 32 of a-z, 0-9 and _.
pub fn valid_agent(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

impl Entry {
    /// Parses `agent:model`, `agent@account:model`, or a bare `model` run by `default_agent`. The
    /// entry names its cooldown file, so the model holds no `/` and no control character.
    pub fn parse(text: &str, default_agent: &str) -> Result<Self, String> {
        let (key, model) = text.split_once(':').unwrap_or((default_agent, text));
        let (agent, account) = split_key(key)
            .map_err(|message| message.replacen(&format!("'{key}'"), &format!("'{text}'"), 1))?;
        if model.is_empty()
            || model.len() > 128
            || model.contains('/')
            || model.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(format!("'{text}': '{model}' is not a model name"));
        }
        Ok(Self {
            agent,
            account,
            model: model.to_owned(),
        })
    }

    pub fn key(&self) -> String {
        key_of(&self.agent, self.account.as_deref())
    }

    pub fn list(text: &str, default_agent: &str) -> Result<Vec<Self>, String> {
        text.split_whitespace()
            .map(|word| Self::parse(word, default_agent))
            .collect()
    }
}

/// The model list, `TICK_MODELS`, and the lane's entry, `LANE_MODEL`, with bare models run by
/// `TICK_AGENT`.
fn model_entries(env: Lookup) -> Result<(Vec<Entry>, Option<Entry>), String> {
    let agent = env("TICK_AGENT").unwrap_or_else(|| "claude".to_owned());
    if !valid_agent(&agent) {
        return Err(format!(
            "TICK_AGENT: '{agent}' is not 1 to 32 of a-z, 0-9 and _"
        ));
    }
    let models = env("TICK_MODELS").unwrap_or_else(|| "claude-opus-5-5".to_owned());
    let entries =
        Entry::list(&models, &agent).map_err(|message| format!("TICK_MODELS: {message}"))?;
    let lane = env("LANE_MODEL")
        .map(|text| Entry::parse(text.trim(), &agent))
        .transpose()
        .map_err(|message| format!("LANE_MODEL: {message}"))?;
    Ok((entries, lane))
}

/// Where one agent's or account's reading comes from, and what refreshes it.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentSource {
    pub agent: String,
    pub account: Option<Account>,
    pub source: Source,
    pub fetch: Option<PathBuf>,
}

impl AgentSource {
    /// `USAGE_SOURCE_<agent>` and `USAGE_FETCH_<agent>`. Claude reads its cache, `USAGE_CACHE`, and
    /// refreshes it with `USAGE_FETCH`; codex reads its session rollouts; any other agent must name
    /// its source, `none` included, and without one it allows nothing.
    pub fn from_env(env: Lookup, agent: &str) -> Result<Self, String> {
        let home = || env("HOME").map(PathBuf::from);
        let need_home = || home().ok_or_else(|| "HOME is not set".to_owned());
        let key = format!("USAGE_SOURCE_{agent}");
        let source = match (env(&key), agent) {
            (Some(text), _) => {
                let codex_home = || {
                    env("CODEX_HOME")
                        .map(PathBuf::from)
                        .or_else(|| home().map(|home| home.join(".codex")))
                        .map(|dir| dir.join("sessions"))
                };
                Source::parse(&text, codex_home).map_err(|message| format!("{key}: {message}"))?
            }
            (None, "claude") => Source::ClaudeCache(match env("USAGE_CACHE") {
                Some(path) => PathBuf::from(path),
                None => need_home()?.join(".claude/usage-cache.json"),
            }),
            (None, "codex") => Source::CodexSessions(match env("CODEX_HOME") {
                Some(dir) => PathBuf::from(dir).join("sessions"),
                None => need_home()?.join(".codex/sessions"),
            }),
            (None, _) => Source::Unset,
        };
        Ok(Self {
            agent: agent.to_owned(),
            account: None,
            source,
            fetch: agent_fetch(env, agent),
        })
    }

    /// An account's: `USAGE_SOURCE_<agent>_<account>` and `USAGE_FETCH_<agent>_<account>`, a `-` of
    /// the name read as `_`. Without its own source claude reads `<folder>/usage-cache.json` and codex
    /// `<folder>/sessions`, and any other agent takes `USAGE_SOURCE_<agent>`. A claude account runs
    /// only its own fetch command: claude's would read the default account's login and fill this
    /// account's cache with another account's usage. Any other agent's account falls back to the
    /// agent's command, run with the account's folder ([`AgentSource::fetch_env`]).
    pub fn for_account(env: Lookup, account: Account) -> Result<Self, String> {
        let agent = account.agent.clone();
        let own = format!("{agent}_{}", crate::accounts::key_name(&account.name));
        let sessions = account.dir.join("sessions");
        let parse = |key: &str, text: &str| {
            Source::parse(text, || Some(sessions.clone()))
                .map_err(|message| format!("{key}: {message}"))
        };
        let own_key = format!("USAGE_SOURCE_{own}");
        let agent_key = format!("USAGE_SOURCE_{agent}");
        let source = match (env(&own_key), agent.as_str()) {
            (Some(text), _) => parse(&own_key, &text)?,
            (None, "claude") => Source::ClaudeCache(account.dir.join("usage-cache.json")),
            (None, "codex") => Source::CodexSessions(sessions.clone()),
            (None, _) => match env(&agent_key) {
                Some(text) => parse(&agent_key, &text)?,
                None => Source::Unset,
            },
        };
        let fetch = match env(&format!("USAGE_FETCH_{own}")) {
            Some(path) => Some(PathBuf::from(path)),
            None if agent == "claude" => None,
            None => agent_fetch(env, &agent),
        };
        Ok(Self {
            agent,
            account: Some(account),
            source,
            fetch,
        })
    }

    /// A Claude cache of an account that has no fetch command of its own: nothing would refresh it
    /// but a command that reads another account's login, so it is read as no reading at all.
    pub fn missing_fetch(&self) -> Option<String> {
        let account = self.account.as_ref()?;
        (matches!(self.source, Source::ClaudeCache(_)) && self.fetch.is_none()).then(|| {
            format!(
                "no usage fetch for this account: set USAGE_FETCH_{}_{}",
                account.agent,
                crate::accounts::key_name(&account.name)
            )
        })
    }

    /// What the fetch command runs with for an account: the agent's variable set to its folder, and
    /// for a Claude cache `USAGE_CACHE` naming it and `USAGE_CREDENTIALS_FILE` the folder's
    /// `.credentials.json` when there is one. The default account's credentials file, which the
    /// environment may name, never reaches another account's fetch.
    pub fn fetch_env(&self) -> (Vec<(String, String)>, Vec<&'static str>) {
        let Some(account) = &self.account else {
            return (Vec::new(), Vec::new());
        };
        let mut set = vec![(
            account.variable().to_owned(),
            account.dir.display().to_string(),
        )];
        let mut remove = Vec::new();
        if let Source::ClaudeCache(path) = &self.source {
            set.push(("USAGE_CACHE".to_owned(), path.display().to_string()));
        }
        let credentials = account.dir.join(".credentials.json");
        if credentials.is_file() {
            set.push((
                "USAGE_CREDENTIALS_FILE".to_owned(),
                credentials.display().to_string(),
            ));
        } else {
            remove.push("USAGE_CREDENTIALS_FILE");
        }
        (set, remove)
    }
}

/// `USAGE_FETCH_<agent>`; for claude, else `USAGE_FETCH`, else the usual fetch script.
fn agent_fetch(env: Lookup, agent: &str) -> Option<PathBuf> {
    match (env(&format!("USAGE_FETCH_{agent}")), agent) {
        (Some(path), _) => Some(PathBuf::from(path)),
        (None, "claude") => env("USAGE_FETCH").map(PathBuf::from).or_else(|| {
            env("HOME").map(|home| PathBuf::from(home).join(".claude/scripts/usage-fetch.sh"))
        }),
        (None, _) => None,
    }
}

/// How fresh an agent's reading is.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "detail")]
pub enum Freshness {
    Fresh,
    /// Past its age, but its source records every use and every window says when it resets.
    Holding,
    Stale,
    Unreadable(String),
    /// The source is `none`.
    Unmetered,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WindowCap {
    pub name: String,
    pub used_pct: f64,
    pub window_minutes: u64,
    pub resets_at: Option<Value>,
    pub long: bool,
    pub steps: Steps,
    pub cap: u32,
}

/// One agent's or account's windows and the ticks they allow.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AgentUsage {
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The account's last sign-in check, when it has been checked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sign_in: Option<SignIn>,
    pub source: String,
    pub reading: Freshness,
    pub taken_ms: Option<u64>,
    pub windows: Vec<WindowCap>,
    pub cap: u32,
    /// Its live ticks, which [`pool`] counts; 0 elsewhere.
    pub running: u32,
}

impl AgentUsage {
    pub fn key(&self) -> String {
        key_of(&self.agent, self.account.as_deref())
    }

    fn signed_out(&self) -> bool {
        self.sign_in
            .as_ref()
            .is_some_and(|sign_in| sign_in.state == SignState::SignedOut)
    }
}

/// The steps of window `name`: `USAGE_STEPS_<agent>_<name>`, else by its length. The agent is part
/// of the name because two agents' windows may share one.
fn steps_of(
    env: Lookup,
    settings: &Settings,
    agent: &str,
    window: &Window,
) -> Result<Steps, String> {
    let key = format!("USAGE_STEPS_{agent}_{}", window.name);
    match env(&key) {
        Some(text) => Steps::parse(&key, &text),
        None if window.window_minutes >= LONG_WINDOW_MINUTES => Ok(settings.steps_long),
        None => Ok(settings.steps_short),
    }
}

/// Weighs `reading` (or why there is none) at `now_ms`. A window whose reset time has passed reads
/// as empty. A reading older than `USAGE_MAX_AGE` is stale and allows nothing, unless it holds until
/// its windows reset and every window says when that is: then it allows one tick. Use elsewhere on
/// the account may have moved it, so it never sizes the pool, but that tick's own run writes a fresh
/// reading, so the agent is never locked out.
pub fn assess(
    env: Lookup,
    settings: &Settings,
    source: &AgentSource,
    reading: Result<Reading, String>,
    now_ms: u64,
) -> Result<AgentUsage, String> {
    let mut usage = AgentUsage {
        agent: source.agent.clone(),
        account: source.account.as_ref().map(|account| account.name.clone()),
        sign_in: None,
        source: source.source.describe(),
        reading: Freshness::Unmetered,
        taken_ms: None,
        windows: Vec::new(),
        cap: settings.width,
        running: 0,
    };
    if source.source == Source::None {
        return Ok(usage);
    }
    usage.cap = 0;
    let reading = match reading {
        Ok(reading) => reading,
        Err(message) => {
            usage.reading = Freshness::Unreadable(message);
            return Ok(usage);
        }
    };
    usage.taken_ms = Some(reading.taken_ms);
    // A source that names no window says nothing about how full the agent is.
    if reading.windows.is_empty() {
        usage.reading = Freshness::Unreadable("the reading names no window".to_owned());
        return Ok(usage);
    }
    if reading.taken_ms > now_ms.saturating_add(FUTURE_SLACK_MS) {
        usage.reading = Freshness::Unreadable(format!(
            "dated {} s ahead of this host's clock",
            (reading.taken_ms - now_ms) / 1000
        ));
        return Ok(usage);
    }
    let age_s = now_ms.saturating_sub(reading.taken_ms) / 1000;
    let fresh = age_s <= settings.max_age_s;
    let holding = !fresh
        && reading.holds_until_reset
        && reading.windows.iter().all(|w| w.resets_at_s().is_some());
    let now_s = now_ms / 1000;
    for window in reading.windows {
        let steps = steps_of(env, settings, &source.agent, &window)?;
        let long = window.window_minutes >= LONG_WINDOW_MINUTES;
        let used_pct = if window.resets_at_s().is_some_and(|at| at <= now_s) {
            0.0
        } else {
            window.used_pct
        };
        usage.windows.push(WindowCap {
            cap: window_cap(used_pct, steps, settings.width, settings.ceiling),
            name: window.name,
            used_pct,
            window_minutes: window.window_minutes,
            resets_at: window.resets_at,
            long,
            steps,
        });
    }
    let cap = usage
        .windows
        .iter()
        .map(|w| w.cap)
        .min()
        .unwrap_or(settings.width);
    if fresh {
        usage.reading = Freshness::Fresh;
        usage.cap = cap;
    } else if holding {
        usage.reading = Freshness::Holding;
        usage.cap = cap.min(1);
    } else {
        usage.reading = Freshness::Stale;
    }
    Ok(usage)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Reads the agent's source, refreshing it first through its fetch command when the reading is
/// missing or older than `USAGE_REFRESH_AFTER`, so most calls start nothing.
fn read_refreshed(settings: &Settings, source: &AgentSource) -> Result<Reading, String> {
    let reading = sources::read(&source.source);
    let old = reading.as_ref().map_or(true, |reading| {
        now_ms().saturating_sub(reading.taken_ms) / 1000 >= settings.refresh_after_s
    });
    match &source.fetch {
        Some(fetch) if old && executable(fetch) => {
            run_fetch(fetch, source);
            sources::read(&source.source)
        }
        _ => reading,
    }
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Runs the fetch command with no input or output, killing it past [`FETCH_TIMEOUT`]. Its failure
/// shows as the reading it failed to refresh.
fn run_fetch(fetch: &Path, source: &AgentSource) {
    let (set, remove) = source.fetch_env();
    let mut command = Command::new(fetch);
    for name in remove {
        command.env_remove(name);
    }
    let Ok(mut child) = command
        .envs(set)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let start = Instant::now();
    while matches!(child.try_wait(), Ok(None)) {
        if start.elapsed() > FETCH_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One agent's or account's windows now, refreshed first through its fetch command when `refresh`
/// is set. An account whose folder is refused allows nothing, and neither does one its last check
/// found signed out.
pub fn agent_usage(
    env: Lookup,
    settings: &Settings,
    agent: &str,
    account: Option<&str>,
    refresh: bool,
) -> Result<AgentUsage, String> {
    let source = match account {
        None => AgentSource::from_env(env, agent)?,
        Some(name) => match Account::resolve(env, agent, name) {
            Ok(account) => AgentSource::for_account(env, account)?,
            Err(message) => {
                let source = AgentSource {
                    agent: agent.to_owned(),
                    account: None,
                    source: Source::Unset,
                    fetch: None,
                };
                let mut usage = assess(env, settings, &source, Err(message), now_ms())?;
                usage.account = Some(name.to_owned());
                return Ok(usage);
            }
        },
    };
    let reading = if let Some(why) = source.missing_fetch() {
        Err(why)
    } else {
        match (&source.source, account) {
            (Source::None, _) => Err(String::new()),
            (Source::Unset, Some(name)) => Err(format!(
                "neither USAGE_SOURCE_{agent}_{} nor USAGE_SOURCE_{agent} is set: name its source, or none",
                crate::accounts::key_name(name)
            )),
            (Source::Unset, None) => Err(format!(
                "USAGE_SOURCE_{agent} is not set: name its source, or none"
            )),
            _ if refresh => read_refreshed(settings, &source),
            _ => sources::read(&source.source),
        }
    };
    let mut usage = assess(env, settings, &source, reading, now_ms())?;
    if let (Some(account), Some(state)) = (&source.account, state_dir(env)) {
        usage.sign_in = crate::accounts::cached(&state, &account.key());
        if usage.signed_out() {
            usage.cap = 0;
        }
    }
    Ok(usage)
}

/// The distinct agents and accounts of the entries, in order of first appearance.
fn seats_of<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Vec<(String, Option<String>)> {
    let mut seats: Vec<(String, Option<String>)> = Vec::new();
    for entry in entries {
        let seat = (entry.agent.clone(), entry.account.clone());
        if !seats.contains(&seat) {
            seats.push(seat);
        }
    }
    seats
}

/// Every agent and account of the model list and the lane, with its windows, its live ticks and the
/// ticks its caps allow.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Pool {
    pub width: u32,
    /// The agents' caps summed, at most the width: how many ticks may run at once.
    pub pool_cap: u32,
    /// How many more may start now: each agent's cap less its live ticks, summed, at most the
    /// width. Another agent's ticks past its own cap take none of it.
    pub room: u32,
    pub agents: Vec<AgentUsage>,
    /// The agents and accounts, `agent` or `agent@account`, each of whose entries cools down on its
    /// usage limit.
    pub cooling: Vec<String>,
}

/// The pool as the settings in `env` read now, without refreshing any reading. An agent each of
/// whose entries cools down adds nothing to the pool: no runner could pick it.
pub fn pool(env: Lookup) -> Result<Pool, String> {
    let settings = Settings::from_env(env)?;
    let (entries, lane) = model_entries(env)?;
    let entries: Vec<Entry> = entries.into_iter().chain(lane).collect();
    let cooldown = Duration::from_secs(number(env, "MODEL_COOLDOWN_S", 1800)?);
    let state = state_dir(env);
    let running = match &state {
        Some(state) => running_by_key(state)?,
        None => HashMap::new(),
    };
    let agents = seats_of(&entries)
        .iter()
        .map(|(agent, account)| {
            let mut usage = agent_usage(env, &settings, agent, account.as_deref(), false)?;
            usage.running = running.get(&usage.key()).copied().unwrap_or(0);
            Ok(usage)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let cooling: Vec<String> = agents
        .iter()
        .map(AgentUsage::key)
        .filter(|key| {
            state.as_ref().is_some_and(|state| {
                entries
                    .iter()
                    .filter(|entry| &entry.key() == key)
                    .all(|entry| cooling(state, entry, cooldown))
            })
        })
        .collect();
    let can_run = || {
        agents
            .iter()
            .filter(|usage| !cooling.contains(&usage.key()))
    };
    Ok(Pool {
        width: settings.width,
        pool_cap: pool_cap(settings.width, can_run().map(|usage| usage.cap)),
        room: room(
            settings.width,
            can_run().map(|usage| (usage.cap, usage.running)),
        ),
        agents,
        cooling,
    })
}

/// How many live ticks of each agent and account the state directory records.
fn running_by_key(state: &Path) -> Result<HashMap<String, u32>, String> {
    let mut running: HashMap<String, u32> = HashMap::new();
    for tick in crate::ticks::scan(&state.to_string_lossy())
        .map_err(|error| format!("cannot count the running ticks: {error}"))?
    {
        *running.entry(tick.key()).or_default() += 1;
    }
    Ok(running)
}

/// `HARNESS_STATE`, else the checkout's `state/`.
pub fn state_dir(env: Lookup) -> Option<PathBuf> {
    env("HARNESS_STATE")
        .map(PathBuf::from)
        .or_else(|| harness_home().map(|home| home.join("state")))
}

/// Also checks again each account of the lists, and the supervisor's with `SUPERVISOR=1`, whose
/// sign-in check is old, printing a line for each signed out, or signed in again, since the launcher
/// last logged it: the launcher logs them.
fn refresh(env: Lookup) -> Result<ExitCode, String> {
    let settings = Settings::from_env(env)?;
    let (entries, lane) = model_entries(env)?;
    let mut accounts = Vec::new();
    if env("SUPERVISOR").as_deref() == Some("1")
        && let Some(text) = env("SUPERVISOR_MODEL")
    {
        let agent = env("TICK_AGENT").unwrap_or_else(|| "claude".to_owned());
        let entry = Entry::parse(text.trim(), &agent)
            .map_err(|message| format!("SUPERVISOR_MODEL: {message}"))?;
        if let Some(name) = &entry.account
            && let Ok(account) = Account::resolve(env, &entry.agent, name)
        {
            accounts.push(account);
        }
    }
    for (agent, account) in seats_of(entries.iter().chain(lane.iter())) {
        let source = match account {
            None => AgentSource::from_env(env, &agent)?,
            Some(name) => match Account::resolve(env, &agent, &name) {
                Ok(account) => {
                    accounts.push(account.clone());
                    AgentSource::for_account(env, account)?
                }
                Err(_) => continue,
            },
        };
        if let (Some(account), Some(state)) = (&source.account, state_dir(env)) {
            let missing = source.missing_fetch();
            if crate::accounts::first_time(&state, &account.key(), "no-fetch", missing.is_some()) {
                println!(
                    "{}: {}; none of its ticks start",
                    account.key(),
                    missing.unwrap_or_default()
                );
            }
        }
        if !matches!(source.source, Source::None | Source::Unset) {
            let _ = read_refreshed(&settings, &source);
        }
    }
    if let Some(state) = state_dir(env) {
        for line in crate::accounts::refresh(env, &state, &accounts, now_ms() / 1000, true) {
            println!("{line}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn report(env: Lookup, json: bool) -> Result<ExitCode, String> {
    let report = pool(env)?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|error| error.to_string())?
        );
        return Ok(ExitCode::SUCCESS);
    }
    for agent in &report.agents {
        let state = match &agent.reading {
            Freshness::Fresh => String::new(),
            Freshness::Holding => ": the reading is old, so one tick refreshes it".to_owned(),
            Freshness::Stale => ": the reading is stale".to_owned(),
            Freshness::Unreadable(message) => format!(": no reading ({message})"),
            Freshness::Unmetered => ": no windows".to_owned(),
        };
        let cooling = if report.cooling.contains(&agent.key()) {
            ", every entry cooling down"
        } else {
            ""
        };
        let signed_out = if agent.signed_out() {
            ", signed out"
        } else {
            ""
        };
        println!(
            "{}: cap {} of {}, {} running{cooling}{signed_out}, {}{state}",
            agent.key(),
            agent.cap,
            report.width,
            agent.running,
            agent.source
        );
        for window in &agent.windows {
            let [half, one, none] = window.steps.0;
            println!(
                "  {} {}% of {} min, steps {half} {one} {none}: {}",
                window.name, window.used_pct, window.window_minutes, window.cap
            );
        }
    }
    println!(
        "pool: {} of {}, room for {} more",
        report.pool_cap, report.width, report.room
    );
    Ok(ExitCode::SUCCESS)
}

/// What `check` decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Go,
    Stop,
    Unknown,
}

/// Whether a running tick of this agent may start more work: every window under its stand-down
/// point, `USAGE_TICK_STOP_PCT` for a short one and `USAGE_TICK_STOP_LONG_PCT` for a long one.
pub fn verdict(settings: &Settings, usage: &AgentUsage) -> Verdict {
    match usage.reading {
        Freshness::Unmetered => Verdict::Go,
        Freshness::Stale | Freshness::Unreadable(_) => Verdict::Unknown,
        Freshness::Fresh | Freshness::Holding => {
            let over = usage.windows.iter().any(|w| {
                let stop = if w.long {
                    settings.stop_long
                } else {
                    settings.stop_short
                };
                w.used_pct >= stop || w.used_pct >= settings.ceiling
            });
            if over { Verdict::Stop } else { Verdict::Go }
        }
    }
}

fn check(env: Lookup, key: &str) -> Result<ExitCode, String> {
    if !key.contains('@') && !valid_agent(key) {
        return Err(format!("--agent: '{key}' is not 1 to 32 of a-z, 0-9 and _"));
    }
    let (agent, account) = split_key(key).map_err(|message| format!("--agent: {message}"))?;
    let settings = Settings::from_env(env)?;
    let usage = agent_usage(env, &settings, &agent, account.as_deref(), true)?;
    let windows: Vec<String> = usage
        .windows
        .iter()
        .map(|w| format!("{}={}%", w.name, w.used_pct))
        .collect();
    let stops = format!(
        "stop={}% long_stop={}% ceiling={}%",
        settings.stop_short, settings.stop_long, settings.ceiling
    );
    let verdict = verdict(&settings, &usage);
    let head = if windows.is_empty() {
        format!("{key}: no windows")
    } else {
        format!("{key}: {}", windows.join(" "))
    };
    match verdict {
        Verdict::Go => println!("{head} {stops}: go on"),
        Verdict::Stop => println!("{head} {stops}: start nothing, pause and end"),
        Verdict::Unknown => {
            let why = match &usage.reading {
                Freshness::Unreadable(message) => message.clone(),
                _ => "the reading is stale".to_owned(),
            };
            println!(
                "usage unknown: no fresh reading for {key} from {} ({why}); start nothing",
                usage.source
            );
        }
    }
    Ok(ExitCode::from(match verdict {
        Verdict::Go => 0,
        Verdict::Stop => 1,
        Verdict::Unknown => 2,
    }))
}

/// One entry weighed for the next tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub entry: Entry,
    pub cooling: bool,
    pub cap: u32,
    pub running: u32,
}

/// The first candidate not tried and not cooling down whose agent (or account) may run one more
/// tick: under its cap, or, for a landing tick, which may start past the width, with a cap above 0.
pub fn pick<'a>(candidates: &'a [Candidate], tried: &[Entry], landing: bool) -> Option<&'a Entry> {
    candidates
        .iter()
        .find(|c| {
            !c.cooling
                && !tried.contains(&c.entry)
                && if landing {
                    c.cap > 0
                } else {
                    c.running < c.cap
                }
        })
        .map(|c| &c.entry)
}

/// Whether `entry` hit its usage limit less than `MODEL_COOLDOWN_S` ago: the runner touches
/// `<state>/model-cooldown/<entry>` when it does, `<agent>:<model>` or `<agent>@<account>:<model>`.
pub fn cooling(state: &Path, entry: &Entry, cooldown: Duration) -> bool {
    std::fs::metadata(state.join("model-cooldown").join(entry.to_string()))
        .and_then(|meta| meta.modified())
        .is_ok_and(|at| at.elapsed().unwrap_or_default() < cooldown)
}

struct PickArgs<'a> {
    agent: Option<&'a str>,
    tried: &'a str,
    landing: bool,
    entry: Option<&'a str>,
}

fn pick_command(env: Lookup, args: &PickArgs) -> Result<ExitCode, String> {
    let settings = Settings::from_env(env)?;
    let (entries, _) = model_entries(env)?;
    let default_agent = env("TICK_AGENT").unwrap_or_else(|| "claude".to_owned());
    let mut entries = match args.entry {
        Some(text) => vec![Entry::parse(text, &default_agent)?],
        None => entries,
    };
    if let Some(key) = args.agent {
        entries.retain(|entry| entry.key() == key);
    }
    let tried =
        Entry::list(args.tried, &default_agent).map_err(|message| format!("--tried: {message}"))?;
    let state = state_dir(env).ok_or("no state directory: set HARNESS_STATE")?;
    let cooldown = Duration::from_secs(number(env, "MODEL_COOLDOWN_S", 1800)?);
    let running = running_by_key(&state)?;
    let mut caps = BTreeMap::new();
    for (agent, account) in seats_of(&entries) {
        caps.insert(
            key_of(&agent, account.as_deref()),
            agent_usage(env, &settings, &agent, account.as_deref(), false)?.cap,
        );
    }
    let candidates: Vec<Candidate> = entries
        .into_iter()
        .map(|entry| Candidate {
            cooling: cooling(&state, &entry, cooldown),
            cap: caps.get(&entry.key()).copied().unwrap_or(0),
            running: running.get(&entry.key()).copied().unwrap_or(0),
            entry,
        })
        .collect();
    Ok(match pick(&candidates, &tried, args.landing) {
        Some(entry) => {
            println!("{entry}");
            ExitCode::SUCCESS
        }
        None => ExitCode::from(1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn settings(pairs: &[(&str, &str)]) -> Settings {
        Settings::from_env(&env_of(pairs)).unwrap()
    }

    fn reading(windows: &[(&str, f64, u64)], taken_ms: u64) -> Reading {
        Reading {
            taken_ms,
            windows: windows
                .iter()
                .map(|(name, used_pct, window_minutes)| Window {
                    name: (*name).to_owned(),
                    used_pct: *used_pct,
                    window_minutes: *window_minutes,
                    resets_at: None,
                })
                .collect(),
            holds_until_reset: false,
        }
    }

    const NOW_MS: u64 = 1_800_000_000_000;

    fn json_source(agent: &str) -> AgentSource {
        AgentSource {
            agent: agent.to_owned(),
            account: None,
            source: Source::Json("/u.json".into()),
            fetch: None,
        }
    }

    fn cap_of(env: &[(&str, &str)], windows: &[(&str, f64, u64)]) -> u32 {
        let lookup = env_of(env);
        let s = Settings::from_env(&lookup).unwrap();
        assess(
            &lookup,
            &s,
            &json_source("claude"),
            Ok(reading(windows, NOW_MS)),
            NOW_MS,
        )
        .unwrap()
        .cap
    }

    #[test]
    fn the_defaults_match_the_documented_ones() {
        let s = settings(&[]);
        assert_eq!(s.width, 8);
        assert!((s.ceiling - 90.0).abs() < f64::EPSILON);
        assert!((s.stop_short - 88.0).abs() < f64::EPSILON);
        assert!((s.stop_long - 88.0).abs() < f64::EPSILON);
        assert_eq!(s.steps_short, Steps([60, 70, 80]));
        assert_eq!(s.steps_long, Steps([80, 85, 88]));
        assert_eq!(settings(&[("MAX_SLOTS", "4")]).width, 4);
        assert_eq!(
            settings(&[("MAX_SLOTS", "4"), ("TICK_WIDTH", "3")]).width,
            3
        );
        let own = settings(&[("USAGE_TICK_STOP_PCT", "80")]);
        assert!((own.stop_long - 80.0).abs() < f64::EPSILON);
    }

    #[test]
    fn bad_settings_are_refused_by_name() {
        for (key, value) in [
            ("USAGE_STEPS_SHORT", "60 70"),
            ("USAGE_STEPS_SHORT", "70 60 80"),
            ("USAGE_STEPS_LONG", "80 85 101"),
            ("USAGE_STEPS_LONG", "a b c"),
            ("MAX_SLOTS", "-1"),
            ("USAGE_CEILING_PCT", "300"),
            ("USAGE_MAX_AGE", "soon"),
        ] {
            let error = Settings::from_env(&env_of(&[(key, value)])).unwrap_err();
            assert!(error.starts_with(key), "{error}");
        }
    }

    #[test]
    fn a_short_window_steps_down_by_the_short_steps() {
        let five = |pct| cap_of(&[], &[("five_hour", pct, 300)]);
        assert_eq!(five(10.0), 8);
        assert_eq!(five(59.9), 8);
        assert_eq!(five(60.0), 4);
        assert_eq!(five(70.0), 1);
        assert_eq!(five(79.0), 1);
        assert_eq!(five(80.0), 0);
    }

    #[test]
    fn a_long_window_steps_down_by_the_long_steps() {
        let week = |pct| cap_of(&[], &[("weekly", pct, 10_080)]);
        assert_eq!(week(79.0), 8);
        assert_eq!(week(80.0), 4);
        assert_eq!(week(85.0), 1);
        assert_eq!(week(88.0), 0);
        // A day is long.
        assert_eq!(cap_of(&[], &[("daily", 80.0, 1440)]), 4);
        assert_eq!(cap_of(&[], &[("daily", 80.0, 1439)]), 0);
    }

    #[test]
    fn steps_follow_the_settings_and_a_window_override() {
        let env = [
            ("USAGE_STEPS_SHORT", "10 20 30"),
            ("USAGE_STEPS_LONG", "50 60 70"),
        ];
        assert_eq!(cap_of(&env, &[("five_hour", 15.0, 300)]), 4);
        assert_eq!(cap_of(&env, &[("weekly", 65.0, 10_080)]), 1);
        let own = [("USAGE_STEPS_claude_weekly", "64 70 75")];
        assert_eq!(cap_of(&own, &[("weekly", 66.0, 10_080)]), 4);
        assert_eq!(cap_of(&own, &[("weekly", 71.0, 10_080)]), 1);
        assert_eq!(cap_of(&own, &[("weekly", 75.0, 10_080)]), 0);
        // Another agent's override, or another window's, does not apply.
        let other = [
            ("USAGE_STEPS_codex_weekly", "1 2 3"),
            ("USAGE_STEPS_claude_five_hour", "1 2 3"),
        ];
        assert_eq!(cap_of(&other, &[("weekly", 66.0, 10_080)]), 8);
        let bad = env_of(&[("USAGE_STEPS_claude_weekly", "1 2")]);
        let error = assess(
            &bad,
            &settings(&[]),
            &json_source("claude"),
            Ok(reading(&[("weekly", 1.0, 10_080)], NOW_MS)),
            NOW_MS,
        )
        .unwrap_err();
        assert!(error.starts_with("USAGE_STEPS_claude_weekly"), "{error}");
    }

    #[test]
    fn the_tightest_window_and_the_ceiling_decide() {
        assert_eq!(
            cap_of(&[], &[("five_hour", 65.0, 300), ("weekly", 86.0, 10_080)]),
            1
        );
        assert_eq!(
            cap_of(&[], &[("a", 1.0, 60), ("b", 1.0, 60), ("c", 75.0, 60)]),
            1
        );
        // Steps set past the ceiling still stop at it.
        let past = [("USAGE_STEPS_SHORT", "95 96 97")];
        assert_eq!(cap_of(&past, &[("five_hour", 92.0, 300)]), 0);
        assert_eq!(cap_of(&[], &[]), 0, "a reading of no window is no reading");
    }

    #[test]
    fn the_width_halves_up_and_one_never_passes_it() {
        assert_eq!(window_cap(65.0, Steps([60, 70, 80]), 3, 90.0), 2);
        assert_eq!(window_cap(75.0, Steps([60, 70, 80]), 0, 90.0), 0);
        assert_eq!(window_cap(65.0, Steps([60, 70, 80]), 1, 90.0), 1);
    }

    #[test]
    fn no_fresh_reading_allows_nothing() {
        let s = settings(&[]);
        let env = env_of(&[]);
        let stale = NOW_MS - 1_801_000;
        let usage = assess(
            &env,
            &s,
            &json_source("claude"),
            Ok(reading(&[("a", 1.0, 60)], stale)),
            NOW_MS,
        )
        .unwrap();
        assert_eq!((usage.cap, &usage.reading), (0, &Freshness::Stale));
        let usage = assess(
            &env,
            &s,
            &json_source("claude"),
            Err("gone".to_owned()),
            NOW_MS,
        )
        .unwrap();
        assert_eq!(usage.cap, 0);
        assert_eq!(usage.reading, Freshness::Unreadable("gone".to_owned()));
        assert_eq!(verdict(&s, &usage), Verdict::Unknown);
        let edge = NOW_MS - 1_800_000;
        let usage = assess(
            &env,
            &s,
            &json_source("claude"),
            Ok(reading(&[("a", 1.0, 60)], edge)),
            NOW_MS,
        )
        .unwrap();
        assert_eq!(usage.cap, 8);
    }

    #[test]
    fn a_reading_dated_ahead_or_of_no_window_allows_nothing() {
        let s = settings(&[]);
        let env = env_of(&[]);
        let ahead = assess(
            &env,
            &s,
            &json_source("claude"),
            Ok(reading(&[("a", 1.0, 60)], NOW_MS + 301_000)),
            NOW_MS,
        )
        .unwrap();
        assert_eq!(ahead.cap, 0);
        assert!(matches!(ahead.reading, Freshness::Unreadable(_)));
        let skewed = assess(
            &env,
            &s,
            &json_source("claude"),
            Ok(reading(&[("a", 1.0, 60)], NOW_MS + 299_000)),
            NOW_MS,
        )
        .unwrap();
        assert_eq!((skewed.cap, &skewed.reading), (8, &Freshness::Fresh));
        let empty = assess(
            &env,
            &s,
            &json_source("claude"),
            Ok(reading(&[], NOW_MS)),
            NOW_MS,
        )
        .unwrap();
        assert_eq!(empty.cap, 0);
        assert_eq!(verdict(&s, &empty), Verdict::Unknown);
    }

    #[test]
    fn an_agent_without_a_source_allows_nothing() {
        let s = settings(&[]);
        let usage = agent_usage(&env_of(&[]), &s, "gemini", None, false).unwrap();
        assert_eq!(usage.cap, 0);
        assert_eq!(
            usage.reading,
            Freshness::Unreadable(
                "USAGE_SOURCE_gemini is not set: name its source, or none".to_owned()
            )
        );
        let none = agent_usage(
            &env_of(&[("USAGE_SOURCE_gemini", "none")]),
            &s,
            "gemini",
            None,
            false,
        )
        .unwrap();
        assert_eq!(none.cap, 8);
    }

    #[test]
    fn an_unmetered_agent_has_the_full_width() {
        let s = settings(&[("MAX_SLOTS", "3")]);
        let source = AgentSource {
            agent: "gemini".to_owned(),
            account: None,
            source: Source::None,
            fetch: None,
        };
        let usage = assess(&env_of(&[]), &s, &source, Err(String::new()), NOW_MS).unwrap();
        assert_eq!((usage.cap, &usage.reading), (3, &Freshness::Unmetered));
        assert_eq!(verdict(&s, &usage), Verdict::Go);
    }

    #[test]
    fn a_rollout_reading_holds_until_its_windows_reset() {
        let s = settings(&[]);
        let env = env_of(&[]);
        let source = AgentSource {
            agent: "codex".to_owned(),
            account: None,
            source: Source::CodexSessions("/s".into()),
            fetch: None,
        };
        let now_s = NOW_MS / 1000;
        let day_old = NOW_MS - 86_400_000;
        let rollout = |resets: &[Option<u64>]| Reading {
            taken_ms: day_old,
            windows: resets
                .iter()
                .enumerate()
                .map(|(i, at)| Window {
                    name: format!("w{i}"),
                    used_pct: 82.0,
                    window_minutes: 10_080,
                    resets_at: at.map(Value::from),
                })
                .collect(),
            holds_until_reset: true,
        };
        let held = assess(&env, &s, &source, Ok(rollout(&[Some(now_s + 60)])), NOW_MS).unwrap();
        assert_eq!((held.cap, &held.reading), (1, &Freshness::Holding));
        let reset = assess(&env, &s, &source, Ok(rollout(&[Some(now_s - 60)])), NOW_MS).unwrap();
        assert_eq!((reset.cap, &reset.reading), (1, &Freshness::Holding));
        assert!(reset.windows[0].used_pct.abs() < f64::EPSILON);
        let unknown = assess(
            &env,
            &s,
            &source,
            Ok(rollout(&[Some(now_s + 60), None])),
            NOW_MS,
        )
        .unwrap();
        assert_eq!((unknown.cap, &unknown.reading), (0, &Freshness::Stale));
    }

    #[test]
    fn the_stand_down_points_split_short_and_long_windows() {
        let lookup = env_of(&[
            ("USAGE_CEILING_PCT", "95"),
            ("USAGE_TICK_STOP_PCT", "90"),
            ("USAGE_TICK_STOP_LONG_PCT", "93"),
        ]);
        let s = Settings::from_env(&lookup).unwrap();
        let verdict_of = |five: f64, week: f64| {
            let usage = assess(
                &lookup,
                &s,
                &json_source("claude"),
                Ok(reading(
                    &[("five_hour", five, 300), ("weekly", week, 10_080)],
                    NOW_MS,
                )),
                NOW_MS,
            )
            .unwrap();
            verdict(&s, &usage)
        };
        assert_eq!(verdict_of(89.0, 92.0), Verdict::Go);
        assert_eq!(verdict_of(90.0, 10.0), Verdict::Stop);
        assert_eq!(verdict_of(10.0, 93.0), Verdict::Stop);
        let d = settings(&[]);
        let usage = assess(
            &env_of(&[]),
            &d,
            &json_source("claude"),
            Ok(reading(&[("five_hour", 87.0, 300)], NOW_MS)),
            NOW_MS,
        )
        .unwrap();
        assert_eq!(verdict(&d, &usage), Verdict::Go);
        let usage = assess(
            &env_of(&[]),
            &d,
            &json_source("claude"),
            Ok(reading(&[("five_hour", 88.0, 300)], NOW_MS)),
            NOW_MS,
        )
        .unwrap();
        assert_eq!(verdict(&d, &usage), Verdict::Stop);
    }

    #[test]
    fn the_pool_is_the_caps_summed_up_to_the_width() {
        assert_eq!(pool_cap(8, [1, 0, 2]), 3);
        assert_eq!(pool_cap(4, [4, 4]), 4);
        assert_eq!(pool_cap(4, []), 0);
        assert_eq!(pool_cap(4, [u32::MAX, 1]), 4);
    }

    #[test]
    fn one_agent_past_its_cap_leaves_another_its_room() {
        // claude at cap 0 with four ticks still running; codex at cap 4 with none.
        assert_eq!(room(8, [(0, 4), (4, 0)]), 4);
        assert_eq!(room(8, [(2, 1), (4, 3)]), 2);
        assert_eq!(room(3, [(4, 0), (4, 0)]), 3);
        assert_eq!(room(8, []), 0);
    }

    #[test]
    fn entries_name_an_agent_or_take_the_default() {
        assert_eq!(
            Entry::list("claude:opus codex:gpt-5.5 gemini-3-pro", "claude").unwrap(),
            [
                Entry {
                    agent: "claude".into(),
                    account: None,
                    model: "opus".into()
                },
                Entry {
                    agent: "codex".into(),
                    account: None,
                    model: "gpt-5.5".into()
                },
                Entry {
                    agent: "claude".into(),
                    account: None,
                    model: "gemini-3-pro".into()
                },
            ]
        );
        assert_eq!(
            Entry::parse("ollama:llama3:8b", "claude").unwrap().model,
            "llama3:8b"
        );
        assert_eq!(
            Entry::parse("claude-opus-4-6[1m]", "claude")
                .unwrap()
                .to_string(),
            "claude:claude-opus-4-6[1m]"
        );
        for bad in [
            "Claude:opus",
            "codex:",
            ":m",
            "a-b:m",
            "codex:../x",
            "codex:a/b",
        ] {
            assert!(Entry::parse(bad, "claude").is_err(), "{bad}");
        }
    }

    #[test]
    fn sources_default_per_agent() {
        let env = env_of(&[("HOME", "/home/dev-a")]);
        let of = |agent| AgentSource::from_env(&env, agent).unwrap();
        assert_eq!(
            of("claude").source,
            Source::ClaudeCache("/home/dev-a/.claude/usage-cache.json".into())
        );
        assert_eq!(
            of("claude").fetch,
            Some("/home/dev-a/.claude/scripts/usage-fetch.sh".into())
        );
        assert_eq!(
            of("codex").source,
            Source::CodexSessions("/home/dev-a/.codex/sessions".into())
        );
        assert_eq!(of("codex").fetch, None);
        assert_eq!(of("gemini").source, Source::Unset);
        let set = env_of(&[
            ("HOME", "/home/dev-a"),
            ("USAGE_CACHE", "/c.json"),
            ("USAGE_FETCH", "/f.sh"),
            ("CODEX_HOME", "/ch"),
            ("USAGE_SOURCE_gemini", "json:/g.json"),
            ("USAGE_FETCH_gemini", "/g.sh"),
        ]);
        let of = |agent| AgentSource::from_env(&set, agent).unwrap();
        assert_eq!(of("claude").source, Source::ClaudeCache("/c.json".into()));
        assert_eq!(of("claude").fetch, Some("/f.sh".into()));
        assert_eq!(
            of("codex").source,
            Source::CodexSessions("/ch/sessions".into())
        );
        assert_eq!(of("gemini").source, Source::Json("/g.json".into()));
        assert_eq!(of("gemini").fetch, Some("/g.sh".into()));
        let bad = env_of(&[("USAGE_SOURCE_codex", "s3://x")]);
        assert!(
            AgentSource::from_env(&bad, "codex")
                .unwrap_err()
                .starts_with("USAGE_SOURCE_codex")
        );
    }

    fn candidate(entry: &str, cooling: bool, cap: u32, running: u32) -> Candidate {
        Candidate {
            entry: Entry::parse(entry, "claude").unwrap(),
            cooling,
            cap,
            running,
        }
    }

    #[test]
    fn pick_takes_the_best_entry_whose_agent_has_room() {
        let list = [
            candidate("claude:opus", false, 2, 2),
            candidate("codex:gpt-5.5", true, 4, 0),
            candidate("codex:gpt-5.4", false, 4, 1),
            candidate("gemini:gemini-3-pro", false, 4, 0),
        ];
        let name = |found: Option<&Entry>| found.map(ToString::to_string);
        assert_eq!(
            name(pick(&list, &[], false)),
            Some("codex:gpt-5.4".to_owned())
        );
        let tried = [Entry::parse("codex:gpt-5.4", "claude").unwrap()];
        assert_eq!(
            name(pick(&list, &tried, false)),
            Some("gemini:gemini-3-pro".to_owned())
        );
        // A landing tick may start past its agent's running count, never past a cap of 0.
        assert_eq!(name(pick(&list, &[], true)), Some("claude:opus".to_owned()));
        let full = [
            candidate("claude:opus", false, 0, 0),
            candidate("codex:gpt-5.5", false, 1, 1),
        ];
        assert_eq!(
            name(pick(&full, &[], true)),
            Some("codex:gpt-5.5".to_owned())
        );
        assert_eq!(pick(&full, &[], false), None);
        assert_eq!(pick(&[], &[], false), None);
    }

    #[test]
    fn an_agent_whose_every_entry_cools_adds_nothing_to_the_pool() {
        let dir = std::env::temp_dir().join(format!("usage-pool-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("state/model-cooldown")).unwrap();
        let reading = format!(
            r#"{{"timestamp": {}, "windows": [{{"name": "h", "used_pct": 75, "window_minutes": 60}}]}}"#,
            now_ms()
        );
        std::fs::write(dir.join("u.json"), reading).unwrap();
        let source = format!("json:{}", dir.join("u.json").display());
        let state = dir.join("state").display().to_string();
        let env = env_of(&[
            ("TICK_MODELS", "claude:a codex:b codex:c"),
            ("USAGE_SOURCE_claude", &source),
            ("USAGE_SOURCE_codex", &source),
            ("HARNESS_STATE", &state),
            ("MAX_SLOTS", "4"),
        ]);
        let both = pool(&env).unwrap();
        std::fs::write(dir.join("state/model-cooldown/codex:b"), "").unwrap();
        let one_cooling = pool(&env).unwrap();
        std::fs::write(dir.join("state/model-cooldown/codex:c"), "").unwrap();
        let all_cooling = pool(&env).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!((both.pool_cap, both.cooling.len()), (2, 0));
        assert_eq!(one_cooling.pool_cap, 2, "codex:c can still run");
        assert_eq!(
            (all_cooling.pool_cap, all_cooling.cooling),
            (1, vec!["codex".to_owned()])
        );
    }

    #[test]
    fn the_pool_counts_each_agent_once_and_the_lane_too() {
        let env = env_of(&[
            ("TICK_MODELS", "claude:a codex:b claude:c"),
            ("LANE_MODEL", "gemini:d"),
        ]);
        let (entries, lane) = model_entries(&env).unwrap();
        assert_eq!(
            seats_of(entries.iter().chain(lane.iter())),
            [
                ("claude".to_owned(), None),
                ("codex".to_owned(), None),
                ("gemini".to_owned(), None)
            ]
        );
        let bare = env_of(&[("TICK_AGENT", "codex"), ("TICK_MODELS", "gpt-5.5")]);
        assert_eq!(
            model_entries(&bare).unwrap().0[0].to_string(),
            "codex:gpt-5.5"
        );
    }

    #[test]
    fn an_entry_may_name_an_account() {
        let entry = Entry::parse("claude@work:opus", "claude").unwrap();
        assert_eq!(
            (
                entry.agent.as_str(),
                entry.account.as_deref(),
                entry.model.as_str()
            ),
            ("claude", Some("work"), "opus")
        );
        assert_eq!(entry.key(), "claude@work");
        assert_eq!(entry.to_string(), "claude@work:opus");
        assert_eq!(Entry::parse("codex:gpt", "claude").unwrap().account, None);
        assert_eq!(
            Entry::parse("codex@client-2:gpt", "claude").unwrap().key(),
            "codex@client-2"
        );
        for bad in [
            "claude@:opus",
            "claude@Work:opus",
            "claude@-a:opus",
            "claude@a_b:opus",
            "claude@a@b:opus",
            "aider@work:m",
            "@work:opus",
        ] {
            let error = Entry::parse(bad, "claude").unwrap_err();
            assert!(error.starts_with(&format!("'{bad}'")), "{bad}: {error}");
        }
        assert_eq!(
            split_key("gemini@lab").unwrap(),
            ("gemini".to_owned(), Some("lab".to_owned()))
        );
        assert!(split_key("gemini@").is_err());
    }

    fn account_dir(root: &Path, agent: &str, name: &str) -> Account {
        std::fs::create_dir_all(root.join(agent).join(format!("{agent}-{name}"))).unwrap();
        Account {
            agent: agent.to_owned(),
            name: name.to_owned(),
            dir: root.join(agent).join(format!("{agent}-{name}")),
        }
    }

    #[test]
    fn an_account_reads_its_own_folder_unless_its_keys_say_otherwise() {
        let root = std::env::temp_dir().join(format!("usage-accounts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let claude = account_dir(&root, "claude", "work");
        let codex = account_dir(&root, "codex", "client-2");
        let gemini = account_dir(&root, "gemini", "lab");
        let env = env_of(&[
            ("HOME", "/home/dev-a"),
            ("USAGE_CACHE", "/default-cache.json"),
            ("USAGE_FETCH", "/f.sh"),
            ("CODEX_HOME", "/default-codex"),
            ("USAGE_SOURCE_gemini", "none"),
        ]);
        let of = |account: &Account| AgentSource::for_account(&env, account.clone()).unwrap();
        assert_eq!(
            of(&claude).source,
            Source::ClaudeCache(claude.dir.join("usage-cache.json"))
        );
        assert_eq!(
            of(&claude).fetch,
            None,
            "claude's own fetch would read another account's login"
        );
        assert_eq!(
            of(&claude).missing_fetch().as_deref(),
            Some("no usage fetch for this account: set USAGE_FETCH_claude_work")
        );
        assert_eq!(
            of(&codex).source,
            Source::CodexSessions(codex.dir.join("sessions"))
        );
        assert_eq!(of(&codex).missing_fetch(), None);
        assert_eq!(
            AgentSource::from_env(&env, "claude").unwrap().fetch,
            Some("/f.sh".into()),
            "the default account keeps claude's fetch"
        );
        assert_eq!(of(&gemini).source, Source::None);
        let own = env_of(&[
            ("USAGE_SOURCE_codex_client_2", "codex-sessions"),
            ("USAGE_SOURCE_claude_work", "json:/w.json"),
            ("USAGE_FETCH_claude_work", "/w.sh"),
        ]);
        let of = |account: &Account| AgentSource::for_account(&own, account.clone()).unwrap();
        assert_eq!(of(&claude).source, Source::Json("/w.json".into()));
        assert_eq!(of(&claude).fetch, Some("/w.sh".into()));
        assert_eq!(
            of(&codex).source,
            Source::CodexSessions(codex.dir.join("sessions")),
            "a bare codex-sessions names the account's own"
        );
        assert_eq!(of(&gemini).source, Source::Unset);
        let bad = env_of(&[("USAGE_SOURCE_claude_work", "s3://x")]);
        assert!(
            AgentSource::for_account(&bad, claude.clone())
                .unwrap_err()
                .starts_with("USAGE_SOURCE_claude_work")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_accounts_fetch_runs_with_its_folder_and_its_own_credentials() {
        let root = std::env::temp_dir().join(format!("usage-fetch-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let claude = account_dir(&root, "claude", "work");
        let codex = account_dir(&root, "codex", "home");
        let env = env_of(&[]);
        let source = AgentSource::for_account(&env, claude.clone()).unwrap();
        let cache = claude.dir.join("usage-cache.json").display().to_string();
        let dir = claude.dir.display().to_string();
        assert_eq!(
            source.fetch_env(),
            (
                vec![
                    ("CLAUDE_CONFIG_DIR".to_owned(), dir.clone()),
                    ("USAGE_CACHE".to_owned(), cache.clone())
                ],
                vec!["USAGE_CREDENTIALS_FILE"]
            )
        );
        std::fs::write(claude.dir.join(".credentials.json"), "{}").unwrap();
        let credentials = claude.dir.join(".credentials.json").display().to_string();
        assert_eq!(
            source.fetch_env().0,
            [
                ("CLAUDE_CONFIG_DIR".to_owned(), dir),
                ("USAGE_CACHE".to_owned(), cache),
                ("USAGE_CREDENTIALS_FILE".to_owned(), credentials)
            ]
        );
        let codex = AgentSource::for_account(&env, codex.clone()).unwrap();
        assert_eq!(codex.fetch_env().0[0].0, "CODEX_HOME");
        assert_eq!(
            AgentSource::from_env(&env_of(&[("HOME", "/h")]), "claude")
                .unwrap()
                .fetch_env(),
            (Vec::new(), Vec::new())
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn caps_cooldowns_and_the_pool_count_per_account() {
        let dir = std::env::temp_dir().join(format!("usage-pool-accounts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("accounts");
        let work = account_dir(&root, "claude", "work");
        let home = account_dir(&root, "claude", "home");
        account_dir(&root, "codex", "gone");
        std::fs::remove_dir(root.join("codex/codex-gone")).unwrap();
        let cache = |account: &Account, five: u32| {
            std::fs::write(
                account.dir.join("usage-cache.json"),
                format!(
                    r#"{{"timestamp": {}, "data": {{"fiveHourPercent": {five}, "weeklyPercent": 10}}}}"#,
                    now_ms()
                ),
            )
            .unwrap();
        };
        cache(&work, 65);
        cache(&home, 75);
        std::fs::create_dir_all(dir.join("state/model-cooldown")).unwrap();
        let state = dir.join("state").display().to_string();
        let root_text = root.display().to_string();
        let env = env_of(&[
            (
                "TICK_MODELS",
                "claude@work:opus claude@home:opus claude@home:sonnet codex@gone:gpt",
            ),
            ("AGENT_ACCOUNTS_DIR", &root_text),
            ("USAGE_FETCH_claude_work", "/no-fetch"),
            ("USAGE_FETCH_claude_home", "/no-fetch"),
            ("HARNESS_STATE", &state),
            ("MAX_SLOTS", "8"),
        ]);
        let both = pool(&env).unwrap();
        let caps: Vec<(String, u32)> = both
            .agents
            .iter()
            .map(|usage| (usage.key(), usage.cap))
            .collect();
        assert_eq!(
            caps,
            [
                ("claude@work".to_owned(), 4),
                ("claude@home".to_owned(), 1),
                ("codex@gone".to_owned(), 0)
            ]
        );
        assert_eq!(
            both.pool_cap, 5,
            "each account's cap counts toward the pool"
        );
        assert!(
            matches!(&both.agents[2].reading, Freshness::Unreadable(why) if why.contains("codex-gone")),
            "{:?}",
            both.agents[2].reading
        );
        std::fs::write(dir.join("state/model-cooldown/claude@home:opus"), "").unwrap();
        assert_eq!(
            pool(&env).unwrap().pool_cap,
            5,
            "claude@home:sonnet still runs"
        );
        std::fs::write(dir.join("state/model-cooldown/claude@home:sonnet"), "").unwrap();
        let cooled = pool(&env).unwrap();
        assert_eq!(
            (cooled.pool_cap, cooled.cooling),
            (4, vec!["claude@home".to_owned()])
        );
        std::fs::remove_dir_all(dir.join("state/model-cooldown")).unwrap();
        std::fs::create_dir_all(dir.join("state/accounts")).unwrap();
        std::fs::write(
            dir.join("state/accounts/claude@work.json"),
            r#"{"state": "signed_out", "method": null, "expires_at": null, "checked_at": 1}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("state/accounts/claude@home.json"),
            r#"{"state": "unknown", "method": null, "expires_at": null, "checked_at": 1}"#,
        )
        .unwrap();
        let signed_out = pool(&env).unwrap();
        assert_eq!(
            (signed_out.agents[0].cap, signed_out.agents[1].cap),
            (0, 1),
            "signed out allows nothing; unknown blocks nothing"
        );
        assert_eq!(signed_out.pool_cap, 1);
        let state_path = dir.join("state");
        let candidates = [
            Candidate {
                entry: Entry::parse("claude@work:opus", "claude").unwrap(),
                cooling: cooling(
                    &state_path,
                    &Entry::parse("claude@work:opus", "claude").unwrap(),
                    Duration::from_secs(60),
                ),
                cap: signed_out.agents[0].cap,
                running: 0,
            },
            Candidate {
                entry: Entry::parse("claude@home:opus", "claude").unwrap(),
                cooling: false,
                cap: signed_out.agents[1].cap,
                running: 0,
            },
        ];
        assert_eq!(
            pick(&candidates, &[], false).map(ToString::to_string),
            Some("claude@home:opus".to_owned()),
            "a signed-out account is passed over for the next"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_claude_account_without_its_own_fetch_allows_nothing() {
        let dir = std::env::temp_dir().join(format!("usage-no-fetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("accounts");
        let work = account_dir(&root, "claude", "work");
        std::fs::write(
            work.dir.join("usage-cache.json"),
            format!(
                r#"{{"timestamp": {}, "data": {{"fiveHourPercent": 10, "weeklyPercent": 10}}}}"#,
                now_ms()
            ),
        )
        .unwrap();
        let root_text = root.display().to_string();
        let state = dir.join("state").display().to_string();
        let env = env_of(&[
            ("AGENT_ACCOUNTS_DIR", &root_text),
            ("HARNESS_STATE", &state),
            ("USAGE_FETCH", "/f.sh"),
        ]);
        let usage = agent_usage(&env, &settings(&[]), "claude", Some("work"), false).unwrap();
        assert_eq!(
            (usage.cap, usage.reading),
            (
                0,
                Freshness::Unreadable(
                    "no usage fetch for this account: set USAGE_FETCH_claude_work".to_owned()
                )
            ),
            "even a fresh cache is not trusted: nothing of this account's wrote it"
        );
        let state = dir.join("state");
        assert!(crate::accounts::first_time(
            &state,
            "claude@work",
            "no-fetch",
            true
        ));
        assert!(!crate::accounts::first_time(
            &state,
            "claude@work",
            "no-fetch",
            true
        ));
        assert!(!crate::accounts::first_time(
            &state,
            "claude@work",
            "no-fetch",
            false
        ));
        assert!(crate::accounts::first_time(
            &state,
            "claude@work",
            "no-fetch",
            true
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
