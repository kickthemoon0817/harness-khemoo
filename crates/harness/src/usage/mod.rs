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

use crate::home::harness_home;
pub use sources::{Reading, Source, Window};

/// A window this long or longer is a long one.
const LONG_WINDOW_MINUTES: u64 = 24 * 60;
/// A fetch that hangs must not stall the launcher past its next firing.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// How the environment is read: the value of a name, `None` when it is unset or empty.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

#[derive(clap::Args)]
#[command(
    args_conflicts_with_subcommands = true,
    after_help = "\
With no subcommand, prints each agent of TICK_MODELS (and LANE_MODEL) with its windows and cap, and
the pool's cap. Reads TICK_MODELS, TICK_AGENT, LANE_MODEL, MAX_SLOTS, TICK_WIDTH and the USAGE_*
keys docs/INSTANCE.md gives from the environment."
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
        #[arg(long)]
        agent: String,
    },
    /// Print the best entry of the model list that is not cooling down and whose agent runs fewer ticks
    /// than its cap; exit 1 when there is none.
    Pick {
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
}

pub fn run(args: &Args) -> ExitCode {
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let result = match &args.command {
        None => report(&env, args.json),
        Some(Sub::Check { agent }) => check(&env, agent),
        Some(Sub::Pick {
            tried,
            landing,
            entry,
        }) => pick_command(&env, tried, *landing, entry.as_deref()),
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

/// One entry of the model list: an agent and the model it runs.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Entry {
    pub agent: String,
    pub model: String,
}

impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.agent, self.model)
    }
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
    /// Parses `agent:model`, or a bare `model` run by `default_agent`. The entry names its cooldown
    /// file, so the model holds no `/` and no control character.
    pub fn parse(text: &str, default_agent: &str) -> Result<Self, String> {
        let (agent, model) = text.split_once(':').unwrap_or((default_agent, text));
        if !valid_agent(agent) {
            return Err(format!(
                "'{text}': agent '{agent}' is not 1 to 32 of a-z, 0-9 and _"
            ));
        }
        if model.is_empty()
            || model.len() > 128
            || model.contains('/')
            || model.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(format!("'{text}': '{model}' is not a model name"));
        }
        Ok(Self {
            agent: agent.to_owned(),
            model: model.to_owned(),
        })
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

/// Where one agent's reading comes from, and what refreshes it.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentSource {
    pub agent: String,
    pub source: Source,
    pub fetch: Option<PathBuf>,
}

impl AgentSource {
    /// `USAGE_SOURCE_<agent>` and `USAGE_FETCH_<agent>`. Claude reads its cache, `USAGE_CACHE`, and
    /// refreshes it with `USAGE_FETCH`; codex reads its session rollouts; any other agent has no
    /// windows unless a source is set.
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
            (None, _) => Source::None,
        };
        let fetch = match (env(&format!("USAGE_FETCH_{agent}")), agent) {
            (Some(path), _) => Some(PathBuf::from(path)),
            (None, "claude") => env("USAGE_FETCH")
                .map(PathBuf::from)
                .or_else(|| home().map(|home| home.join(".claude/scripts/usage-fetch.sh"))),
            (None, _) => None,
        };
        Ok(Self {
            agent: agent.to_owned(),
            source,
            fetch,
        })
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

/// One agent's windows and the ticks they allow.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AgentUsage {
    pub agent: String,
    pub source: String,
    pub reading: Freshness,
    pub taken_ms: Option<u64>,
    pub windows: Vec<WindowCap>,
    pub cap: u32,
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
        source: source.source.describe(),
        reading: Freshness::Unmetered,
        taken_ms: None,
        windows: Vec::new(),
        cap: settings.width,
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
            run_fetch(fetch);
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
fn run_fetch(fetch: &Path) {
    let Ok(mut child) = Command::new(fetch)
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

/// One agent's windows now, refreshed first through its fetch command when `refresh` is set.
pub fn agent_usage(
    env: Lookup,
    settings: &Settings,
    agent: &str,
    refresh: bool,
) -> Result<AgentUsage, String> {
    let source = AgentSource::from_env(env, agent)?;
    let reading = match source.source {
        Source::None => Err(String::new()),
        _ if refresh => read_refreshed(settings, &source),
        _ => sources::read(&source.source),
    };
    assess(env, settings, &source, reading, now_ms())
}

/// The distinct agents of the entries, in order of first appearance.
fn agents_of<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Vec<String> {
    let mut agents: Vec<String> = Vec::new();
    for entry in entries {
        if !agents.contains(&entry.agent) {
            agents.push(entry.agent.clone());
        }
    }
    agents
}

/// Every agent of the model list and the lane, with its windows, and the pool's cap.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Pool {
    pub width: u32,
    pub pool_cap: u32,
    pub agents: Vec<AgentUsage>,
    /// The agents each of whose entries cools down on its usage limit.
    pub cooling: Vec<String>,
}

/// The pool as the settings in `env` read now; `refresh` runs each stale agent's fetch command first.
/// An agent each of whose entries cools down adds nothing to the pool's cap: no runner could pick it.
pub fn pool(env: Lookup, refresh: bool) -> Result<Pool, String> {
    let settings = Settings::from_env(env)?;
    let (entries, lane) = model_entries(env)?;
    let entries: Vec<Entry> = entries.into_iter().chain(lane).collect();
    let cooldown = Duration::from_secs(number(env, "MODEL_COOLDOWN_S", 1800)?);
    let state = state_dir(env);
    let agents = agents_of(&entries)
        .iter()
        .map(|agent| agent_usage(env, &settings, agent, refresh))
        .collect::<Result<Vec<_>, _>>()?;
    let cooling: Vec<String> = agents
        .iter()
        .map(|usage| usage.agent.clone())
        .filter(|agent| {
            state.as_ref().is_some_and(|state| {
                entries
                    .iter()
                    .filter(|entry| &entry.agent == agent)
                    .all(|entry| cooling(state, entry, cooldown))
            })
        })
        .collect();
    Ok(Pool {
        width: settings.width,
        pool_cap: pool_cap(
            settings.width,
            agents
                .iter()
                .filter(|usage| !cooling.contains(&usage.agent))
                .map(|usage| usage.cap),
        ),
        agents,
        cooling,
    })
}

/// `HARNESS_STATE`, else the checkout's `state/`.
fn state_dir(env: Lookup) -> Option<PathBuf> {
    env("HARNESS_STATE")
        .map(PathBuf::from)
        .or_else(|| harness_home().map(|home| home.join("state")))
}

fn report(env: Lookup, json: bool) -> Result<ExitCode, String> {
    let report = pool(env, true)?;
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
        let cooling = if report.cooling.contains(&agent.agent) {
            ", every entry cooling down"
        } else {
            ""
        };
        println!(
            "{}: cap {} of {}{cooling}, {}{state}",
            agent.agent, agent.cap, report.width, agent.source
        );
        for window in &agent.windows {
            let [half, one, none] = window.steps.0;
            println!(
                "  {} {}% of {} min, steps {half} {one} {none}: {}",
                window.name, window.used_pct, window.window_minutes, window.cap
            );
        }
    }
    println!("pool: {} of {}", report.pool_cap, report.width);
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

fn check(env: Lookup, agent: &str) -> Result<ExitCode, String> {
    if !valid_agent(agent) {
        return Err(format!(
            "--agent: '{agent}' is not 1 to 32 of a-z, 0-9 and _"
        ));
    }
    let settings = Settings::from_env(env)?;
    let usage = agent_usage(env, &settings, agent, true)?;
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
        format!("{agent}: no windows")
    } else {
        format!("{agent}: {}", windows.join(" "))
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
                "usage unknown: no fresh reading for {agent} from {} ({why}); start nothing",
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

/// The first candidate not tried and not cooling down whose agent may run one more tick: under its
/// cap, or, for a landing tick, which may start past the width, with a cap above 0.
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
/// `<state>/model-cooldown/<agent>:<model>` when it does.
fn cooling(state: &Path, entry: &Entry, cooldown: Duration) -> bool {
    std::fs::metadata(state.join("model-cooldown").join(entry.to_string()))
        .and_then(|meta| meta.modified())
        .is_ok_and(|at| at.elapsed().unwrap_or_default() < cooldown)
}

fn pick_command(
    env: Lookup,
    tried: &str,
    landing: bool,
    only: Option<&str>,
) -> Result<ExitCode, String> {
    let settings = Settings::from_env(env)?;
    let (entries, _) = model_entries(env)?;
    let default_agent = env("TICK_AGENT").unwrap_or_else(|| "claude".to_owned());
    let entries = match only {
        Some(text) => vec![Entry::parse(text, &default_agent)?],
        None => entries,
    };
    let tried =
        Entry::list(tried, &default_agent).map_err(|message| format!("--tried: {message}"))?;
    let state = state_dir(env).ok_or("no state directory: set HARNESS_STATE")?;
    let cooldown = Duration::from_secs(number(env, "MODEL_COOLDOWN_S", 1800)?);
    let mut running: HashMap<String, u32> = HashMap::new();
    for tick in crate::ticks::scan(&state.to_string_lossy())
        .map_err(|error| format!("cannot count the running ticks: {error}"))?
    {
        *running.entry(tick.agent).or_default() += 1;
    }
    let mut caps = BTreeMap::new();
    for agent in agents_of(&entries) {
        caps.insert(
            agent.clone(),
            agent_usage(env, &settings, &agent, true)?.cap,
        );
    }
    let candidates: Vec<Candidate> = entries
        .into_iter()
        .map(|entry| Candidate {
            cooling: cooling(&state, &entry, cooldown),
            cap: caps.get(&entry.agent).copied().unwrap_or(0),
            running: running.get(&entry.agent).copied().unwrap_or(0),
            entry,
        })
        .collect();
    Ok(match pick(&candidates, &tried, landing) {
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
        assert_eq!(cap_of(&[], &[]), 8, "no windows: nothing limits the agent");
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
    fn an_unmetered_agent_has_the_full_width() {
        let s = settings(&[("MAX_SLOTS", "3")]);
        let source = AgentSource {
            agent: "gemini".to_owned(),
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
    fn entries_name_an_agent_or_take_the_default() {
        assert_eq!(
            Entry::list("claude:opus codex:gpt-5.5 gemini-3-pro", "claude").unwrap(),
            [
                Entry {
                    agent: "claude".into(),
                    model: "opus".into()
                },
                Entry {
                    agent: "codex".into(),
                    model: "gpt-5.5".into()
                },
                Entry {
                    agent: "claude".into(),
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
        assert_eq!(of("gemini").source, Source::None);
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
        let both = pool(&env, false).unwrap();
        std::fs::write(dir.join("state/model-cooldown/codex:b"), "").unwrap();
        let one_cooling = pool(&env, false).unwrap();
        std::fs::write(dir.join("state/model-cooldown/codex:c"), "").unwrap();
        let all_cooling = pool(&env, false).unwrap();
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
            agents_of(entries.iter().chain(lane.iter())),
            ["claude", "codex", "gemini"]
        );
        let bare = env_of(&[("TICK_AGENT", "codex"), ("TICK_MODELS", "gpt-5.5")]);
        assert_eq!(
            model_entries(&bare).unwrap().0[0].to_string(),
            "codex:gpt-5.5"
        );
    }
}
