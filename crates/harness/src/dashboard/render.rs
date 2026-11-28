//! Renders a snapshot as the dashboard page and its independently refreshable sections.
//!
//! Values from the snapshot are HTML-escaped; status always pairs a shape and a word with its
//! colour.

// The page is many small fragments; collecting each formatted piece reads clearest.
#![allow(clippy::format_collect)]

use std::fmt::Write as _;

use serde_json::Value;

use super::collector::{entry_label, wall_clock};
use super::snapshot::SOURCE_ORDER;
use super::text::{clip, float_g, float_repr, optional_int, parse_iso, sha256_hex, utc};

const SPARK_WIDTH: f64 = 240.0;
const SPARK_HEIGHT: f64 = 34.0;
const SPARK_HIT_BUCKETS: usize = 48;
const STYLE: &str = concat!("\n", include_str!("dashboard.css"));
const QUEUE_TITLES: [(&str, &str); 5] = [
    ("running", "Running"),
    ("claimable", "Claimable"),
    ("operator", "Waiting on the operator"),
    ("signoff", "Waiting for sign-off"),
    ("other", "Everything else"),
];

fn status_level(status: &str) -> Option<&'static str> {
    Some(match status {
        "ok" | "alive" | "running" => "good",
        "held" => "info",
        "free" | "unknown" | "paused" => "idle",
        "orphan" | "down" => "critical",
        "slow" => "warning",
        "demoted" => "serious",
        _ => return None,
    })
}

/// A value as text: `None`, `True` and `False`, a whole float with `.0`.
fn text_of(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => match (number.as_i64(), number.as_u64(), number.as_f64()) {
            (Some(integer), _, _) => integer.to_string(),
            (_, Some(integer), _) => integer.to_string(),
            (_, _, Some(float)) => float_repr(float),
            _ => number.to_string(),
        },
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// `value`, or `default` when the value is null, false, zero or empty.
fn or<'v>(value: &'v Value, default: &'v Value) -> &'v Value {
    if truthy(value) { value } else { default }
}

/// HTML-escaped text for element and attribute content.
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#x27;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// A value as text, escaped for the page.
fn t(value: &Value) -> String {
    escape(&text_of(value))
}

/// A snapshot value, escaped; none reads as an empty string.
fn e(value: &Value) -> String {
    if value.is_null() {
        String::new()
    } else {
        escape(&text_of(value))
    }
}

fn s(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn list(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

fn pill(level: &str, text: &str) -> String {
    format!(
        r#"<span class="pill {}"><i></i>{}</span>"#,
        escape(status_level(level).unwrap_or(level)),
        escape(text)
    )
}

fn issue_link(number: &Value, url: &Value) -> String {
    if number.is_null() {
        return String::new();
    }
    // Only an https URL becomes a link: a `javascript:` or `data:` one must not run on a click.
    if url.as_str().is_some_and(|url| url.starts_with("https://")) {
        return format!(
            r#"<a href="{}" target="_blank" rel="noopener noreferrer">#{}</a>"#,
            e(url),
            e(number)
        );
    }
    format!("#{}", e(number))
}

fn issue_ref(issue: &Value, holder: &Value) -> String {
    if issue.is_null() {
        e(or(holder, &Value::String(String::new()))).clone()
    } else {
        format!("#{}", e(issue))
    }
}

/// `45s`, `12m`, `3h05m` or `2d04h`; `-` when unknown.
fn duration(seconds: &Value) -> String {
    let Some(seconds) = seconds
        .as_f64()
        .or_else(|| seconds.as_str().and_then(|text| text.parse().ok()))
    else {
        return "-".to_owned();
    };
    #[allow(clippy::cast_possible_truncation)]
    let total = (seconds as i64).max(0);
    if total < 60 {
        format!("{total}s")
    } else if total < 3600 {
        format!("{}m", total / 60)
    } else if total < 86400 {
        format!("{}h{:02}m", total / 3600, (total % 3600) / 60)
    } else {
        format!("{}d{:02}h", total / 86400, (total % 86400) / 3600)
    }
}

/// `HH:MMZ`, prefixed with `MM-DD` when not today (UTC).
fn clock_epoch(epoch: f64, seconds: bool) -> String {
    let moment = utc(epoch);
    let today = moment.date() == utc(wall_clock()).date();
    let clock = if seconds {
        format!(
            "{:02}:{:02}:{:02}Z",
            moment.hour(),
            moment.minute(),
            moment.second()
        )
    } else {
        format!("{:02}:{:02}Z", moment.hour(), moment.minute())
    };
    if today {
        clock
    } else {
        format!(
            "{:02}-{:02} {clock}",
            u8::from(moment.month()),
            moment.day()
        )
    }
}

/// The clock of an ISO time, or `-`.
fn clock(iso_text: &Value, seconds: bool) -> String {
    let epoch = iso_text
        .as_str()
        .filter(|text| !text.is_empty())
        .and_then(parse_iso)
        .unwrap_or(0.0);
    if epoch == 0.0 {
        "-".to_owned()
    } else {
        clock_epoch(epoch, seconds)
    }
}

fn number(value: &Value, decimals: usize) -> String {
    value
        .as_f64()
        .map_or_else(|| "-".to_owned(), |value| format!("{value:.decimals$}"))
}

fn number_of(value: f64, decimals: usize) -> String {
    format!("{value:.decimals$}")
}

fn percent(value: &Value) -> String {
    value
        .as_f64()
        .map_or_else(|| "-".to_owned(), |value| format!("{value:.0} %"))
}

fn gb(megabytes: &Value) -> String {
    megabytes
        .as_f64()
        .map_or_else(|| "-".to_owned(), |value| format!("{:.1}", value / 1024.0))
}

fn bytes_gb(count: &Value) -> String {
    count.as_f64().map_or_else(
        || "-".to_owned(),
        |value| format!("{:.1}", value / 1_073_741_824.0),
    )
}

fn short_host(host: &str) -> &str {
    host.split_once('@').map_or(host, |(_, name)| name)
}

/// A timestamp the client can display as an age.
fn sample_time(stamp: &Value) -> String {
    if !truthy(stamp) {
        return "-".to_owned();
    }
    format!(
        r#"<time datetime="{0}" title="{0}">{1}</time>"#,
        e(stamp),
        clock(stamp, true)
    )
}

fn query_of(access_token: &str) -> String {
    if access_token.is_empty() {
        return String::new();
    }
    let mut query = "?t=".to_owned();
    for byte in access_token.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~".contains(&byte) {
            query.push(char::from(byte));
        } else {
            write!(query, "%{byte:02X}").ok();
        }
    }
    query
}

/// The units a browser can update independently: section names and escaped HTML in display
/// order.
pub fn sections(snapshot: &Value, access_token: &str) -> Vec<(String, String)> {
    let query = query_of(access_token);
    let mut sections = vec![("header".to_owned(), header(snapshot))];
    for machine in list(&snapshot["machines"]) {
        let key = sha256_hex(s(&machine["key"]).as_bytes());
        sections.push((format!("machine-{key}"), machine_card(machine, snapshot)));
    }
    sections.extend([
        ("ticks".to_owned(), ticks_panel(snapshot)),
        ("leases".to_owned(), leases_panel(snapshot)),
        ("fleet_devices".to_owned(), fleet_devices_panel(snapshot)),
        ("fleet_sessions".to_owned(), fleet_sessions_panel(snapshot)),
        ("queue".to_owned(), queue_section(snapshot)),
        ("landings".to_owned(), landings_panel(snapshot)),
        ("stalls".to_owned(), stalls_panel(snapshot)),
        ("allocator".to_owned(), allocator_panel(snapshot)),
        ("supervisor".to_owned(), supervisor_panel(snapshot)),
        ("flow".to_owned(), flow_section(snapshot)),
        ("sources".to_owned(), sources_footer(snapshot, &query)),
    ]);
    sections
}

/// The whole page.
#[cfg(test)]
pub fn render(snapshot: &Value, refresh_s: u64, access_token: &str) -> String {
    document(
        &sections(snapshot, access_token),
        &[],
        refresh_s,
        access_token,
        s(&snapshot["title"]),
    )
}

/// Wraps the prepared sections in the initial document. The ticks take the whole row when the
/// instance has no resource panel; the fleet panels are left out with their key.
pub fn document(
    sections: &[(String, String)],
    versions: &[(String, String)],
    refresh_s: u64,
    access_token: &str,
    title: &str,
) -> String {
    let query = query_of(access_token);
    let slot = |name: &str| -> String {
        let body = sections
            .iter()
            .find(|(section, _)| section == name)
            .map_or("", |(_, body)| body.as_str());
        let version = versions
            .iter()
            .find(|(section, _)| section == name)
            .map_or("", |(_, version)| version.as_str());
        format!(r#"<div data-section="{name}" data-version="{version}">{body}</div>"#)
    };
    let machines: String = sections
        .iter()
        .filter(|(name, _)| name.starts_with("machine-"))
        .map(|(name, _)| slot(name))
        .collect();
    let leases_present = sections
        .iter()
        .any(|(name, body)| name == "leases" && !body.is_empty());
    format!(
        concat!(
            r#"<!doctype html><html lang="en"><head><meta charset="utf-8">"#,
            r#"<meta name="viewport" content="width=device-width,initial-scale=1">"#,
            r#"<noscript><meta http-equiv="refresh" content="{refresh};url=/{query}"></noscript>"#,
            r#"<meta name="referrer" content="no-referrer"><link rel="icon" href="data:,">"#,
            "<title>{title}</title>",
            r#"<style>{style}</style><script defer src="/dashboard.js{query}"></script></head>"#,
            r#"<body data-refresh="{refresh}">{header}<main>"#,
            r#"<section class="grid machines">{machines}</section>"#,
            r#"<section class="grid{split}">{ticks}{leases}"#,
            "</section>{fleet_devices}{fleet_sessions}{queue}",
            r#"<section class="grid thirds">{landings}{stalls}{allocator}</section>"#,
            "{supervisor}{flow}{sources}</main></body></html>"
        ),
        refresh = refresh_s,
        query = query,
        title = escape(if title.is_empty() {
            "Harness status"
        } else {
            title
        }),
        style = STYLE,
        header = slot("header"),
        machines = machines,
        split = if leases_present { " split" } else { "" },
        ticks = slot("ticks"),
        leases = slot("leases"),
        fleet_devices = slot("fleet_devices"),
        fleet_sessions = slot("fleet_sessions"),
        queue = slot("queue"),
        landings = slot("landings"),
        stalls = slot("stalls"),
        allocator = slot("allocator"),
        supervisor = slot("supervisor"),
        flow = slot("flow"),
        sources = slot("sources"),
    )
}

/// The top bar: harness state, tick width, work waiting, usage windows and models.
fn header(snapshot: &Value) -> String {
    let harness = &snapshot["harness"];
    let state_pill = if truthy(&harness["paused"]) {
        pill(
            "paused",
            &format!("paused: {}", s(&harness["paused_reason"])),
        )
    } else {
        pill("running", "running")
    };
    let mut width_text = format!(
        "{} ticks live",
        t(or(&harness["live_ticks"], &Value::from(0)))
    );
    if !harness["last_allowed"].is_null() {
        write!(
            width_text,
            " &middot; allocator allowed {} at {}",
            t(&harness["last_allowed"]),
            clock(&harness["last_allowed_at"], false)
        )
        .ok();
    }
    let from_failing_read = if truthy(&harness["waiting_count_from_failing_read"]) {
        " at the last good read"
    } else {
        ""
    };
    let waiting = t(&harness["waiting_count"]);
    let waiting_pill = if truthy(&harness["work_waiting"]) {
        pill(
            "warning",
            &format!("work waiting: {waiting} claimable{from_failing_read}, slots idle"),
        )
    } else if truthy(&harness["waiting_count"]) {
        pill("info", &format!("{waiting} claimable{from_failing_read}"))
    } else {
        String::new()
    };
    let models: Vec<String> = list(&harness["tick_models"])
        .iter()
        .map(|entry| entry_label(s(entry)))
        .collect();
    let cooling: String = list(&harness["cooling_models"])
        .iter()
        .map(|model| {
            pill(
                "warning",
                &format!(
                    "{} cooling {}",
                    e(&model["label"]),
                    duration(&model["remaining_s"])
                ),
            )
        })
        .collect();
    let lane_label = s(&harness["labels"]["lane"]);
    let lane = if truthy(&harness["lane_model"]) && !lane_label.is_empty() {
        format!(
            r#"<span class="flag sub">lane: {} for {}</span>"#,
            escape(&entry_label(s(&harness["lane_model"]))),
            escape(lane_label)
        )
    } else {
        String::new()
    };
    let title = s(&snapshot["title"]);
    let repo = s(&snapshot["repo"]);
    let models_text = if models.is_empty() {
        "default".to_owned()
    } else {
        models.join(" > ")
    };
    format!(
        concat!(
            "<header><h1>{title}</h1>",
            r#"<span class="sub">{repo} &middot; {home}</span>"#,
            r#"<span class="flag">{state}{waiting}<span class="sub num">{width}</span></span>"#,
            "{usage}",
            r#"<span class="flag sub">models: {models}{cooling}</span>{lane}"#,
            r#"<span class="sub num" style="margin-left:auto">updated {updated}</span></header>"#
        ),
        title = escape(if title.is_empty() {
            "Harness status"
        } else {
            title
        }),
        repo = escape(if repo.is_empty() { "no repo" } else { repo }),
        home = e(&snapshot["harness_home"]),
        state = state_pill,
        waiting = waiting_pill,
        width = width_text,
        usage = usage_block(&snapshot["usage"]),
        models = escape(&models_text),
        cooling = cooling,
        lane = lane,
        updated = clock(&snapshot["generated_at"], true),
    )
}

/// A window's reset: Unix seconds or an ISO time, as the source gives it.
fn reset_text(resets: &Value) -> String {
    match resets {
        #[allow(clippy::cast_precision_loss)]
        Value::Number(seconds) => seconds.as_u64().map_or_else(
            || "-".to_owned(),
            |seconds| clock_epoch(seconds as f64, false),
        ),
        Value::String(_) => clock(resets, false),
        _ => "-".to_owned(),
    }
}

/// How fresh an agent's reading is, as a pill.
fn freshness_pill(agent: &Value) -> String {
    let reading = &agent["reading"];
    let age = duration(&agent["age_s"]);
    match s(&reading["state"]) {
        "fresh" => format!(r#"<span class="sub">read {age} ago</span>"#),
        "holding" => pill("info", &format!("holding, read {age} ago")),
        "stale" => pill("warning", &format!("stale {age}")),
        "unmetered" => pill("idle", "unmetered"),
        "unreadable" => pill(
            "critical",
            &format!("unreadable: {}", s(&reading["detail"])),
        ),
        other => pill("idle", other),
    }
}

/// Each agent's usage windows as meters marked at their steps, the ticks each allows, and the
/// pool's cap.
fn usage_block(usage: &Value) -> String {
    if !truthy(&usage["available"]) {
        return format!(
            r#"<span class="pill warning"><i></i>usage unknown: {}</span>"#,
            e(&usage["error"])
        );
    }
    let mut rows = String::new();
    for agent in list(&usage["agents"]) {
        let name = e(&agent["agent"]);
        let cooling = if truthy(&agent["cooling"]) {
            format!(" {}", pill("warning", "every entry cooling"))
        } else {
            String::new()
        };
        let windows = list(&agent["windows"]);
        if windows.is_empty() {
            write!(
                rows,
                r#"<span class="sub">{name}</span><span class="meter"><b class="none"></b></span><span class="num">cap {} &middot; {}{cooling}</span>"#,
                t(&agent["cap"]),
                freshness_pill(agent)
            )
            .ok();
            continue;
        }
        for (index, window) in windows.iter().enumerate() {
            let steps: Vec<f64> = list(&window["steps"])
                .iter()
                .filter_map(Value::as_f64)
                .collect();
            let used = window["used_pct"].as_f64().unwrap_or(0.0);
            let severity = match steps.as_slice() {
                [_, _, none] if used >= *none => "critical",
                [_, one, _] if used >= *one => "serious",
                [half, _, _] if used >= *half => "warning",
                _ => "",
            };
            let ticks: String = steps
                .iter()
                .map(|step| format!(r#"<span class="tick" style="left:{step:.0}%"></span>"#))
                .collect();
            let step_text: Vec<String> = list(&window["steps"]).iter().map(t).collect();
            let resets = if window["resets_at"].is_null() {
                String::new()
            } else {
                format!(" &middot; resets {}", reset_text(&window["resets_at"]))
            };
            let tail = if index == 0 {
                format!(" &middot; {}{cooling}", freshness_pill(agent))
            } else {
                String::new()
            };
            write!(
                rows,
                concat!(
                    r#"<span class="sub">{name} {window}</span><span class="meter" title="steps {steps} %">"#,
                    r#"<b class="{severity}" style="width:{width:.0}%"></b>{ticks}</span>"#,
                    r#"<span class="num">{percent} &middot; cap {cap}{resets}{tail}</span>"#
                ),
                name = name,
                window = e(&window["name"]),
                steps = step_text.join(", "),
                severity = severity,
                width = used.clamp(0.0, 100.0),
                ticks = ticks,
                percent = percent(&window["used_pct"]),
                cap = t(&window["cap"]),
                resets = resets,
                tail = tail,
            )
            .ok();
        }
    }
    let pool = pill(
        if usage["pool_cap"].as_u64().is_some_and(|cap| cap > 0) {
            "info"
        } else {
            "warning"
        },
        // The launcher leaves out a lane-only agent while no lane tick is wanted, which this page
        // cannot tell, so its cap is shown as the bound it is.
        &format!(
            "pool at most {} of {}, room for {}",
            text_of(&usage["pool_cap"]),
            text_of(&usage["width"]),
            text_of(&usage["room"])
        ),
    );
    format!(r#"<span class="usage">{rows}</span>{pool}"#)
}

fn meter_row(
    label: &str,
    value: &Value,
    total: &Value,
    text: &str,
    severity_at: (f64, f64, f64),
) -> String {
    let fraction = match (value.as_f64(), total.as_f64().filter(|total| *total != 0.0)) {
        (Some(value), Some(total)) => Some(value / total),
        _ => None,
    };
    let bar = match fraction {
        Some(fraction) => {
            let severity = if fraction >= severity_at.2 {
                "critical"
            } else if fraction >= severity_at.1 {
                "serious"
            } else if fraction >= severity_at.0 {
                "warning"
            } else {
                ""
            };
            format!(
                r#"<b class="{severity}" style="width:{:.1}%"></b>"#,
                (fraction * 100.0).clamp(0.0, 100.0)
            )
        }
        None => r#"<b class="none"></b>"#.to_owned(),
    };
    format!(r#"<span class="k">{label}</span><span class="meter">{bar}</span><em>{text}</em>"#)
}

/// One machine: status, meters, sparklines, slots, device processes and containers; the device
/// rows only when the machine reports a device, and the slots only when the instance reports
/// resources.
#[allow(clippy::too_many_lines)]
fn machine_card(machine: &Value, snapshot: &Value) -> String {
    let status = s(&machine["status"]);
    let mut pills = vec![pill(
        status_level(status).unwrap_or("idle"),
        if machine["status"].is_null() {
            "unknown"
        } else {
            status
        },
    )];
    for signal in list(&machine["signals"]) {
        pills.push(pill(s(&signal["level"]), s(&signal["text"])));
    }
    let empty = Value::Object(serde_json::Map::new());
    let gpu = if truthy(&machine["gpu"]) {
        &machine["gpu"]
    } else {
        &empty
    };
    let history = &machine["history"];
    let has_device = truthy(gpu) || truthy(&history["gpu_util"]);
    let mut facts = Vec::new();
    if truthy(&gpu["name"]) {
        facts.push(e(&gpu["name"]));
    }
    if truthy(&machine["preferred_rank"]) {
        facts.push(format!(
            "harness preference #{}",
            t(&machine["preferred_rank"])
        ));
    }
    if truthy(&machine["card_note"]) {
        facts.push(e(&machine["card_note"]));
    }
    if truthy(&machine["slot_count"]) {
        facts.push(format!("{} slots", t(&machine["slot_count"])));
    }
    let load1 = &machine["load1"];
    let nproc = &machine["nproc"];
    let cores = if truthy(nproc) {
        t(nproc)
    } else {
        "?".to_owned()
    };
    let mut meters = meter_row(
        "CPU",
        load1,
        nproc,
        &format!("{} / {cores} cores", number(load1, 1)),
        (0.85, 1.0, 1.5),
    );
    meters.push_str(&meter_row(
        "Mem",
        &machine["mem_used_mb"],
        &machine["mem_total_mb"],
        &format!(
            "{} / {} GB",
            gb(&machine["mem_used_mb"]),
            gb(&machine["mem_total_mb"])
        ),
        (0.85, 0.92, 0.97),
    ));
    if has_device {
        meters.push_str(&meter_row(
            "Device",
            &gpu["util_pct"],
            &Value::from(100.0),
            &percent(&gpu["util_pct"]),
            (2.0, 2.0, 2.0),
        ));
        meters.push_str(&meter_row(
            "Dev mem",
            &gpu["mem_used_mb"],
            &gpu["mem_total_mb"],
            &format!(
                "{} / {} GB",
                gb(&gpu["mem_used_mb"]),
                gb(&gpu["mem_total_mb"])
            ),
            (0.85, 0.93, 0.98),
        ));
    }
    let link = if machine["kind"] == "remote" {
        let upload = if truthy(&machine["upload_timed_out"]) {
            "timeout".to_owned()
        } else {
            format!("{} MB/s", number(&machine["upload_mb_s"], 1))
        };
        format!(
            r#"<div class="kv"><span>upload <b>{upload}</b></span><span>connect <b>{} s</b></span><span>probed {}</span></div>"#,
            number(&machine["connect_s"], 2),
            sample_time(&machine["probed_at"])
        )
    } else {
        String::new()
    };
    let now = snapshot["generated_epoch"].as_f64().unwrap_or(0.0);
    let hours = window_hours(snapshot);
    // Time passing by less than one chart pixel need not shift every SVG coordinate.
    let pixel_s = (hours * 3600.0 / SPARK_WIDTH).max(1.0);
    let mut right_edge = now - now.rem_euclid(pixel_s);
    for series in ["gpu_util", "load_per_core"] {
        if let Some(last) = list(&history[series]).last() {
            right_edge = right_edge.max(last[0].as_f64().unwrap_or(0.0));
        }
    }
    if let Some(last) = list(&history["down"]).last() {
        right_edge = right_edge.max(last.as_f64().unwrap_or(0.0));
    }
    let window_text = format!("{} h", float_g(hours));
    let down: Vec<f64> = list(&history["down"])
        .iter()
        .filter_map(Value::as_f64)
        .collect();
    let mut sparks = String::new();
    if has_device {
        sparks.push_str(&spark_row(
            "Device util",
            &window_text,
            &points(&history["gpu_util"]),
            right_edge,
            hours,
            Some(100.0),
            None,
            "%",
            &down,
        ));
    }
    sparks.push_str(&spark_row(
        "Load / core",
        &window_text,
        &points(&history["load_per_core"]),
        right_edge,
        hours,
        None,
        Some(1.0),
        "",
        &down,
    ));
    let mut slot_chips: String = list(&machine["slots"])
        .iter()
        .map(|slot| {
            let tags: Vec<String> = list(&slot["tags"]).iter().map(text_of).collect();
            format!(
                r#"<span class="chip strong">slot {} {}{}{}</span>"#,
                t(&slot["slot"]),
                issue_ref(&slot["issue"], &slot["holder"]),
                if tags.is_empty() {
                    String::new()
                } else {
                    format!(" {}", escape(&tags.join(" ")))
                },
                if slot["state"] == "orphan" {
                    " ORPHAN"
                } else {
                    ""
                }
            )
        })
        .collect();
    if slot_chips.is_empty() {
        r#"<span class="note">no slot held</span>"#.clone_into(&mut slot_chips);
    }
    if snapshot["leases"].is_null() {
        slot_chips = String::new();
    }
    let processes: Vec<String> = list(&machine["gpu_processes"])
        .iter()
        .map(|process| {
            let name = if truthy(&process["container"]) {
                &process["container"]
            } else {
                &process["name"]
            };
            format!(
                "{} pid {} {} GB",
                e(name),
                t(&process["pid"]),
                gb(&process["used_mb"])
            )
        })
        .collect();
    let containers: Vec<String> = list(&machine["containers"])
        .iter()
        .map(|container| {
            format!(
                r#"<li><span class="line" title="{}">{}</span><span class="sub">{}</span></li>"#,
                e(&container["image"]),
                e(&container["name"]),
                e(&container["status"])
            )
        })
        .collect();
    let mut details = String::new();
    if !processes.is_empty() {
        write!(
            details,
            r#"<li title="processes on the device">device: {}</li>"#,
            processes.join("; ")
        )
        .ok();
    }
    if !containers.is_empty() {
        write!(
            details,
            r#"<li class="fold" data-key="containers"><details><summary>containers <span class="sub">{}</span></summary><ul class="containers">{}</ul></details></li>"#,
            containers.len(),
            containers.concat()
        )
        .ok();
    }
    let error = if truthy(&machine["probe_error"]) {
        format!(
            r#"<div class="err">live read: {}</div>"#,
            e(&machine["probe_error"])
        )
    } else {
        String::new()
    };
    let sampled = if machine["age_s"].is_null() {
        r#"<div class="note">no live sample yet</div>"#.to_owned()
    } else {
        format!(
            r#"<div class="note">sampled {}</div>"#,
            sample_time(&machine["sampled_at"])
        )
    };
    let facts = if facts.is_empty() {
        "&nbsp;".to_owned()
    } else {
        facts.join(" &middot; ")
    };
    let slots = if slot_chips.is_empty() {
        String::new()
    } else {
        format!(r#"<div style="margin-top:6px">{slot_chips}</div>"#)
    };
    format!(
        concat!(
            r#"<article class="panel"><div class="card-head"><b title="{name}">{name}</b><span class="pills">{pills}</span></div>"#,
            r#"<div class="note">{facts}</div><div class="meters">{meters}</div>{link}{sparks}{slots}"#,
            r#"<ul class="list">{details}</ul>{error}{sampled}</article>"#
        ),
        name = e(&machine["name"]),
        pills = pills.concat(),
        facts = facts,
        meters = meters,
        link = link,
        sparks = sparks,
        slots = slots,
        details = details,
        error = error,
        sampled = sampled,
    )
}

fn points(series: &Value) -> Vec<(f64, f64)> {
    list(series)
        .iter()
        .filter_map(|point| Some((point[0].as_f64()?, point[1].as_f64()?)))
        .collect()
}

/// One labelled sparkline with its current value and peak.
#[allow(clippy::too_many_arguments)]
fn spark_row(
    label: &str,
    window_text: &str,
    points: &[(f64, f64)],
    now: f64,
    hours: f64,
    fixed_max: Option<f64>,
    reference: Option<f64>,
    unit: &str,
    down: &[f64],
) -> String {
    let decimals = if unit.is_empty() { 2 } else { 0 };
    let current = points.last().map_or_else(
        || "-".to_owned(),
        |(_, value)| format!("{}{unit}", number_of(*value, decimals)),
    );
    let peak = points
        .iter()
        .map(|(_, value)| *value)
        .reduce(f64::max)
        .map_or_else(
            || "no data".to_owned(),
            |peak| format!("peak {}{unit}", number_of(peak, decimals)),
        );
    let svg = sparkline(points, now, hours, fixed_max, reference, unit, down);
    format!(
        r#"<div class="spark"><span class="k">{label}<br>{window_text}</span>{svg}<span class="v"><b style="color:var(--ink)">{current}</b><br>{peak}</span></div>"#
    )
}

/// A time-scaled sparkline: area, line, optional reference line, down marks and hover targets.
/// A gap longer than three times the typical sample spacing breaks the line, so a missing
/// stretch of probes reads as missing rather than as a straight ramp.
fn sparkline(
    points: &[(f64, f64)],
    now: f64,
    hours: f64,
    fixed_max: Option<f64>,
    reference: Option<f64>,
    unit: &str,
    down: &[f64],
) -> String {
    let (width, height) = (SPARK_WIDTH, SPARK_HEIGHT);
    let start = now - hours * 3600.0;
    let span_s = (now - start).max(1.0);
    let visible: Vec<(f64, f64)> = points
        .iter()
        .copied()
        .filter(|(epoch, _)| *epoch >= start)
        .collect();
    let mut y_max = fixed_max.unwrap_or_else(|| {
        visible
            .iter()
            .map(|(_, value)| *value)
            .fold(reference.unwrap_or(0.0), f64::max)
            * 1.1
    });
    if y_max == 0.0 {
        y_max = 1.0;
    }
    let x_of = |epoch: f64| (epoch - start) / span_s * width;
    let y_of = |value: f64| height - 1.0 - (value / y_max).clamp(0.0, 1.0) * (height - 3.0);
    let mut spacings: Vec<f64> = visible
        .windows(2)
        .map(|pair| pair[1].0 - pair[0].0)
        .collect();
    spacings.sort_by(f64::total_cmp);
    let gap_s = if spacings.is_empty() {
        span_s
    } else {
        spacings[spacings.len() / 2] * 3.0
    };
    let mut segments: Vec<Vec<(f64, f64)>> = Vec::new();
    for point in &visible {
        match segments.last_mut() {
            Some(segment) if segment.last().is_some_and(|last| point.0 - last.0 <= gap_s) => {
                segment.push(*point);
            }
            _ => segments.push(vec![*point]),
        }
    }
    let mut parts = String::new();
    if let Some(reference) = reference.filter(|reference| *reference <= y_max) {
        write!(
            parts,
            r#"<line class="ref" x1="0" x2="{}" y1="{:.1}" y2="{:.1}"/>"#,
            float_g(width),
            y_of(reference),
            y_of(reference)
        )
        .ok();
    }
    let height_text = float_g(height);
    for segment in &segments {
        let coordinates: Vec<String> = segment
            .iter()
            .map(|(epoch, value)| format!("{:.1},{:.1}", x_of(*epoch), y_of(*value)))
            .collect();
        let coordinates = coordinates.join(" ");
        if let [(epoch, value)] = segment.as_slice() {
            write!(
                parts,
                r#"<rect class="area" x="{:.1}" y="{:.1}" width="2" height="{:.1}"/>"#,
                x_of(*epoch) - 1.0,
                y_of(*value),
                height - y_of(*value)
            )
            .ok();
            continue;
        }
        let (first, last) = (segment[0].0, segment[segment.len() - 1].0);
        write!(
            parts,
            r#"<polygon class="area" points="{:.1},{height_text} {coordinates} {:.1},{height_text}"/><polyline class="line" points="{coordinates}"/>"#,
            x_of(first),
            x_of(last)
        )
        .ok();
    }
    for epoch in down.iter().copied().filter(|epoch| *epoch >= start) {
        write!(
            parts,
            r#"<rect class="down" x="{:.1}" y="{}" width="2" height="3"><title>{} down</title></rect>"#,
            x_of(epoch) - 1.0,
            float_g(height - 3.0),
            clock_epoch(epoch, false)
        )
        .ok();
    }
    #[allow(clippy::cast_precision_loss)]
    let bucket_s = span_s / SPARK_HIT_BUCKETS as f64;
    let decimals = if unit.is_empty() { 2 } else { 0 };
    for bucket in 0..SPARK_HIT_BUCKETS {
        #[allow(clippy::cast_precision_loss)]
        let bucket_start = start + bucket as f64 * bucket_s;
        let in_bucket: Vec<f64> = visible
            .iter()
            .filter(|(epoch, _)| bucket_start <= *epoch && *epoch < bucket_start + bucket_s)
            .map(|(_, value)| *value)
            .collect();
        if in_bucket.is_empty() {
            continue;
        }
        let max = in_bucket.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        #[allow(clippy::cast_precision_loss)]
        let mean = in_bucket.iter().sum::<f64>() / in_bucket.len() as f64;
        #[allow(clippy::cast_precision_loss)]
        let bucket_width = width / SPARK_HIT_BUCKETS as f64;
        write!(
            parts,
            r#"<rect class="hit" x="{:.1}" y="0" width="{bucket_width:.1}" height="{height_text}"><title>{}: max {}{unit}, mean {}{unit}</title></rect>"#,
            x_of(bucket_start),
            clock_epoch(bucket_start, false),
            number_of(max, decimals),
            number_of(mean, decimals)
        )
        .ok();
    }
    if visible.is_empty() {
        write!(
            parts,
            r#"<text x="{}" y="{}" text-anchor="middle" font-size="10" fill="currentColor" opacity=".5">no samples in window</text>"#,
            float_repr(width / 2.0),
            float_repr(height / 2.0 + 4.0)
        )
        .ok();
    }
    format!(
        r#"<svg viewBox="0 0 {} {height_text}" preserveAspectRatio="none" role="img" aria-label="{} samples">{parts}</svg>"#,
        float_g(width),
        visible.len()
    )
}

/// The live ticks.
fn ticks_panel(snapshot: &Value) -> String {
    let ticks = list(&snapshot["ticks"]);
    let with_leases = !snapshot["leases"].is_null();
    let mut rows = String::new();
    for tick in ticks {
        let issue_cell = if !tick["issue"].is_null() {
            format!(
                r#"{} <span title="{1}">{1}</span>"#,
                issue_link(&tick["issue"], &tick["url"]),
                e(&tick["title"])
            )
        } else if truthy(&tick["choosing"]) {
            r#"<span class="note">choosing an issue</span>"#.to_owned()
        } else {
            r#"<span class="err">issue unknown</span>"#.to_owned()
        };
        let kind = s(&tick["kind"]);
        let kind_level = match kind {
            "lane" => "serious",
            "landing" => "info",
            _ => "good",
        };
        let model_text = format!(
            "{} {}{}",
            s(&tick["agent"]),
            tick["model_label"].as_str().unwrap_or("?"),
            if kind == "work" {
                String::new()
            } else {
                format!(" {kind}")
            }
        );
        let doing = s(&tick["doing"]);
        let doing_cell = if doing.is_empty() {
            r#"<span class="note">thinking</span>"#.to_owned()
        } else {
            format!(
                r#"<span title="{0}">{1} {0}</span>"#,
                escape(doing),
                duration(&tick["doing_age_s"])
            )
        };
        let leases = if with_leases {
            let slots: Vec<String> = list(&tick["lease_slots"])
                .iter()
                .map(|slot| format!("#{}", t(slot)))
                .collect();
            format!(
                "<td>{}</td>",
                if slots.is_empty() {
                    "-".to_owned()
                } else {
                    slots.join(" ")
                }
            )
        } else {
            String::new()
        };
        let slot = if truthy(&tick["slot"]) {
            t(&tick["slot"])
        } else {
            "-".to_owned()
        };
        let source = if truthy(&tick["issue_source"]) {
            e(&tick["issue_source"])
        } else {
            "-".to_owned()
        };
        write!(
            rows,
            r#"<tr><td>{issue_cell}</td><td>{}</td><td>{slot}</td><td>{}</td>{leases}<td>{doing_cell}</td><td class="sub">{} <span class="chip">{source}</span></td></tr>"#,
            pill(kind_level, &model_text),
            duration(&tick["age_s"]),
            t(&tick["pid"])
        )
        .ok();
    }
    let table = if rows.is_empty() {
        r#"<div class="empty">no tick running</div>"#.to_owned()
    } else {
        format!(
            concat!(
                r#"<table><colgroup><col style="width:34%"><col style="width:96px"><col style="width:40px">"#,
                r#"<col style="width:56px">{}<col><col style="width:118px"></colgroup>"#,
                "<tr><th>Issue</th><th>Model</th><th>Slot</th><th>Age</th>{}<th>Now</th><th>PID / from</th></tr>{}</table>"
            ),
            if with_leases {
                r#"<col style="width:56px">"#
            } else {
                ""
            },
            if with_leases { "<th>Leases</th>" } else { "" },
            rows
        )
    };
    format!(
        r#"<article class="panel"><h2>Running ticks <span class="sub">{} live</span></h2><div class="hscroll">{table}</div></article>"#,
        ticks.len()
    )
}

/// The resource slots, whole-device holds, device lease, operator priority and waits; nothing
/// when the instance reports no resources.
fn leases_panel(snapshot: &Value) -> String {
    let leases = &snapshot["leases"];
    if leases.is_null() {
        return String::new();
    }
    if !truthy(&leases["available"]) {
        let error = if truthy(&leases["error"]) {
            e(&leases["error"])
        } else {
            "resource status not read".to_owned()
        };
        return format!(
            r#"<article class="panel"><h2>Resource slots and leases</h2><div class="err">{error}</div></article>"#
        );
    }
    let mut rows = String::new();
    for slot in list(&leases["slots"]) {
        let state = s(&slot["state"]);
        let holder_kind = s(&slot["holder_kind"]);
        let tags: Vec<String> = list(&slot["tags"]).iter().map(text_of).collect();
        let chips: String = tags
            .iter()
            .map(|tag| format!("<span class=chip>{}</span>", escape(tag)))
            .collect();
        let free = state == "free";
        write!(
            rows,
            concat!(
                r#"<tr class="{dim}"><td>{slot}</td><td title="{host}">{short}</td><td>{state}</td><td>{issue}</td>"#,
                r#"<td title="{kind_title}">{kind} {chips}</td><td>{held}</td></tr>"#
            ),
            dim = if free { "dim" } else { "" },
            slot = t(&slot["slot"]),
            host = e(&slot["host"]),
            short = escape(short_host(s(&slot["host"]))),
            state = pill(status_level(state).unwrap_or("idle"), state),
            issue = if free { String::new() } else { issue_ref(&slot["issue"], &slot["holder"]) },
            kind_title = escape(&format!("{holder_kind} {}", tags.join(" "))),
            kind = escape(holder_kind.split(' ').next().unwrap_or_default()),
            chips = chips,
            held = if free { String::new() } else { duration(&slot["held_s"]) },
        )
        .ok();
    }
    let table = format!(
        concat!(
            r#"<table><colgroup><col style="width:36px"><col style="width:27%"><col style="width:74px">"#,
            r#"<col style="width:58px"><col><col style="width:52px"></colgroup>"#,
            "<tr><th>Slot</th><th>Host</th><th>State</th><th>Issue</th><th>Holder and tags</th><th>Held</th></tr>{}</table>"
        ),
        rows
    );
    let mut notes = Vec::new();
    for hold in list(&leases["whole_card_holds"]) {
        let verb = if truthy(&hold["reserving"]) {
            "reserving"
        } else {
            "holds"
        };
        let slots: Vec<String> = list(&hold["slots"]).iter().map(t).collect();
        notes.push(format!(
            "<li>{} {} {verb} {} (slots {}) for {}, holder {}</li>",
            pill("info", "whole device"),
            issue_ref(&hold["issue"], &hold["holder"]),
            escape(short_host(s(&hold["host"]))),
            slots.join(", "),
            duration(&hold["held_s"]),
            e(&hold["holder_kind"])
        ));
    }
    let device = &leases["device"];
    if truthy(device) {
        let state = s(&device["state"]);
        notes.push(format!(
            "<li>{} {} for {}</li>",
            pill(
                status_level(state).unwrap_or("idle"),
                &format!("device {state}")
            ),
            issue_ref(&device["issue"], &device["holder"]),
            duration(&device["held_s"])
        ));
    }
    let priority: Vec<String> = list(&leases["operator_priority"])
        .iter()
        .map(|item| {
            issue_ref(
                &optional_int(s(item)).map_or(Value::Null, Value::from),
                item,
            )
        })
        .collect();
    notes.push(format!(
        "<li>operator priority: {}</li>",
        if priority.is_empty() {
            r#"<span class="note">none</span>"#.to_owned()
        } else {
            priority.join(" > ")
        }
    ));
    for wait in list(&leases["waits"]) {
        let tag = if truthy(&wait["tag"]) {
            format!(" ({})", e(&wait["tag"]))
        } else {
            String::new()
        };
        notes.push(format!(
            "<li>{} {} for {} {} min{tag}</li>",
            pill("warning", "waiting"),
            issue_ref(&wait["issue"], &Value::Null),
            e(&wait["kind"]),
            t(&wait["minutes"])
        ));
    }
    let preferred: Vec<String> = list(&leases["harness_only_hosts"])
        .iter()
        .map(text_of)
        .collect();
    if !preferred.is_empty() {
        notes.push(format!(
            "<li>harness hosts, preferred first: {}</li>",
            escape(&preferred.join(" > "))
        ));
    }
    if truthy(&leases["local_card_off"]) {
        notes.push("<li>local device off: every run goes to a remote device</li>".to_owned());
    }
    for line in list(&leases["unparsed"]) {
        notes.push(format!(r#"<li class="note">{}</li>"#, e(line)));
    }
    let error = if truthy(&leases["error"]) {
        format!(
            r#"<div class="err">last read failed: {}</div>"#,
            e(&leases["error"])
        )
    } else {
        String::new()
    };
    let slots = list(&leases["slots"]);
    let held = slots.iter().filter(|slot| slot["state"] != "free").count();
    format!(
        r#"<article class="panel"><h2>Resource slots and leases <span class="sub">{held} of {} held</span></h2><div class="hscroll">{table}</div><ul class="list">{}</ul>{error}</article>"#,
        slots.len(),
        notes.concat()
    )
}

/// fleet's devices: reachability, CPU, memory, GPUs and each adapter's dispatched sessions
/// against its capacity; nothing without `FLEET_STATUS_CMD` or when its document lists no devices.
fn fleet_devices_panel(snapshot: &Value) -> String {
    let fleet = &snapshot["fleet"];
    if fleet.is_null() || (truthy(&fleet["available"]) && fleet["devices"].is_null()) {
        return String::new();
    }
    let devices = list(&fleet["devices"]);
    let mut rows = String::new();
    for device in devices {
        let (state, reading) = if truthy(&device["error"]) {
            (
                pill("critical", "unreachable"),
                format!(
                    r#"<td colspan="4" class="err" title="{0}">{0}</td>"#,
                    e(&device["error"])
                ),
            )
        } else {
            let load = list(&device["load"]);
            let gpus: Vec<String> = list(&device["gpus"])
                .iter()
                .map(|gpu| {
                    let memory = if gpu["mem_total_bytes"].is_null() {
                        String::new()
                    } else {
                        format!(
                            " {} / {} GB",
                            bytes_gb(&gpu["mem_used_bytes"]),
                            bytes_gb(&gpu["mem_total_bytes"])
                        )
                    };
                    format!("{} {}{memory}", e(&gpu["name"]), percent(&gpu["util_pct"]))
                })
                .collect();
            let gpus = if gpus.is_empty() {
                r#"<span class="note">none</span>"#.to_owned()
            } else {
                gpus.join("; ")
            };
            let adapters: String = list(&device["adapters"])
                .iter()
                .map(|adapter| {
                    let models: Vec<String> =
                        list(&adapter["models"]).iter().map(text_of).collect();
                    let capacity = if truthy(&adapter["max_concurrent"]) {
                        format!(
                            "{}/{}",
                            t(&adapter["running"]),
                            t(&adapter["max_concurrent"])
                        )
                    } else {
                        "watch only".to_owned()
                    };
                    format!(
                        r#"<span class="chip" title="{}">{} {capacity}</span>"#,
                        escape(&models.join(", ")),
                        e(&adapter["name"])
                    )
                })
                .collect();
            let cpus = if truthy(&device["cpus"]) {
                t(&device["cpus"])
            } else {
                "?".to_owned()
            };
            (
                pill("good", "reachable"),
                format!(
                    r#"<td class="num">{} &middot; load {} / {cpus}</td><td class="num">{} / {} GB</td><td class="wrap">{gpus}</td><td class="wrap">{adapters}</td>"#,
                    percent(&device["cpu_percent"]),
                    number(load.first().unwrap_or(&Value::Null), 2),
                    bytes_gb(&device["mem_used_bytes"]),
                    bytes_gb(&device["mem_total_bytes"])
                ),
            )
        };
        let host: Vec<String> = [&device["hostname"], &device["os"]]
            .into_iter()
            .filter(|part| truthy(part))
            .map(e)
            .collect();
        write!(
            rows,
            r#"<tr><td title="{0}"><b>{0}</b><div class="note line">{1}</div></td><td>{2}</td><td>{state}</td>{reading}</tr>"#,
            e(&device["name"]),
            host.join(" &middot; "),
            e(&device["transport"])
        )
        .ok();
    }
    let table = if rows.is_empty() {
        r#"<div class="empty">no device read</div>"#.to_owned()
    } else {
        format!(
            concat!(
                r#"<table><colgroup><col style="width:18%"><col style="width:60px"><col style="width:110px">"#,
                r#"<col style="width:150px"><col style="width:110px"><col><col style="width:22%"></colgroup>"#,
                "<tr><th>Device</th><th>Via</th><th>State</th><th>CPU</th><th>Memory</th><th>GPUs</th><th>Adapters</th></tr>{}</table>"
            ),
            rows
        )
    };
    let reachable = devices
        .iter()
        .filter(|device| !truthy(&device["error"]))
        .count();
    format!(
        r#"<article class="panel"><h2>Devices <span class="sub">{reachable} of {} reachable through fleet</span></h2><div class="hscroll">{table}</div>{}</article>"#,
        devices.len(),
        fleet_read_note(fleet)
    )
}

/// The agent sessions fleet reports on every device, dispatched and discovered; nothing without
/// `FLEET_STATUS_CMD`.
fn fleet_sessions_panel(snapshot: &Value) -> String {
    let fleet = &snapshot["fleet"];
    if fleet.is_null() {
        return String::new();
    }
    let sessions = list(&fleet["sessions"]);
    let mut rows = String::new();
    for session in sessions {
        let running = truthy(&session["running"]);
        let level = if running {
            "good"
        } else if session["state"] == "exited 0" {
            "idle"
        } else {
            "warning"
        };
        let issue = if session["issue"].is_null() {
            r#"<span class="note">-</span>"#.to_owned()
        } else {
            format!(
                r#"{} <span class="chip">{}</span>"#,
                issue_link(&session["issue"], &session["url"]),
                e(&session["issue_source"])
            )
        };
        let who = if session["pid"].is_null() {
            e(&session["id"])
        } else {
            format!("pid {}", t(&session["pid"]))
        };
        let model = if truthy(&session["model"]) {
            format!(r#" <span class="sub">{}</span>"#, e(&session["model"]))
        } else {
            String::new()
        };
        let cwd = if truthy(&session["cwd"]) {
            e(&session["cwd"])
        } else {
            "-".to_owned()
        };
        write!(
            rows,
            concat!(
                r#"<tr class="{dim}"><td>{device}</td><td>{adapter}{model}</td><td title="{id} {origin}: {summary}">{who}</td>"#,
                r#"<td>{state}</td><td>{age}</td><td title="{cwd_title}">{cwd}</td><td title="{title}">{issue}</td></tr>"#
            ),
            dim = if running { "" } else { "dim" },
            device = e(&session["device"]),
            adapter = e(&session["adapter"]),
            model = model,
            id = e(&session["id"]),
            origin = e(&session["origin"]),
            summary = e(&session["summary"]),
            who = who,
            state = pill(level, s(&session["state"])),
            age = duration(&session["age_s"]),
            cwd_title = e(&session["cwd"]),
            cwd = cwd,
            title = e(&session["title"]),
            issue = issue,
        )
        .ok();
    }
    let table = if rows.is_empty() {
        r#"<div class="empty">no agent session</div>"#.to_owned()
    } else {
        format!(
            concat!(
                r#"<table><colgroup><col style="width:14%"><col style="width:16%"><col style="width:16%">"#,
                r#"<col style="width:100px"><col style="width:56px"><col><col style="width:110px"></colgroup>"#,
                "<tr><th>Device</th><th>Adapter</th><th>Session</th><th>State</th><th>Age</th><th>Workdir</th><th>Issue</th></tr>{}</table>"
            ),
            rows
        )
    };
    let mut notes = String::new();
    if fleet["devices"].is_null() {
        for error in list(&fleet["errors"]) {
            write!(notes, r#"<li class="err">{}</li>"#, e(error)).ok();
        }
    }
    for name in list(&fleet["truncated"]) {
        write!(
            notes,
            r#"<li class="note">{}: more sessions than one answer holds</li>"#,
            e(name)
        )
        .ok();
    }
    let notes = if notes.is_empty() {
        String::new()
    } else {
        format!("<ul class=list>{notes}</ul>")
    };
    let running = sessions
        .iter()
        .filter(|session| truthy(&session["running"]))
        .count();
    format!(
        r#"<article class="panel"><h2>Agent sessions <span class="sub">{running} running</span></h2><div class="hscroll">{table}</div>{notes}{}</article>"#,
        fleet_read_note(fleet)
    )
}

fn fleet_read_note(fleet: &Value) -> String {
    if truthy(&fleet["error"]) {
        return format!(
            r#"<div class="err">last read failed: {}</div>"#,
            e(&fleet["error"])
        );
    }
    format!(
        r#"<div class="note">read {}</div>"#,
        sample_time(&fleet["fetched_at"])
    )
}

/// One panel per issue group, sign-off stacked under claimable since both are usually short.
/// The claimable panel leads with the dashboard's own count, sets the queue count's and the
/// allocator's beside it, and lists under it what the first-before-low gate holds back.
fn queue_section(snapshot: &Value) -> String {
    let queue = &snapshot["queue"];
    let groups = &queue["groups"];
    let labels = &snapshot["harness"]["labels"];
    let mut panels: Vec<(&str, String)> = Vec::new();
    for (name, title) in QUEUE_TITLES {
        let issues = list(&groups[name]);
        let mut rows: String = issues
            .iter()
            .map(|issue| issue_row(issue, labels))
            .collect();
        if rows.is_empty() {
            r#"<tr><td class="empty" colspan="2">none</td></tr>"#.clone_into(&mut rows);
        }
        let mut extra = String::new();
        if name == "claimable" {
            extra = claimable_counts(queue, s(&snapshot["harness"]["queue_label"]));
            let held = list(&groups["held"]);
            if !held.is_empty() {
                write!(rows, r#"<tr><td colspan="2" class="note"><b>held back by the first-before-low gate: {}</b></td></tr>"#, held.len()).ok();
                rows.extend(held.iter().map(|issue| issue_row(issue, labels)));
            }
        }
        panels.push((
            name,
            format!(
                r#"<article class="panel"><h2>{title} <span class="sub">{}{extra}</span></h2><div class="scroll"><table><colgroup><col><col style="width:60px"></colgroup>{rows}</table></div></article>"#,
                issues.len()
            ),
        ));
    }
    let panel = |name: &str| {
        panels
            .iter()
            .find(|(key, _)| *key == name)
            .map_or("", |(_, body)| body.as_str())
            .to_owned()
    };
    let status = if truthy(&queue["error"]) {
        format!(r#"<div class="err">GitHub: {}</div>"#, e(&queue["error"]))
    } else {
        String::new()
    };
    let age = if truthy(&queue["fetched_at"]) {
        format!("issues read {}", sample_time(&queue["fetched_at"]))
    } else {
        "issues not read yet".to_owned()
    };
    format!(
        r#"<section><div class="note" style="margin:0 2px 6px">Issue queue &middot; {age}</div>{status}<div class="grid queue">{}<div class="stack">{}{}</div>{}{}</div></section>"#,
        panel("running"),
        panel("claimable"),
        panel("signoff"),
        panel("operator"),
        panel("other")
    )
}

/// The queue count's and the allocator's counts beside the dashboard's own, each marked
/// unverified when the dashboard's own GitHub read was failing as it was taken: the queue count
/// prints 1 when its own read fails.
fn claimable_counts(queue: &Value, queue_label: &str) -> String {
    let unverified = pill("warning", "unverified");
    let mut text = format!(" from this read ({})", escape(queue_label));
    let claimable = &queue["claimable_sh"];
    if !claimable["value"].is_null() {
        write!(
            text,
            " &middot; queue count {}, {}{}",
            t(&claimable["value"]),
            sample_time(&claimable["fetched_at"]),
            if truthy(&claimable["unverified"]) {
                format!(" {unverified}")
            } else {
                String::new()
            }
        )
        .ok();
    } else if truthy(&claimable["error"]) {
        write!(
            text,
            r#" &middot; <span class="err">queue count: {}</span>"#,
            e(&claimable["error"])
        )
        .ok();
    }
    let allocator = &queue["allocator_claimable"];
    if truthy(allocator) {
        write!(
            text,
            " &middot; allocator {} at {}{}",
            e(&allocator["value"]),
            clock(&allocator["at"], false),
            if truthy(&allocator["unverified"]) {
                format!(" {unverified}")
            } else {
                String::new()
            }
        )
        .ok();
    }
    text
}

/// One issue: number and title, then its labels other than the queue's own and why it is in its
/// group, then the age of its last comment.
fn issue_row(issue: &Value, labels: &Value) -> String {
    let all_label = labels["all_issues"].as_str().unwrap_or("ai");
    let structural: Vec<&str> = ["all_issues", "queue", "wip", "operator", "signoff"]
        .iter()
        .map(|key| s(&labels[*key]))
        .collect();
    let strong: Vec<&str> = ["first", "lane"]
        .iter()
        .map(|key| s(&labels[*key]))
        .filter(|label| !label.is_empty())
        .collect();
    let prefix = format!("{all_label}:");
    let flags: String = list(&issue["labels"])
        .iter()
        .map(s)
        .filter(|label| !structural.contains(label))
        .map(|label| {
            format!(
                r#"<span class="chip{}">{}</span>"#,
                if strong.contains(&label) {
                    " strong"
                } else {
                    ""
                },
                escape(label.strip_prefix(&prefix).unwrap_or(label))
            )
        })
        .collect();
    let last_comment = or(&issue["last_comment_at"], &issue["updated_at"]);
    let title = e(&issue["title"]);
    format!(
        r#"<tr><td title="{title}">{} {title}<div class="note line">{flags}{}</div></td><td class="sub" title="last comment {}">{}</td></tr>"#,
        issue_link(&issue["number"], &issue["url"]),
        e(&issue["reason"]),
        e(last_comment),
        sample_time(last_comment)
    )
}

/// The PRs merged in the last 24 hours.
fn landings_panel(snapshot: &Value) -> String {
    let landings = list(&snapshot["landings"]);
    let mut rows: String = landings
        .iter()
        .map(|pull| {
            format!(
                r#"<tr><td title="{0}">{1} {0}</td><td><span class="chip">{2}</span></td><td class="sub">{3}</td></tr>"#,
                e(&pull["title"]),
                issue_link(&pull["number"], &pull["url"]),
                e(&pull["baseRefName"]),
                sample_time(&pull["mergedAt"])
            )
        })
        .collect();
    if rows.is_empty() {
        r#"<tr><td class="empty">none in the last 24 h</td></tr>"#.clone_into(&mut rows);
    }
    format!(
        r#"<article class="panel"><h2>Landings <span class="sub">{} merged in 24 h</span></h2><div class="scroll"><table><colgroup><col><col style="width:110px"><col style="width:60px"></colgroup>{rows}</table></div></article>"#,
        landings.len()
    )
}

/// The newest lines of state/stalls.log; nothing when the file does not exist.
fn stalls_panel(snapshot: &Value) -> String {
    let logged = &snapshot["health"]["stalls"];
    if logged.is_null() {
        return String::new();
    }
    let mut rows: String = list(logged)
        .iter()
        .rev()
        .map(|stall| {
            let kind = s(&stall["kind"]);
            format!(
                r#"<tr><td class="sub">{}</td><td>{}</td><td title="{}"><div class="clamp"><b>{}</b> {}</div></td></tr>"#,
                sample_time(&stall["time"]),
                pill(if kind == "host_degraded" { "serious" } else { "warning" }, kind),
                e(&stall["detail"]),
                escape(short_host(s(&stall["key"]))),
                escape(&clip(s(&stall["detail"]), 220))
            )
        })
        .collect();
    if rows.is_empty() {
        r#"<tr><td class="empty">no stall logged</td></tr>"#.clone_into(&mut rows);
    }
    format!(
        r#"<article class="panel"><h2>Stalls <span class="sub">state/stalls.log, newest first</span></h2><div class="scroll"><table><colgroup><col style="width:60px"><col style="width:120px"><col></colgroup>{rows}</table></div></article>"#
    )
}

/// The supervisor: when it is next due, the directive in force with its stops, its last runs and
/// the directive log's tail. Left off while the supervisor is off: the snapshot has no key for it.
fn supervisor_panel(snapshot: &Value) -> String {
    let view = &snapshot["supervisor"];
    let directive = &view["directive"];
    let status = s(&directive["status"]);
    if view.is_null() {
        return String::new();
    }
    let state = format!(
        "on, next due between {} and {}",
        e(&view["next_due"]["earliest"]),
        e(&view["next_due"]["latest"])
    );
    let level = match status {
        "active" => "good",
        "invalid" => "critical",
        "expired" => "idle",
        _ => "info",
    };
    let body = &directive["directive"];
    let mut fields: Vec<String> = ["width", "models", "wake_at", "focus", "avoid"]
        .iter()
        .filter(|key| !body[**key].is_null())
        .map(|key| format!("{key}={}", escape(&text_of_list(&body[*key]))))
        .collect();
    if !body["pause"].is_null() {
        fields.push(format!(
            "pause until {}: {}",
            e(&body["pause"]["until"]),
            e(&body["pause"]["reason"])
        ));
    }
    let detail = match status {
        "invalid" => e(&directive["error"]),
        "none" | "" => "none: the configuration's own formulas hold".to_owned(),
        _ => format!(
            "<b>{}</b> until {} {}",
            e(&body["reason"]),
            e(&body["expires_at"]),
            fields.join(" ")
        ),
    };
    let stops: String = list(&directive["stops"])
        .iter()
        .map(|stop| {
            format!(
                r#"<tr><td class="sub">{}</td><td>pid {}</td><td>{} {}</td></tr>"#,
                e(&stop["due_at"]),
                t(&stop["pid"]),
                if truthy(&stop["sent"]) {
                    "sent"
                } else {
                    "pending"
                },
                e(&stop["reason"])
            )
        })
        .collect();
    let lines = |key: &str| -> String {
        list(&view[key])
            .iter()
            .map(|line| format!(r#"<tr><td><div class="clamp">{}</div></td></tr>"#, e(line)))
            .collect()
    };
    format!(
        concat!(
            r#"<article class="panel"><h2>Supervisor <span class="sub">{state}</span></h2>"#,
            r#"<p>{pill} {detail}</p><div class="scroll"><table>{stops}</table>"#,
            r#"<h3>Last runs <span class="sub">{failures} failed in a row</span></h3><table>{runs}</table>"#,
            r#"<h3>Directive log <span class="sub">state/logs/directive.log, newest first</span></h3><table>{log}</table></div></article>"#
        ),
        state = state,
        pill = pill(level, if status.is_empty() { "none" } else { status }),
        detail = detail,
        stops = stops,
        failures = t(&view["failures"]),
        runs = lines("runs"),
        log = lines("directive_log"),
    )
}

/// A list's items joined by spaces, any other value as text.
fn text_of_list(value: &Value) -> String {
    match value.as_array() {
        Some(items) => items.iter().map(text_of).collect::<Vec<_>>().join(" "),
        None => text_of(value),
    }
}

/// The allocator's last decisions.
fn allocator_panel(snapshot: &Value) -> String {
    let mut rows = String::new();
    for line in list(&snapshot["health"]["allocator"]).iter().rev() {
        let fields = &line["fields"];
        let mut summary: Vec<String> = list(&line["usage"])
            .iter()
            .map(|agent| {
                let windows = match agent["windows"].as_array() {
                    Some(windows) => windows
                        .iter()
                        .map(|window| format!("{}:{}%", e(&window["name"]), t(&window["used_pct"])))
                        .collect::<Vec<_>>()
                        .join(","),
                    None => "unknown".to_owned(),
                };
                format!("{}={}[{windows}]", e(&agent["agent"]), t(&agent["cap"]))
            })
            .collect();
        summary.extend(
            [
                "usage",
                "allowed",
                "busy",
                "claimable",
                "young",
                "ready",
                "spawning",
            ]
            .iter()
            .filter(|key| !fields[**key].is_null())
            .map(|key| format!("{key}={}", e(&fields[*key]))),
        );
        if truthy(&line["claimable_unverified"]) {
            summary.push(pill("warning", "claimable unverified"));
        }
        let summary = if summary.is_empty() {
            e(&line["text"])
        } else {
            summary.join(" ")
        };
        write!(
            rows,
            r#"<tr><td class="sub">{}</td><td><span class="chip strong">{}</span>{}</td><td title="{}">{summary}</td></tr>"#,
            sample_time(&line["time"]),
            e(&line["kind"]),
            e(&line["slot"]),
            e(&line["text"])
        )
        .ok();
    }
    if rows.is_empty() {
        r#"<tr><td class="empty">no allocator line</td></tr>"#.clone_into(&mut rows);
    }
    format!(
        r#"<article class="panel"><h2>Allocator <span class="sub">state/logs/allocator.log, newest first</span></h2><div class="scroll"><table><colgroup><col style="width:60px"><col style="width:110px"><col></colgroup>{rows}</table></div></article>"#
    )
}

/// The flow diagram: devices on top, issue cards in state columns under them, a line from each
/// card to each device lane it holds (solid) or waits for (dashed), and the newest transitions
/// under it. A card that moved since the last diagram starts where it was and slides to its
/// place; new cards and lines fade in. Positions come from the snapshot, so the page needs no
/// script.
fn flow_section(snapshot: &Value) -> String {
    let flow = &snapshot["flow"];
    if !truthy(flow) {
        return String::new();
    }
    #[allow(clippy::cast_possible_truncation)]
    let (width, height) = (
        flow["width"].as_f64().unwrap_or(0.0) as i64,
        flow["height"].as_f64().unwrap_or(0.0) as i64,
    );
    let moved: Vec<&Value> = list(&flow["cards"])
        .iter()
        .filter(|card| truthy(&card["moved"]))
        .map(|card| &card["issue"])
        .collect();
    let mut paths = String::new();
    for edge in list(&flow["edges"]) {
        let coordinate = |key: &str| edge[key].as_f64().unwrap_or(0.0);
        let (x1, y1, x2, y2) = (
            coordinate("x1"),
            coordinate("y1"),
            coordinate("x2"),
            coordinate("y2"),
        );
        let bend = ((y1 - y2) / 2.0).max(40.0);
        let state = if moved.contains(&&edge["issue"]) {
            "late"
        } else if truthy(&edge["fresh"]) {
            "fresh"
        } else {
            ""
        };
        let edge_key: Vec<String> = ["issue", "device", "lane", "kind"]
            .iter()
            .map(|key| text_of(&edge[*key]))
            .collect();
        write!(
            paths,
            r#"<path class="fedge {lane} {kind} {state}" data-key="edge-{key}" d="M{x1:.0},{y1:.0} C{x1:.0},{:.0} {x2:.0},{:.0} {x2:.0},{y2:.0}"><title>#{issue} {kind} the {lane} lane of {device}</title></path>"#,
            y1 - bend,
            y2 + bend,
            lane = e(&edge["lane"]),
            kind = e(&edge["kind"]),
            key = escape(&edge_key.join("|")),
            issue = e(&edge["issue"]),
            device = e(&edge["device"]),
        )
        .ok();
    }
    let mut parts = vec![format!(
        r#"<svg class="flow-edges" data-key="edges" width="{width}" height="{height}" viewBox="0 0 {width} {height}" aria-hidden="true">{paths}</svg>"#
    )];
    parts.extend(list(&flow["devices"]).iter().map(flow_device));
    for column in list(&flow["columns"]) {
        parts.push(format!(
            r#"<div class="fcol" data-key="col-{}" style="left:{}px;top:{}px;width:{}px">{} <span class="sub">{}</span></div>"#,
            e(&column["key"]),
            t(&column["x"]),
            t(&column["y"]),
            t(&column["w"]),
            e(&column["title"]),
            t(&column["count"])
        ));
        if truthy(&column["hidden"]) {
            parts.push(format!(
                r#"<div class="fmore" data-key="more-{}" style="left:{}px;top:{}px">+{} more</div>"#,
                e(&column["key"]),
                column["x"].as_i64().unwrap_or(0) + 4,
                t(&column["more_y"]),
                t(&column["hidden"])
            ));
        }
    }
    parts.extend(list(&flow["cards"]).iter().map(flow_card));
    let mut legend = r#"<div class="flow-legend">"#.to_owned();
    for lane in list(&flow["lanes"]).iter().map(s) {
        let lane_name = if lane == "device" {
            "a device"
        } else {
            "a compile lane"
        };
        for kind in ["holds", "waits"] {
            write!(
                legend,
                r#"<span><svg width="28" height="8"><line x1="0" y1="4" x2="28" y2="4" class="fedge {lane} {kind}"/></svg> {} {lane_name}</span>"#,
                kind.replace("waits", "waits for"),
                lane = escape(lane),
            )
            .ok();
        }
    }
    legend.push_str("<span>cards slide to their new state on each refresh</span></div>");
    let mut trail: String = list(&flow["transitions"])
        .iter()
        .take(8)
        .map(|transition| {
            format!(
                r#"<li title="{0} → {1}"><span class="sub">{2}</span> <b>#{3}</b> {0} → {1}</li>"#,
                e(&transition["from"]),
                e(&transition["to"]),
                clock(&transition["time"], false),
                e(&transition["issue"])
            )
        })
        .collect();
    if trail.is_empty() {
        r#"<li class="note">no transition seen yet</li>"#.clone_into(&mut trail);
    }
    format!(
        concat!(
            r#"<section class="panel"><h2>Flow <span class="sub">issues by state, and the device each holds or waits for</span></h2>"#,
            r#"{}<div class="flow-scroll"><div class="flow-canvas" style="width:{}px;height:{}px">{}</div></div><ol class="flow-trail">{}</ol></section>"#
        ),
        legend,
        width,
        height,
        parts.concat(),
        trail
    )
}

/// One device node: device use and memory when probed, load, lease holders and compile lane
/// when the instance has them, and the idle flag.
fn flow_device(device: &Value) -> String {
    let util = &device["gpu_util_pct"];
    let flagged = truthy(&device["idle_with_queue"]);
    let flag = if flagged {
        pill(
            "warning",
            device["idle_flag"]
                .as_str()
                .filter(|flag| !flag.is_empty())
                .unwrap_or("device idle, queue waiting"),
        )
    } else {
        String::new()
    };
    let status_text = device["status"]
        .as_str()
        .filter(|status| !status.is_empty());
    let status = pill(
        status_level(status_text.unwrap_or("")).unwrap_or("idle"),
        status_text.unwrap_or("unknown"),
    );
    let cores = if truthy(&device["nproc"]) {
        t(&device["nproc"])
    } else {
        "?".to_owned()
    };
    let mut load = format!("{} / {cores} cores", number(&device["load1"], 1));
    let probed = !util.is_null() || truthy(&device["gpu_name"]);
    let mut usage = String::new();
    if probed {
        let width = util.as_f64().unwrap_or(0.0).clamp(0.0, 100.0);
        let name = if truthy(&device["gpu_name"]) {
            e(&device["gpu_name"])
        } else {
            "device".to_owned()
        };
        usage = format!(
            r#"<div class="sub">{name}</div><div style="display:grid;grid-template-columns:auto 1fr auto;gap:6px;align-items:center"><span class="sub">Device</span><span class="meter"><b style="width:{width:.0}%"></b></span><span class="num">{}</span></div>"#,
            percent(util)
        );
        if !device["gpu_mem_total_mb"].is_null() {
            write!(
                load,
                " &middot; device memory {} / {} GB",
                gb(&device["gpu_mem_used_mb"]),
                gb(&device["gpu_mem_total_mb"])
            )
            .ok();
        }
    }
    let device_width = device["w"].as_f64().unwrap_or(0.0);
    let mut holders = String::new();
    let mut anchors = String::new();
    if !device["holders"].is_null() {
        let held: Vec<String> = list(&device["holders"]).iter().map(e).collect();
        let held = if held.is_empty() {
            "no lease held".to_owned()
        } else {
            held.join(", ")
        };
        holders = format!(r#"<div class="holders" title="{held}">lease: {held}</div>"#);
        write!(
            anchors,
            r#"<span style="left:{:.0}px">▾ device</span>"#,
            device_width * 0.3
        )
        .ok();
    }
    let mut compile_line = String::new();
    if !device["compile"].is_null() {
        compile_line = format!(r#"<div class="sub">{}</div>"#, e(&device["compile"]));
        write!(
            anchors,
            r#"<span style="left:{:.0}px">▾ compile</span>"#,
            device_width * 0.75
        )
        .ok();
    }
    format!(
        concat!(
            r#"<div class="fdev{flagged}" data-key="device-{key}" style="left:{x}px;top:{y}px;width:{w}px;height:{h}px">"#,
            r#"<div class="top"><span><b>{tag}</b> <span class="sub">{name}</span></span><span class="pills">{flag}{status}</span></div>"#,
            r#"{usage}<div class="sub num">load {load}</div>{holders}{compile}<div class="anchors">{anchors}</div></div>"#
        ),
        flagged = if flagged { " flagged" } else { "" },
        key = e(&device["key"]),
        x = t(&device["x"]),
        y = t(&device["y"]),
        w = t(&device["w"]),
        h = t(&device["h"]),
        tag = e(&device["tag"]),
        name = e(&device["name"]),
        flag = flag,
        status = status,
        usage = usage,
        load = load,
        holders = holders,
        compile = compile_line,
        anchors = anchors,
    )
}

/// One issue card at its place; a moved card starts at its old place, a new one fades in.
fn flow_card(card: &Value) -> String {
    let mut classes = format!("fcard {}", e(&card["column"]));
    let mut motion = String::new();
    if truthy(&card["moved"]) && !card["from_x"].is_null() {
        classes.push_str(" moved");
        let delta = |from: &str, to: &str| {
            card[from].as_f64().unwrap_or(0.0) - card[to].as_f64().unwrap_or(0.0)
        };
        motion = format!(
            ";--dx:{:.0}px;--dy:{:.0}px",
            delta("from_x", "x"),
            delta("from_y", "y")
        );
    } else if truthy(&card["fresh"]) {
        classes.push_str(" fresh");
    }
    let chips: String = list(&card["chips"])
        .iter()
        .map(|chip| format!(r#"<span class="chip">{}</span>"#, e(chip)))
        .collect();
    let hover = format!(
        "#{} {}: {}",
        t(&card["issue"]),
        s(&card["title"]),
        s(&card["detail"])
    );
    format!(
        concat!(
            r#"<div class="{classes}" data-key="issue-{issue}" style="left:{x}px;top:{y}px;width:{w}px;height:{h}px{motion}" title="{hover}">"#,
            r#"<div class="l1">{link}{chips}</div><div class="l2">{detail}</div><div class="l3">{title}</div></div>"#
        ),
        classes = classes,
        issue = card["issue"].as_i64().unwrap_or(0),
        x = t(&card["x"]),
        y = t(&card["y"]),
        w = t(&card["w"]),
        h = t(&card["h"]),
        motion = motion,
        hover = escape(&hover),
        link = issue_link(&card["issue"], &card["url"]),
        chips = chips,
        detail = e(&card["detail"]),
        title = e(&card["title"]),
    )
}

/// Every source's age, cadence, duration and last error.
fn sources_footer(snapshot: &Value, token_query: &str) -> String {
    let sources = &snapshot["sources"];
    let mut names: Vec<&str> = SOURCE_ORDER
        .iter()
        .copied()
        .filter(|name| !sources[*name].is_null())
        .collect();
    if let Some(map) = sources.as_object() {
        names.extend(
            map.keys()
                .map(String::as_str)
                .filter(|name| !SOURCE_ORDER.contains(name)),
        );
    }
    let mut rows = String::new();
    for name in names {
        let source = &sources[name];
        if source["interval_s"].as_f64().unwrap_or(0.0) <= 0.0 && source["age_s"].is_null() {
            continue;
        }
        let age = if source["age_s"].is_null() {
            "never".to_owned()
        } else {
            duration(&source["age_s"])
        };
        let took = if source["duration_s"].is_null() {
            String::new()
        } else {
            format!("{} s", t(&source["duration_s"]))
        };
        let error_class = if truthy(&source["error"]) { "err" } else { "" };
        write!(
            rows,
            r#"<tr><td>{}</td><td>{age}</td><td>every {}</td><td>{took}</td><td class="{error_class}">{}</td></tr>"#,
            escape(name),
            duration(&source["interval_s"]),
            e(or(&source["error"], &Value::String(String::new())))
        )
        .ok();
    }
    format!(
        concat!(
            r#"<footer class="panel"><div class="hscroll"><table><colgroup><col style="width:120px"><col style="width:70px">"#,
            r#"<col style="width:90px"><col style="width:70px"><col></colgroup>"#,
            "<tr><th>Source</th><th>Age</th><th>Cadence</th><th>Took</th><th>Last error</th></tr>{}</table></div>",
            r#"<div style="margin-top:4px">JSON: <a href="/api/state{}">/api/state</a></div></footer>"#
        ),
        rows,
        escape(token_query)
    )
}

fn window_hours(snapshot: &Value) -> f64 {
    snapshot["history_hours"]
        .as_f64()
        .filter(|hours| *hours != 0.0)
        .unwrap_or(6.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn containers_fold_into_a_details_list() {
        let snapshot = json!({"generated_epoch": 0, "machines": [{
            "key": "local", "name": "box", "kind": "local", "status": "ok", "history": {}, "slots": [],
            "containers": [{"name": "dev-run-4", "status": "Up 8 minutes", "image": "sim:1"},
                           {"name": "sim_dev", "status": "Up 3 weeks", "image": "sim:2"}],
        }]});
        let page = render(&snapshot, 30, "");
        assert!(
            page.contains("<details><summary>containers <span class=\"sub\">2</span></summary>")
        );
        assert!(page.contains("<li><span class=\"line\" title=\"sim:1\">dev-run-4</span>"));
        assert!(!page.contains("dev-run-4 <span class=\"sub\">Up 8 minutes</span>; "));
    }

    #[test]
    fn every_value_is_escaped_and_only_https_links() {
        let snapshot = json!({"generated_epoch": 0, "leases": {"available": true, "slots": [], "waits": [
            {"issue": 1, "kind": "single-slot", "minutes": "<img src=x>", "tag": ""}]},
            "ticks": [{"issue": 7, "url": "javascript:alert(1)", "title": "t", "pid": "<b>pid</b>", "slot": "<q>"}],
            "machines": [{"key": "local", "name": "box", "kind": "local", "status": "ok", "history": {}, "slots": [],
                          "slot_count": "<script>", "nproc": "<u>"}],
            "flow": {"width": 10, "height": 10, "lanes": ["<x>"], "cards": [], "edges": [], "devices": [], "columns": []}});
        let page = render(&snapshot, 30, "");
        for raw in [
            "<img src=x>",
            "<b>pid</b>",
            "<q>",
            "<script> slots",
            "<u> cores",
            "class=\"fedge <x>",
        ] {
            assert!(!page.contains(raw), "{raw}");
        }
        assert!(!page.contains("javascript:"));
        assert!(page.contains("#7 "));
        assert_eq!(
            issue_link(&json!(1), &json!("https://example.com/i/1")),
            r#"<a href="https://example.com/i/1" target="_blank" rel="noopener noreferrer">#1</a>"#
        );
        assert_eq!(
            issue_link(&json!(1), &json!("http://example.com/i/1")),
            "#1"
        );
    }

    #[test]
    fn escape_covers_both_quotes() {
        assert_eq!(
            escape(r"<a href='x'>&"),
            "&lt;a href=&#x27;x&#x27;&gt;&amp;"
        );
    }
}
