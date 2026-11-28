//! `harness ticks`: the live ticks of one harness state directory.
//!
//! The runner starts every tick, a local agent CLI or `fleet dispatch`, with
//! `HARNESS_TICK="state=<HARNESS_STATE> slot=<n> agent=<agent> model=<model> kind=<kind>"` in its
//! environment. A tick is the topmost process carrying it: one whose parent does not. Everything a
//! tick starts inherits the variable, a node shim's native agent and every tool shell alike, so each
//! resolves to the process the runner started. Only this user's processes are read, and only markers
//! naming this state directory count, so two installations on one host never count each other's.
//!
//! A daemon a tick leaves behind (an ssh master, a gpg or build agent) keeps the marker once its
//! parent is gone, so a tick must also hold its slot's lock open, as the descriptor the runner's
//! process inherits does; a daemon closes the descriptors it was started with.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;

use crate::home::{env_nonempty, harness_home};

/// Linux caps argv and the environment together at a quarter of the stack limit: 2 MiB under the
/// usual 8 MiB stack. Past this the marker may be missed, and the process is not counted.
const ENVIRON_LIMIT: u64 = 4 << 20;
const STAT_LIMIT: u64 = 4 << 10;
const MARKER: &[u8] = b"HARNESS_TICK=";
/// How many of a process's open descriptors are looked at for its slot lock.
const FD_LIMIT: usize = 4096;

#[derive(clap::Args)]
#[command(after_help = "\
Prints one tick a line, oldest first: pid, age in seconds, slot, agent, model and kind (work, lane,
landing or supervisor). Reads the ticks of --state, else $HARNESS_STATE, else $HARNESS_HOME/state.")]
pub struct Args {
    /// Print a JSON document instead of lines.
    #[arg(long)]
    json: bool,
    /// The state directory whose ticks are listed.
    #[arg(long)]
    state: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Work,
    Lane,
    Landing,
    /// The supervisor: it holds no slot and claims nothing.
    Supervisor,
}

impl Kind {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "work" => Some(Self::Work),
            "lane" => Some(Self::Lane),
            "landing" => Some(Self::Landing),
            "supervisor" => Some(Self::Supervisor),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Lane => "lane",
            Self::Landing => "landing",
            Self::Supervisor => "supervisor",
        }
    }
}

/// What a `HARNESS_TICK` value says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    pub state: String,
    pub slot: u32,
    pub agent: String,
    pub model: String,
    pub kind: Kind,
}

impl Marker {
    /// Parses a `HARNESS_TICK` value. The state directory comes first and may hold spaces, so the
    /// four fields after it are split off from the right.
    pub fn parse(value: &str) -> Option<Self> {
        let rest = value.strip_prefix("state=")?;
        let mut fields = rest.rsplitn(5, ' ');
        let kind = Kind::parse(fields.next()?.strip_prefix("kind=")?)?;
        let model = fields.next()?.strip_prefix("model=")?;
        let agent = fields.next()?.strip_prefix("agent=")?;
        let slot = fields.next()?.strip_prefix("slot=")?.parse().ok()?;
        let state = fields.next()?;
        if state.is_empty() || agent.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self {
            state: state.to_owned(),
            slot,
            agent: agent.to_owned(),
            model: model.to_owned(),
            kind,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Tick {
    pub pid: u32,
    pub age_s: u64,
    pub slot: u32,
    pub agent: String,
    pub model: String,
    pub kind: Kind,
}

#[derive(Serialize)]
struct Document<'a> {
    ticks: &'a [Tick],
}

pub fn run(args: &Args) -> ExitCode {
    let Some(state) = args
        .state
        .clone()
        .or_else(|| env_nonempty("HARNESS_STATE").map(PathBuf::from))
        .or_else(|| harness_home().map(|home| home.join("state")))
    else {
        eprintln!("harness ticks: no state directory: set HARNESS_STATE or pass --state");
        return ExitCode::from(2);
    };
    let ticks = match scan(&state.to_string_lossy()) {
        Ok(ticks) => ticks,
        Err(error) => {
            eprintln!("harness ticks: cannot read /proc: {error}");
            return ExitCode::from(1);
        }
    };
    if args.json {
        match serde_json::to_string(&Document { ticks: &ticks }) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("harness ticks: {error}");
                return ExitCode::from(1);
            }
        }
    } else {
        for tick in &ticks {
            println!(
                "{} {} {} {} {} {}",
                tick.pid,
                tick.age_s,
                tick.slot,
                tick.agent,
                tick.model,
                tick.kind.as_str()
            );
        }
    }
    ExitCode::SUCCESS
}

/// The live ticks whose marker names `state`, oldest first.
pub fn scan(state: &str) -> io::Result<Vec<Tick>> {
    scan_in(Path::new("/proc"), state)
}

struct Process {
    ppid: u32,
    start_ticks: u64,
    marker: Option<Marker>,
}

fn scan_in(proc_root: &Path, state: &str) -> io::Result<Vec<Tick>> {
    let uid = rustix::process::getuid().as_raw();
    let uptime = uptime_s(proc_root)?;
    #[allow(clippy::cast_precision_loss)]
    let clock_ticks = rustix::param::clock_ticks_per_second() as f64;
    let mut processes = HashMap::new();
    for entry in std::fs::read_dir(proc_root)? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .filter(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        // A process that exits mid-read is simply left out.
        if !entry.metadata().is_ok_and(|meta| meta.uid() == uid) {
            continue;
        }
        let Some((ppid, start_ticks)) = read_limited(&entry.path().join("stat"), STAT_LIMIT)
            .ok()
            .and_then(|stat| parse_stat(&stat))
        else {
            continue;
        };
        let marker = read_limited(&entry.path().join("environ"), ENVIRON_LIMIT)
            .ok()
            .and_then(|environ| marker_of(&environ))
            .filter(|marker| marker.state == state);
        processes.insert(
            pid,
            Process {
                ppid,
                start_ticks,
                marker,
            },
        );
    }
    let locks = Path::new(state).join("locks");
    let locks = std::fs::canonicalize(&locks).unwrap_or(locks);
    let mut ticks: Vec<Tick> = processes
        .iter()
        .filter_map(|(pid, process)| {
            let marker = process.marker.as_ref()?;
            let parent_marked = processes
                .get(&process.ppid)
                .is_some_and(|parent| parent.marker.is_some());
            let lock = if marker.kind == Kind::Supervisor {
                locks.join("supervisor.lock")
            } else {
                locks.join(format!("slot{}.lock", marker.slot))
            };
            if parent_marked || !holds(proc_root, *pid, &lock) {
                return None;
            }
            #[allow(clippy::cast_precision_loss)]
            let started = process.start_ticks as f64 / clock_ticks;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let age_s = (uptime - started).max(0.0) as u64;
            Some(Tick {
                pid: *pid,
                age_s,
                slot: marker.slot,
                agent: marker.agent.clone(),
                model: marker.model.clone(),
                kind: marker.kind,
            })
        })
        .collect();
    ticks.sort_by(|a, b| b.age_s.cmp(&a.age_s).then(a.pid.cmp(&b.pid)));
    Ok(ticks)
}

/// Whether process `pid` has `lock` open.
fn holds(proc_root: &Path, pid: u32, lock: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(proc_root.join(pid.to_string()).join("fd")) else {
        return false;
    };
    entries
        .take(FD_LIMIT)
        .flatten()
        .any(|entry| std::fs::read_link(entry.path()).is_ok_and(|target| target == lock))
}

fn read_limited(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn uptime_s(proc_root: &Path) -> io::Result<f64> {
    let text = read_limited(&proc_root.join("uptime"), STAT_LIMIT)?;
    String::from_utf8_lossy(&text)
        .split_whitespace()
        .next()
        .and_then(|first| first.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no uptime in /proc/uptime"))
}

/// The parent pid and the start time in clock ticks, from `/proc/<pid>/stat`. The command name in
/// parentheses may hold spaces and parentheses itself, so the fields are read after the last `)`.
fn parse_stat(stat: &[u8]) -> Option<(u32, u64)> {
    let text = String::from_utf8_lossy(stat);
    let (_, rest) = text.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After the name: state, ppid, ..., starttime as the 20th.
    let ppid = fields.get(1)?.parse().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    Some((ppid, start))
}

fn marker_of(environ: &[u8]) -> Option<Marker> {
    environ
        .split(|&byte| byte == 0)
        .find_map(|entry| entry.strip_prefix(MARKER))
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(Marker::parse)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marker_parses_with_spaces_in_its_state() {
        let marker =
            Marker::parse("state=/srv/my harness/state slot=3 agent=codex model=gpt-5.5 kind=lane")
                .unwrap();
        assert_eq!(
            marker,
            Marker {
                state: "/srv/my harness/state".to_owned(),
                slot: 3,
                agent: "codex".to_owned(),
                model: "gpt-5.5".to_owned(),
                kind: Kind::Lane,
            }
        );
    }

    #[test]
    fn a_malformed_marker_is_none() {
        for value in [
            "",
            "slot=1 agent=a model=m kind=work",
            "state=/s slot=x agent=a model=m kind=work",
            "state=/s slot=1 agent=a model=m kind=other",
            "state=/s slot=1 agent= model=m kind=work",
            "state= slot=1 agent=a model=m kind=work",
            "state=/s slot=1 model=m kind=work",
        ] {
            assert_eq!(Marker::parse(value), None, "{value:?}");
        }
    }

    #[test]
    fn the_marker_is_found_among_the_environment() {
        let environ =
            b"PATH=/bin\0HARNESS_TICK=state=/s slot=1 agent=claude model=m kind=work\0X=1\0";
        assert_eq!(marker_of(environ).unwrap().agent, "claude");
        assert_eq!(marker_of(b"PATH=/bin\0NOT_HARNESS_TICK=state=/s\0"), None);
    }

    #[test]
    fn stat_is_read_after_the_last_parenthesis() {
        let stat = b"42 (a) b (c) S 7 42 42 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 123456 0 0";
        assert_eq!(parse_stat(stat), Some((7, 123_456)));
        assert_eq!(parse_stat(b"42 (short) S 7"), None);
    }
}
