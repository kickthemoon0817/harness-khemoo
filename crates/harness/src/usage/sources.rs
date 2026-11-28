//! Where an agent's usage reading comes from, each parsed into the same windows.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

/// A cache or normalized file larger than this is not a usage reading.
pub const FILE_LIMIT: u64 = 1 << 20;
/// How much of the newest rollout is read, from its end: its last `rate_limits` lies near it, and a
/// session's rollout grows past hundreds of megabytes.
pub const ROLLOUT_TAIL: u64 = 4 << 20;
/// How many entries the walk of the sessions directory reads before it gives up.
const WALK_LIMIT: usize = 100_000;
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
        Source::None => Err("no source".to_owned()),
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
    Ok(Window {
        name,
        used_pct,
        window_minutes,
        resets_at,
    })
}

/// The newest `rate_limits` in the newest `rollout-*.jsonl` under `dir`, taken when that file was
/// last written.
fn read_codex_sessions(dir: &Path) -> Result<Reading, String> {
    let (path, modified) = newest_rollout(dir)?;
    let mut file = File::open(&path).map_err(|error| format!("{}: {error}", path.display()))?;
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
    let windows =
        parse_rollout_tail(&tail).map_err(|error| format!("{}: {error}", path.display()))?;
    let taken_ms = modified.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    });
    Ok(Reading {
        taken_ms,
        windows,
        holds_until_reset: true,
    })
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
    let resets_at = raw.get("resets_at").filter(|value| value.is_u64()).cloned();
    window(name.to_owned(), used, minutes, resets_at)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|part| part == needle)
}

/// The most recently written `rollout-*.jsonl` at most four directories under `dir` (codex keeps
/// them under `YYYY/MM/DD/`).
fn newest_rollout(dir: &Path) -> Result<(PathBuf, SystemTime), String> {
    let mut newest: Option<(PathBuf, SystemTime)> = None;
    let mut pending = vec![(dir.to_path_buf(), 0)];
    let mut seen = 0;
    while let Some((current, depth)) = pending.pop() {
        let entries = std::fs::read_dir(&current)
            .map_err(|error| format!("{}: {error}", current.display()))?;
        for entry in entries.flatten() {
            seen += 1;
            if seen > WALK_LIMIT {
                return Err(format!("{}: more than {WALK_LIMIT} entries", dir.display()));
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() && depth < 4 {
                pending.push((path, depth + 1));
            } else if kind.is_file()
                && entry.file_name().to_str().is_some_and(|name| {
                    name.starts_with("rollout-")
                        && Path::new(name)
                            .extension()
                            .is_some_and(|ext| ext == "jsonl")
                })
                && let Ok(modified) = entry.metadata().and_then(|meta| meta.modified())
                && newest.as_ref().is_none_or(|(_, at)| modified > *at)
            {
                newest = Some((path, modified));
            }
        }
    }
    newest.ok_or_else(|| format!("{}: no rollout-*.jsonl", dir.display()))
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
        assert!(empty.windows.is_empty());
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
    }

    #[test]
    fn the_newest_rollout_is_read_from_its_tail() {
        let dir = std::env::temp_dir().join(format!("usage-codex-{}", std::process::id()));
        let day = dir.join("2026/09/19");
        std::fs::create_dir_all(&day).unwrap();
        let old = day.join("rollout-old.jsonl");
        std::fs::write(&old, CODEX_LINE.replace("48.0", "99.0") + "\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // A huge rollout: the rate_limits line sits after more than the tail's worth of other lines.
        let filler = format!(
            "{{\"type\":\"response\",\"text\":\"{}\"}}\n",
            "x".repeat(1 << 16)
        );
        let mut big = filler.repeat(usize::try_from(ROLLOUT_TAIL >> 16).unwrap() + 4);
        big.push_str(CODEX_LINE);
        big.push('\n');
        std::fs::write(day.join("rollout-new.jsonl"), &big).unwrap();
        std::fs::write(day.join("notes.jsonl"), CODEX_LINE.replace("48.0", "1.0")).unwrap();
        let reading = read(&Source::CodexSessions(dir.clone())).unwrap();
        assert!(reading.holds_until_reset);
        assert!((reading.windows[0].used_pct - 48.0).abs() < f64::EPSILON);

        // With the line pushed out of the tail, there is no reading.
        let mut buried = CODEX_LINE.to_owned() + "\n";
        buried.push_str(&filler.repeat(usize::try_from(ROLLOUT_TAIL >> 16).unwrap() + 4));
        std::fs::write(day.join("rollout-new.jsonl"), &buried).unwrap();
        assert!(read(&Source::CodexSessions(dir.clone())).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(read(&Source::CodexSessions(dir)).is_err());
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
