//! Human-readable tables. They are not a stable interface; scripts use `--json`.
//!
//! Every string a device reported is untrusted: [`clean`] replaces its control characters,
//! bidi controls and invisible characters before it reaches a terminal, so a device cannot
//! move the cursor, retitle the window, or make text read differently from what it is.

use std::fmt::Write as _;

use fleet_proto::SessionState;
use fleet_proto::output::{SessionsOutput, StatusOutput, TransportKind};

/// Bidi controls and invisible characters: they can make text read in another order than it
/// is (a "Trojan Source" spoof) or hide a difference between two names. Tag characters can
/// also carry hidden text.
fn hides_text(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'
            | '\u{200C}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{3164}'
            | '\u{FEFF}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// The zero-width joiner and the variation selectors: invisible, but emoji need them, and they
/// cannot reorder or hide text on their own.
fn joins_emoji(c: char) -> bool {
    matches!(c, '\u{200D}' | '\u{FE00}'..='\u{FE0F}')
}

/// A device's name, label, workdir or error for a terminal: every control character and
/// invisible character becomes U+FFFD, since such a string has no reason to hold one.
pub fn clean(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || hides_text(c) || joins_emoji(c) {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

/// An agent's output for a terminal: [`clean`], but keeping tab, LF and CR so the text
/// keeps its layout, and the joiner and variation selectors so emoji survive.
pub fn clean_output(s: &str) -> String {
    s.chars()
        .map(|c| {
            if (c.is_control() && !matches!(c, '\t' | '\n' | '\r')) || hides_text(c) {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

pub fn transport(t: TransportKind) -> &'static str {
    match t {
        TransportKind::Local => "local",
        TransportKind::Ssh => "ssh",
        TransportKind::Tls => "tls",
    }
}

pub fn state(s: &SessionState) -> String {
    match s {
        SessionState::Running {} => "running".into(),
        SessionState::Exited { code } => format!("exited {code}"),
        SessionState::Signalled { signal } => format!("signalled {signal}"),
        SessionState::Cancelled {} => "cancelled".into(),
        SessionState::TimedOut {} => "timed_out".into(),
        SessionState::Failed { .. } => "failed".into(),
    }
}

#[allow(clippy::cast_precision_loss)] // display only
fn bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n}B")
    } else {
        format!("{v:.1}{}", UNITS[unit])
    }
}

fn age(now: u64, since: u64) -> String {
    let s = now.saturating_sub(since);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// Lays out rows in columns two spaces apart; the first row is the header.
fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.first().map_or(0, Vec::len);
    let widths: Vec<usize> = (0..cols)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (c, cell) in row.iter().enumerate() {
            if c + 1 == cols {
                line.push_str(cell);
            } else {
                let pad = widths[c] - cell.chars().count() + 2;
                let _ = write!(line, "{cell}{:pad$}", "");
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

pub fn status_table(doc: &StatusOutput) -> String {
    let mut rows = vec![
        [
            "DEVICE",
            "TRANSPORT",
            "STATE",
            "CPUS",
            "LOAD",
            "CPU",
            "MEMORY",
            "GPUS",
            "SESSIONS",
            "ADAPTERS",
        ]
        .map(String::from)
        .to_vec(),
    ];
    for d in &doc.devices {
        let mut row = vec![d.name.to_string(), transport(d.transport).into()];
        match (&d.status, &d.error) {
            (Some(s), _) => {
                let m = &s.metrics;
                let running = d.sessions.iter().filter(|s| s.state.is_running()).count();
                let adapters = s
                    .adapters
                    .iter()
                    .map(|a| {
                        if a.max_concurrent == 0 {
                            format!("{} -", a.name)
                        } else {
                            format!("{} {}/{}", a.name, a.running, a.max_concurrent)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                row.extend([
                    "ok".into(),
                    s.device.cpus.to_string(),
                    format!("{:.2}", m.load[0]),
                    format!("{:.0}%", m.cpu_percent),
                    format!(
                        "{}/{}",
                        bytes(m.memory.used_bytes),
                        bytes(m.memory.total_bytes)
                    ),
                    m.gpus.len().to_string(),
                    format!("{running}/{}", d.sessions.len()),
                    adapters,
                ]);
            }
            (None, Some(e)) => {
                row.push(e.code.to_string());
                row.extend(std::iter::repeat_n("-".to_owned(), 6));
                row.push(clean(&e.message));
            }
            (None, None) => row.extend(std::iter::repeat_n("-".to_owned(), 8)),
        }
        rows.push(row);
    }
    table(&rows)
}

pub fn sessions_table(doc: &SessionsOutput) -> String {
    let mut rows = vec![
        [
            "DEVICE", "SESSION", "ADAPTER", "ORIGIN", "STATE", "PID", "AGE", "MODEL", "ISSUE",
            "CWD",
        ]
        .map(String::from)
        .to_vec(),
    ];
    for ds in &doc.sessions {
        let s = &ds.session;
        rows.push(vec![
            ds.device.to_string(),
            s.id.to_string(),
            s.adapter.to_string(),
            match s.origin {
                fleet_proto::Origin::Dispatched => "dispatched".into(),
                fleet_proto::Origin::Discovered => "discovered".into(),
            },
            state(&s.state),
            s.pid.map_or_else(|| "-".into(), |p| p.to_string()),
            age(doc.generated_at, s.started_at),
            s.model
                .as_ref()
                .map_or_else(|| "-".into(), ToString::to_string),
            s.labels.get("issue").map_or_else(|| "-".into(), clean),
            s.cwd.as_deref().map_or_else(|| "-".into(), clean),
        ]);
    }
    table(&rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_replaces_terminal_escapes() {
        assert_eq!(
            clean("a\x1b]0;x\x07b\u{9b}c"),
            "a\u{FFFD}]0;x\u{FFFD}b\u{FFFD}c"
        );
    }

    #[test]
    fn clean_output_keeps_layout_and_drops_escapes() {
        assert_eq!(
            clean_output("ok\tdone\r\n\x1b[2J\x7f\u{85}\u{9b}31m\x00é"),
            "ok\tdone\r\n\u{FFFD}[2J\u{FFFD}\u{FFFD}\u{FFFD}31m\u{FFFD}é"
        );
    }

    #[test]
    fn bidi_and_invisible_characters_are_replaced() {
        let hidden = [
            '\u{202A}',
            '\u{202B}',
            '\u{202C}',
            '\u{202D}',
            '\u{202E}',
            '\u{2066}',
            '\u{2067}',
            '\u{2068}',
            '\u{2069}',
            '\u{200E}',
            '\u{200F}',
            '\u{061C}',
            '\u{200B}',
            '\u{200C}',
            '\u{2060}',
            '\u{2061}',
            '\u{2064}',
            '\u{FEFF}',
            '\u{00AD}',
            '\u{180E}',
            '\u{3164}',
            '\u{E0000}',
            '\u{E0041}',
            '\u{E007F}',
        ];
        for c in hidden {
            let s = format!("a{c}b");
            assert_eq!(clean(&s), "a\u{FFFD}b", "{:04X}", u32::from(c));
            assert_eq!(clean_output(&s), "a\u{FFFD}b", "{:04X}", u32::from(c));
        }
        for c in ['\u{200D}', '\u{FE00}', '\u{FE0F}'] {
            assert_eq!(
                clean(&format!("a{c}b")),
                "a\u{FFFD}b",
                "{:04X}",
                u32::from(c)
            );
        }
        assert_eq!(clean("é\u{2010}"), "é\u{2010}");
        assert_eq!(clean_output("x\ty\n"), "x\ty\n");
    }

    #[test]
    fn agent_output_keeps_emoji_sequences() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let heart = "\u{2764}\u{FE0F}";
        let text = format!("{family} {heart}");
        assert_eq!(clean_output(&text), text);
    }

    #[test]
    fn sizes_and_ages() {
        assert_eq!(bytes(512), "512B");
        assert_eq!(bytes(8 * 1024 * 1024 * 1024), "8.0G");
        assert_eq!(age(100, 40), "1m");
        assert_eq!(age(100, 200), "0s");
        assert_eq!(age(90_000, 0), "1d");
    }

    #[test]
    fn columns_align() {
        let t = table(&[
            vec!["A".into(), "B".into()],
            vec!["long".into(), "x".into()],
        ]);
        assert_eq!(t, "A     B\nlong  x\n");
    }
}
