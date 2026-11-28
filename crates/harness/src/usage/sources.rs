//! Where an agent's usage reading comes from, each parsed into the same windows.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde_json::Value;

/// A cache or normalized file larger than this is not a usage reading.
pub const FILE_LIMIT: u64 = 1 << 20;
/// How much of the newest rollout is read, from its end: its last `rate_limits` lies near it, and a
/// session's rollout grows past hundreds of megabytes.
pub const ROLLOUT_TAIL: u64 = 4 << 20;
/// How many entries of one directory of the sessions tree are read: past it the walk gives up
/// rather than read part of a directory and miss its newest rollout.
const DIR_LIMIT: usize = 10_000;
/// A `resets_at` number under this cannot be Unix seconds of this century.
const EPOCH_SECONDS_MIN: u64 = 1_000_000_000;
/// How many of the newest rollouts are tried for `rate_limits`: a session that just started has
/// none until its first response.
const ROLLOUTS_TRIED: usize = 16;
const MAX_WINDOWS: usize = 64;

/// A source of usage windows, as `USAGE_SOURCE_<agent>` names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The normalized document.
    Json(PathBuf),
    /// Claude Code's usage cache.
    ClaudeCache(PathBuf),
    /// Codex's session rollouts, which record the account's limits as codex last saw them.
    CodexSessions(PathBuf),
    /// No window: only the usage-limit cooldown gates the agent.
    None,
    /// No source named, for an agent that has no default one.
    Unset,
}

impl Source {
    /// Parses a `USAGE_SOURCE_<agent>` value; `codex_default` is the sessions directory a bare
    /// `codex-sessions` names.
    pub fn parse(
        value: &str,
        codex_default: impl FnOnce() -> Option<PathBuf>,
    ) -> Result<Self, String> {
        let (kind, path) = match value.split_once(':') {
            Some((kind, path)) => (kind, Some(path)),
            None => (value, None),
        };
        let path = path.filter(|path| !path.is_empty()).map(PathBuf::from);
        let need =
            |path: Option<PathBuf>| path.ok_or(format!("{kind} needs a path: {kind}:<path>"));
        match kind {
            "json" => Ok(Self::Json(need(path)?)),
            "claude-cache" => Ok(Self::ClaudeCache(need(path)?)),
            "codex-sessions" => path
                .or_else(codex_default)
                .map(Self::CodexSessions)
                .ok_or_else(|| "codex-sessions needs a directory, or HOME set".to_owned()),
            "none" if path.is_none() => Ok(Self::None),
            _ => Err(format!(
                "'{value}' is not json:<path>, claude-cache:<path>, codex-sessions[:<dir>] or none"
            )),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Json(path) => format!("json:{}", path.display()),
            Self::ClaudeCache(path) => format!("claude-cache:{}", path.display()),
            Self::CodexSessions(dir) => format!("codex-sessions:{}", dir.display()),
            Self::None => "none".to_owned(),
            Self::Unset => "unset".to_owned(),
        }
    }
}

/// One usage window: how full it is and how long it runs.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub name: String,
    pub used_pct: f64,
    pub window_minutes: u64,
    /// Shown as the source gives it. A number is Unix seconds, and the window has reset once it
    /// passes.
    pub resets_at: Option<Value>,
}

impl Window {
    pub fn resets_at_s(&self) -> Option<u64> {
        self.resets_at.as_ref().and_then(Value::as_u64)
    }
}

/// What a source read: when the figures were taken, and the windows.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    pub taken_ms: u64,
    pub windows: Vec<Window>,
    /// The figures stay true past their age until each window resets: nothing but the agent
    /// itself changes them, and it records every change where they were read.
    pub holds_until_reset: bool,
}

/// Reads the source; `None` has no reading.
pub fn read(source: &Source) -> Result<Reading, String> {
    match source {
        Source::Json(path) => parse_normalized(&read_file(path)?),
        Source::ClaudeCache(path) => parse_claude_cache(&read_file(path)?),
        Source::CodexSessions(dir) => read_codex_sessions(dir),
        Source::None | Source::Unset => Err("no source".to_owned()),
    }
}

fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if bytes.len() as u64 > FILE_LIMIT {
        return Err(format!(
            "{}: larger than {FILE_LIMIT} bytes",
            path.display()
        ));
    }
    Ok(bytes)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Normalized {
    timestamp: u64,
    windows: Vec<NormalizedWindow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NormalizedWindow {
    name: String,
    used_pct: f64,
    window_minutes: u64,
    #[serde(default)]
    resets_at: Option<Value>,
}

/// `{"timestamp": <ms>, "windows": [{"name", "used_pct", "window_minutes", "resets_at"?}]}`.
pub fn parse_normalized(bytes: &[u8]) -> Result<Reading, String> {
    let doc: Normalized =
        serde_json::from_slice(bytes).map_err(|error| format!("not a usage document: {error}"))?;
    if doc.windows.len() > MAX_WINDOWS {
        return Err(format!("more than {MAX_WINDOWS} windows"));
    }
    let mut names = HashSet::new();
    let windows = doc
        .windows
        .into_iter()
        .map(|raw| {
            if !names.insert(raw.name.clone()) {
                return Err(format!("window {} is listed twice", raw.name));
            }
            window(raw.name, raw.used_pct, raw.window_minutes, raw.resets_at)
        })
        .collect::<Result<_, _>>()?;
    Ok(Reading {
        taken_ms: doc.timestamp,
        windows,
        holds_until_reset: false,
    })
}

/// Claude Code's cache: `{"timestamp": <ms>, "data": {"fiveHourPercent": n, "weeklyPercent": n}}`,
/// with optional `fiveHourResetsAt` and `weeklyResetsAt`. Both windows must be there.
pub fn parse_claude_cache(bytes: &[u8]) -> Result<Reading, String> {
    let doc: Value =
        serde_json::from_slice(bytes).map_err(|error| format!("not a usage cache: {error}"))?;
    let taken_ms = doc
        .get("timestamp")
        .and_then(Value::as_f64)
        .filter(|ms| ms.is_finite() && *ms >= 0.0)
        .ok_or("the cache has no timestamp")?;
    let data = doc.get("data").ok_or("the cache has no data")?;
    let pct = |key: &str| {
        data.get(key)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("the cache has no {key}"))
    };
    let resets = |key: &str| data.get(key).filter(|value| value.is_string()).cloned();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let taken_ms = taken_ms as u64;
    Ok(Reading {
        taken_ms,
        windows: vec![
            window(
                "five_hour".to_owned(),
                pct("fiveHourPercent")?,
                300,
                resets("fiveHourResetsAt"),
            )?,
            window(
                "weekly".to_owned(),
                pct("weeklyPercent")?,
                10_080,
                resets("weeklyResetsAt"),
            )?,
        ],
        holds_until_reset: false,
    })
}

fn window(
    name: String,
    used_pct: f64,
    window_minutes: u64,
    resets_at: Option<Value>,
) -> Result<Window, String> {
    if name.is_empty()
        || name.len() > 32
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(format!(
            "window name {name:?} is not 1 to 32 of A-Z, a-z, 0-9 and _"
        ));
    }
    if !used_pct.is_finite() || used_pct < 0.0 {
        return Err(format!(
            "window {name}: used {used_pct} is not a percentage"
        ));
    }
    if window_minutes == 0 {
        return Err(format!("window {name}: window_minutes is 0"));
    }
    let resets_at = resets_at.filter(|value| !value.is_null());
    match &resets_at {
        None | Some(Value::String(_)) => {}
        Some(value) if value.as_u64().is_some_and(|at| at >= EPOCH_SECONDS_MIN) => {}
        Some(value) => {
            return Err(format!(
                "window {name}: resets_at {value} is not Unix seconds"
            ));
        }
    }
    Ok(Window {
        name,
        used_pct,
        window_minutes,
        resets_at,
    })
}

/// The last `rate_limits` of the newest `rollout-*.jsonl` under `dir` that has one, taken when that
/// file was last written. Codex keeps rollouts under `YYYY/MM/DD/`, named by their start, so the walk
/// goes newest first by name and stops at the first that reads, after [`ROLLOUTS_TRIED`] at most.
fn read_codex_sessions(dir: &Path) -> Result<Reading, String> {
    let mut tried = 0;
    let mut last_error = format!("{}: no rollout-*.jsonl", dir.display());
    for year in newest_first(dir, is_number)? {
        for month in newest_first(&dir.join(&year), is_number)? {
            let month = dir.join(&year).join(month);
            for day in newest_first(&month, is_number)? {
                let day = month.join(day);
                for name in newest_first(&day, is_rollout)? {
                    let path = day.join(name);
                    match rollout_windows(&path) {
                        Ok(windows) => {
                            let modified = std::fs::metadata(&path)
                                .and_then(|meta| meta.modified())
                                .map_err(|error| format!("{}: {error}", path.display()))?;
                            let taken_ms = modified.duration_since(UNIX_EPOCH).map_or(0, |since| {
                                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
                            });
                            return Ok(Reading {
                                taken_ms,
                                windows,
                                holds_until_reset: true,
                            });
                        }
                        Err(error) => last_error = error,
                    }
                    tried += 1;
                    if tried >= ROLLOUTS_TRIED {
                        return Err(last_error);
                    }
                }
            }
        }
    }
    Err(last_error)
}

fn is_number(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_rollout(name: &str) -> bool {
    name.starts_with("rollout-")
        && Path::new(name)
            .extension()
            .is_some_and(|ext| ext == "jsonl")
}

/// The names in `dir` that `keep` accepts, last first.
fn newest_first(dir: &Path, keep: fn(&str) -> bool) -> Result<Vec<String>, String> {
    let entries = std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let mut names = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= DIR_LIMIT {
            return Err(format!("{}: more than {DIR_LIMIT} entries", dir.display()));
        }
        if let Some(name) = entry
            .ok()
            .and_then(|entry| entry.file_name().into_string().ok())
            .filter(|name| keep(name))
        {
            names.push(name);
        }
    }
    names.sort_unstable_by(|a, b| b.cmp(a));
    Ok(names)
}

/// The windows of the last `rate_limits` in the tail of the rollout at `path`.
fn rollout_windows(path: &Path) -> Result<Vec<Window>, String> {
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let len = file
        .metadata()
        .map_err(|error| format!("{}: {error}", path.display()))?
        .len();
    file.seek(SeekFrom::Start(len.saturating_sub(ROLLOUT_TAIL)))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let mut tail = Vec::new();
    file.take(ROLLOUT_TAIL)
        .read_to_end(&mut tail)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    parse_rollout_tail(&tail).map_err(|error| format!("{}: {error}", path.display()))
}

/// The windows of the last line in `tail` whose `payload.rate_limits` is an object. A line cut by
/// the tail's start, or one still being written, does not parse and is passed over.
pub fn parse_rollout_tail(tail: &[u8]) -> Result<Vec<Window>, String> {
    for line in tail.split(|&byte| byte == b'\n').rev() {
        if !contains(line, b"\"rate_limits\"") {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let Some(limits) = event
            .get("payload")
            .and_then(|payload| payload.get("rate_limits"))
            .filter(|limits| limits.is_object())
        else {
            continue;
        };
        return ["primary", "secondary"]
            .into_iter()
            .filter_map(|name| {
                limits
                    .get(name)
                    .filter(|window| !window.is_null())
                    .map(|window| codex_window(name, window))
            })
            .collect();
    }
    Err("no rate_limits in the newest rollout".to_owned())
}

fn codex_window(name: &str, raw: &Value) -> Result<Window, String> {
    let used = raw
        .get("used_percent")
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("{name} has no used_percent"))?;
    let minutes = raw
        .get("window_minutes")
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{name} has no window_minutes"))?;
    window(
        name.to_owned(),
        used,
        minutes,
        raw.get("resets_at").cloned(),
    )
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|part| part == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEX_LINE: &str = r#"{"timestamp":"2026-09-19T12:35:05.002Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex","primary":{"used_percent":48.0,"window_minutes":10080,"resets_at":1790412153},"secondary":null,"credits":{"has_credits":false}}}}"#;

    #[test]
    fn sources_parse() {
        let none = || None;
        assert_eq!(
            Source::parse("json:/u/a.json", none),
            Ok(Source::Json("/u/a.json".into()))
        );
        assert_eq!(
            Source::parse("claude-cache:/u/c.json", none),
            Ok(Source::ClaudeCache("/u/c.json".into()))
        );
        assert_eq!(
            Source::parse("codex-sessions", || Some("/h/.codex/sessions".into())),
            Ok(Source::CodexSessions("/h/.codex/sessions".into()))
        );
        assert_eq!(
            Source::parse("codex-sessions:/s", none),
            Ok(Source::CodexSessions("/s".into()))
        );
        assert_eq!(Source::parse("none", none), Ok(Source::None));
        for bad in [
            "json",
            "json:",
            "claude-cache",
            "none:/x",
            "http://x",
            "",
            "codex-sessions",
        ] {
            assert!(Source::parse(bad, none).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_normalized_document_holds_any_windows() {
        let reading = parse_normalized(
            br#"{"timestamp": 1700000000000, "windows": [
                {"name": "five_hour", "used_pct": 12.5, "window_minutes": 300, "resets_at": "2026-11-28T17:00:00Z"},
                {"name": "monthly", "used_pct": 40, "window_minutes": 43200}]}"#,
        )
        .unwrap();
        assert_eq!(reading.taken_ms, 1_700_000_000_000);
        assert_eq!(reading.windows.len(), 2);
        assert!((reading.windows[0].used_pct - 12.5).abs() < f64::EPSILON);
        assert_eq!(reading.windows[1].window_minutes, 43_200);
        assert_eq!(reading.windows[1].resets_at, None);
        assert!(!reading.holds_until_reset);
        let empty = parse_normalized(br#"{"timestamp": 1, "windows": []}"#).unwrap();
        assert!(
            empty.windows.is_empty(),
            "weighed as unreadable, not refused here"
        );
    }

    #[test]
    fn a_malformed_normalized_document_is_refused() {
        for bad in [
            &b"not json"[..],
            br#"{"windows": []}"#,
            br#"{"timestamp": 1}"#,
            br#"{"timestamp": -1, "windows": []}"#,
            br#"{"timestamp": 1, "windows": [{"name": "a b", "used_pct": 1, "window_minutes": 5}]}"#,
            br#"{"timestamp": 1, "windows": [{"name": "a", "used_pct": -1, "window_minutes": 5}]}"#,
            br#"{"timestamp": 1, "windows": [{"name": "a", "used_pct": 1, "window_minutes": 0}]}"#,
            br#"{"timestamp": 1, "windows": [{"name": "a", "used_pct": "1", "window_minutes": 5}]}"#,
            br#"{"timestamp": 1, "windows": [{"name": "a", "used_pct": 1, "window_minutes": 5, "extra": 1}]}"#,
            br#"{"timestamp": 1, "windows": [{"name": "a", "used_pct": 1, "window_minutes": 5, "resets_at": 1790}]}"#,
            br#"{"timestamp": 1, "windows": [{"name": "a", "used_pct": 1, "window_minutes": 5, "resets_at": -5}]}"#,
            br#"{"timestamp": 1, "windows": [{"name": "a", "used_pct": 1, "window_minutes": 5, "resets_at": true}]}"#,
            br#"{"timestamp": 1, "windows": [{"name": "a", "used_pct": 1, "window_minutes": 5}, {"name": "a", "used_pct": 2, "window_minutes": 5}]}"#,
        ] {
            assert!(parse_normalized(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        let many: Vec<String> = (0..=MAX_WINDOWS)
            .map(|i| format!(r#"{{"name": "w{i}", "used_pct": 1, "window_minutes": 5}}"#))
            .collect();
        let doc = format!(r#"{{"timestamp": 1, "windows": [{}]}}"#, many.join(","));
        assert!(parse_normalized(doc.as_bytes()).is_err());
    }

    #[test]
    fn the_claude_cache_gives_two_windows() {
        let reading = parse_claude_cache(
            br#"{"timestamp": 1700000000000, "data": {"fiveHourPercent": 61, "weeklyPercent": 12.5, "weeklyResetsAt": "2026-12-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let names: Vec<_> = reading.windows.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["five_hour", "weekly"]);
        assert_eq!(reading.windows[0].window_minutes, 300);
        assert_eq!(reading.windows[1].window_minutes, 10_080);
        assert!((reading.windows[0].used_pct - 61.0).abs() < f64::EPSILON);
        assert_eq!(
            reading.windows[1].resets_at,
            Some(Value::from("2026-12-01T00:00:00Z"))
        );
        for bad in [
            &b"{}"[..],
            br#"{"timestamp": 1, "data": {"fiveHourPercent": 1}}"#,
            br#"{"timestamp": 1, "data": {"fiveHourPercent": 1, "weeklyPercent": null}}"#,
            br#"{"data": {"fiveHourPercent": 1, "weeklyPercent": 1}}"#,
            b"[1, 2]",
        ] {
            assert!(
                parse_claude_cache(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn a_rollout_gives_its_last_rate_limits_and_a_null_window_is_absent() {
        let older = CODEX_LINE.replace("48.0", "10.0");
        let tail = format!(
            "{older}\n{{\"type\":\"other\"}}\n{CODEX_LINE}\n{{\"partial\": \"rate_limits\""
        );
        let windows = parse_rollout_tail(tail.as_bytes()).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].name, "primary");
        assert!((windows[0].used_pct - 48.0).abs() < f64::EPSILON);
        assert_eq!(windows[0].window_minutes, 10_080);
        assert_eq!(windows[0].resets_at_s(), Some(1_790_412_153));
        let both = CODEX_LINE.replace(
            r#""secondary":null"#,
            r#""secondary":{"used_percent":5,"window_minutes":300,"resets_at":1790000000}"#,
        );
        let names: Vec<_> = parse_rollout_tail(both.as_bytes())
            .unwrap()
            .into_iter()
            .map(|w| w.name)
            .collect();
        assert_eq!(names, ["primary", "secondary"]);
    }

    #[test]
    fn a_rollout_without_usable_rate_limits_is_refused() {
        assert!(parse_rollout_tail(b"").is_err());
        assert!(parse_rollout_tail(br#"{"payload":{"rate_limits":null}}"#).is_err());
        let broken = CODEX_LINE.replace(r#""window_minutes":10080,"#, "");
        assert!(parse_rollout_tail(broken.as_bytes()).is_err());
        // Milliseconds, or a duration, are not the Unix seconds a reset is read as.
        let millis = CODEX_LINE.replace("1790412153", "1790412153000000000000");
        assert!(parse_rollout_tail(millis.as_bytes()).is_err());
        let duration = CODEX_LINE.replace("1790412153", "3600");
        assert!(parse_rollout_tail(duration.as_bytes()).is_err());
        // Both windows null: no window, which the weighing reads as no reading.
        let neither = CODEX_LINE.replace(
            r#""primary":{"used_percent":48.0,"window_minutes":10080,"resets_at":1790412153}"#,
            r#""primary":null"#,
        );
        assert_eq!(parse_rollout_tail(neither.as_bytes()), Ok(Vec::new()));
    }

    #[test]
    fn the_newest_rollout_is_read_from_its_tail() {
        let dir = std::env::temp_dir().join(format!("usage-codex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let day = dir.join("2026/09/19");
        std::fs::create_dir_all(&day).unwrap();
        // An earlier day, and an earlier session of the newest day.
        std::fs::create_dir_all(dir.join("2026/09/18")).unwrap();
        std::fs::write(
            dir.join("2026/09/18/rollout-2026-09-18T23-00-00-a.jsonl"),
            CODEX_LINE.replace("48.0", "7.0") + "\n",
        )
        .unwrap();
        let old = day.join("rollout-2026-09-19T10-00-00-b.jsonl");
        std::fs::write(&old, CODEX_LINE.replace("48.0", "99.0") + "\n").unwrap();
        // A huge rollout: the rate_limits line sits after more than the tail's worth of other lines.
        let filler = format!(
            "{{\"type\":\"response\",\"text\":\"{}\"}}\n",
            "x".repeat(1 << 16)
        );
        let newest = day.join("rollout-2026-09-19T11-00-00-c.jsonl");
        let mut big = filler.repeat(usize::try_from(ROLLOUT_TAIL >> 16).unwrap() + 4);
        big.push_str(CODEX_LINE);
        big.push('\n');
        std::fs::write(&newest, &big).unwrap();
        std::fs::write(day.join("notes.jsonl"), CODEX_LINE.replace("48.0", "1.0")).unwrap();
        let reading = read(&Source::CodexSessions(dir.clone())).unwrap();
        assert!(reading.holds_until_reset);
        assert!((reading.windows[0].used_pct - 48.0).abs() < f64::EPSILON);

        // A session that just started has no rate_limits yet: the one before it still reads.
        let started = day.join("rollout-2026-09-19T12-00-00-d.jsonl");
        std::fs::write(&started, "{\"type\":\"session_meta\"}\n").unwrap();
        let reading = read(&Source::CodexSessions(dir.clone())).unwrap();
        assert!((reading.windows[0].used_pct - 48.0).abs() < f64::EPSILON);
        std::fs::remove_file(&started).unwrap();

        // With the line pushed out of the tail, the session before reads, then the day before.
        let mut buried = CODEX_LINE.to_owned() + "\n";
        buried.push_str(&filler.repeat(usize::try_from(ROLLOUT_TAIL >> 16).unwrap() + 4));
        std::fs::write(&newest, &buried).unwrap();
        let reading = read(&Source::CodexSessions(dir.clone())).unwrap();
        assert!((reading.windows[0].used_pct - 99.0).abs() < f64::EPSILON);
        std::fs::remove_file(&old).unwrap();
        let reading = read(&Source::CodexSessions(dir.clone())).unwrap();
        assert!((reading.windows[0].used_pct - 7.0).abs() < f64::EPSILON);
        std::fs::remove_dir_all(dir.join("2026/09/18")).unwrap();
        assert!(read(&Source::CodexSessions(dir.clone())).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(read(&Source::CodexSessions(dir)).is_err());
    }

    #[test]
    fn the_walk_stops_after_the_newest_rollouts() {
        let dir = std::env::temp_dir().join(format!("usage-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let day = dir.join("2026/11/28");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(
            day.join("rollout-2026-11-28T00-00-00-a.jsonl"),
            CODEX_LINE.to_owned() + "\n",
        )
        .unwrap();
        // More sessions without a reading than the walk tries: the one under them is never read.
        for i in 0..ROLLOUTS_TRIED {
            std::fs::write(
                day.join(format!("rollout-2026-11-28T01-00-{i:02}-x.jsonl")),
                "{}\n",
            )
            .unwrap();
        }
        let result = read(&Source::CodexSessions(dir.clone()));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn a_huge_file_is_not_a_reading() {
        let path = std::env::temp_dir().join(format!("usage-huge-{}.json", std::process::id()));
        let huge = format!(
            r#"{{"timestamp": 1, "windows": [], "pad": "{}"}}"#,
            "x".repeat(usize::try_from(FILE_LIMIT).unwrap())
        );
        std::fs::write(&path, huge).unwrap();
        let result = read(&Source::Json(path.clone()));
        std::fs::remove_file(&path).unwrap();
        assert!(result.unwrap_err().contains("larger than"));
    }
}
