//! `harness ticks`: the live ticks of one harness state directory.
//!
//! The runner starts each tick, a local agent CLI or `fleet dispatch`, as its own child and records
//! it in `<state>/locks/slot<n>.tick` as `pid starttime bootid agent model kind` until it ends, so a
//! slot holds one tick at most; the supervisor records its own in `<state>/locks/supervisor.tick`,
//! read as slot 0. A tick on an account records its agent as `agent@account`. A tick is live while
//! this boot's `/proc/<pid>/stat` gives the same start time, which a reused pid does not. Only `stat` is read, never the process's environment or
//! descriptors: an agent CLI that marks itself non-dumpable, as the release Codex CLI does, keeps
//! both private.

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;

use crate::home::{env_nonempty, harness_home};

const STAT_LIMIT: u64 = 4 << 10;
/// A slot file holds one short line.
const SLOT_FILE_LIMIT: u64 = 1 << 10;
/// How many entries of the locks directory are looked at.
const LOCKS_LIMIT: usize = 4096;

#[derive(clap::Args)]
#[command(after_help = "\
Prints one tick a line, oldest first: pid, age in seconds, slot, agent (agent@account for a tick on
an account), model and kind (work, lane, landing or supervisor). Reads the ticks of --state, else $HARNESS_STATE, else $HARNESS_HOME/state.")]
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

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Tick {
    pub pid: u32,
    pub age_s: u64,
    pub slot: u32,
    pub agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub model: String,
    pub kind: Kind,
}

impl Tick {
    /// `agent`, or `agent@account`: what its usage cap counts it toward.
    pub fn key(&self) -> String {
        crate::usage::key_of(&self.agent, self.account.as_deref())
    }
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
            eprintln!("harness ticks: {error}");
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
                tick.key(),
                tick.model,
                tick.kind.as_str()
            );
        }
    }
    ExitCode::SUCCESS
}

/// The live ticks recorded under `state`, oldest first.
pub fn scan(state: &str) -> io::Result<Vec<Tick>> {
    scan_in(Path::new("/proc"), Path::new(state))
}

/// What a slot file records.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    pid: u32,
    start_ticks: u64,
    boot_id: String,
    agent: String,
    account: Option<String>,
    model: String,
    kind: Kind,
}

impl Record {
    fn parse(text: &str) -> Option<Self> {
        let mut fields = text.split_whitespace();
        let pid = fields.next()?.parse().ok().filter(|pid| *pid > 1)?;
        let start_ticks = fields.next()?.parse().ok()?;
        let boot_id = fields.next()?.to_owned();
        let key = fields.next()?;
        let (agent, account) = match key.split_once('@') {
            Some((agent, account)) => (agent, Some(account.to_owned())),
            None => (key, None),
        };
        let record = Self {
            pid,
            start_ticks,
            boot_id,
            agent: agent.to_owned(),
            account,
            model: fields.next()?.to_owned(),
            kind: Kind::parse(fields.next()?)?,
        };
        fields.next().is_none().then_some(record)
    }
}

fn scan_in(proc_root: &Path, state: &Path) -> io::Result<Vec<Tick>> {
    let entries = match std::fs::read_dir(state.join("locks")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let uid = rustix::process::getuid().as_raw();
    let mut boot_id: Option<String> = None;
    let mut found = Vec::new();
    for entry in entries.take(LOCKS_LIMIT).flatten() {
        let name = entry.file_name();
        // The supervisor, which holds no slot, records its tick beside them as slot 0.
        let Some(slot) = name.to_str().and_then(|name| match name {
            "supervisor.tick" => Some(0),
            _ => name
                .strip_prefix("slot")?
                .strip_suffix(".tick")?
                .parse::<u32>()
                .ok(),
        }) else {
            continue;
        };
        // The runner writes it 0600 in this user's state directory; any other file is not its.
        if !entry
            .metadata()
            .is_ok_and(|meta| meta.is_file() && meta.uid() == uid)
        {
            continue;
        }
        let Some(record) = read_regular(&entry.path(), SLOT_FILE_LIMIT)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| Record::parse(&text))
        else {
            continue;
        };
        // Start times count from boot, so a file a crash left behind could match a new process
        // after a reboot: only this boot's records count.
        if boot_id.is_none() {
            boot_id = Some(read_boot_id(proc_root)?);
        }
        if boot_id.as_deref() == Some(record.boot_id.as_str())
            && start_ticks(proc_root, record.pid) == Some(record.start_ticks)
        {
            found.push((slot, record));
        }
    }
    if found.is_empty() {
        return Ok(Vec::new());
    }
    let uptime = uptime_s(proc_root)?;
    #[allow(clippy::cast_precision_loss)]
    let clock_ticks = rustix::param::clock_ticks_per_second() as f64;
    let mut ticks: Vec<Tick> = found
        .into_iter()
        .map(|(slot, record)| {
            #[allow(clippy::cast_precision_loss)]
            let started = record.start_ticks as f64 / clock_ticks;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let age_s = (uptime - started).max(0.0) as u64;
            Tick {
                pid: record.pid,
                age_s,
                slot,
                agent: record.agent,
                account: record.account,
                model: record.model,
                kind: record.kind,
            }
        })
        .collect();
    ticks.sort_by(|a, b| b.age_s.cmp(&a.age_s).then(a.slot.cmp(&b.slot)));
    Ok(ticks)
}

/// The start time of process `pid` in clock ticks, from `/proc/<pid>/stat`, which stays readable
/// for a non-dumpable process.
fn start_ticks(proc_root: &Path, pid: u32) -> Option<u64> {
    read_limited(&proc_root.join(pid.to_string()).join("stat"), STAT_LIMIT)
        .ok()
        .and_then(|stat| parse_start(&stat))
}

fn read_regular(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    crate::regular::open(path)?
        .take(limit)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_limited(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_boot_id(proc_root: &Path) -> io::Result<String> {
    let bytes = read_limited(&proc_root.join("sys/kernel/random/boot_id"), STAT_LIMIT)?;
    let id = String::from_utf8_lossy(&bytes).trim().to_owned();
    if id.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "an empty boot id",
        ));
    }
    Ok(id)
}

fn uptime_s(proc_root: &Path) -> io::Result<f64> {
    let text = read_limited(&proc_root.join("uptime"), STAT_LIMIT)?;
    String::from_utf8_lossy(&text)
        .split_whitespace()
        .next()
        .and_then(|first| first.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no uptime in /proc/uptime"))
}

/// The start time in clock ticks, field 22 of `/proc/<pid>/stat`. The command name in parentheses
/// may hold spaces and parentheses itself, so the fields are counted after the last `)`.
fn parse_start(stat: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(stat);
    let (_, rest) = text.rsplit_once(')')?;
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_record_parses() {
        assert_eq!(
            Record::parse("4242 123456 b00t codex gpt-5.5 lane\n"),
            Some(Record {
                pid: 4242,
                start_ticks: 123_456,
                boot_id: "b00t".to_owned(),
                agent: "codex".to_owned(),
                account: None,
                model: "gpt-5.5".to_owned(),
                kind: Kind::Lane,
            })
        );
        let on_account = Record::parse("4242 123456 b00t claude@work opus work").unwrap();
        assert_eq!(
            (on_account.agent.as_str(), on_account.account.as_deref()),
            ("claude", Some("work"))
        );
    }

    #[test]
    fn a_malformed_slot_record_is_none() {
        for text in [
            "",
            "4242 123456 b codex gpt-5.5",
            "4242 123456 codex gpt-5.5 work",
            "x 123456 b codex m work",
            "1 123456 b codex m work",
            "4242 -1 b codex m work",
            "4242 123456 b codex m other",
            "4242 123456 b codex m work extra",
        ] {
            assert_eq!(Record::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn stat_is_read_after_the_last_parenthesis() {
        let stat = b"42 (a) b (c) S 7 42 42 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 123456 0 0";
        assert_eq!(parse_start(stat), Some(123_456));
        assert_eq!(parse_start(b"42 (short) S 7"), None);
    }

    #[test]
    fn a_state_without_locks_has_no_ticks() {
        let missing = std::env::temp_dir().join(format!("ticks-none-{}", std::process::id()));
        assert_eq!(scan_in(Path::new("/proc"), &missing).unwrap(), []);
    }
}
