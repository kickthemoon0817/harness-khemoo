//! Reads fleet's JSON documents (`fleet status --json`, `fleet sessions --json`; docs/FLEET.md,
//! "JSON output") into the records the Devices and Agent sessions panels show.
//!
//! A document is decoded strictly: the version must be [`OUTPUT_VERSION`] and every field read
//! must have its documented type, or the whole document is refused. Fields the panels do not
//! read are ignored, as the format asks of its readers.

use serde::Serialize;
use serde_json::{Map, Value};

pub const OUTPUT_VERSION: i64 = 1;
const FLOAT_LIMIT: f64 = 1e300;

/// One GPU a device reports.
#[derive(Clone, Debug, Serialize)]
pub struct FleetGpu {
    pub name: String,
    pub util_pct: Option<f64>,
    pub mem_used_bytes: Option<i64>,
    pub mem_total_bytes: Option<i64>,
}

/// One agent adapter on a device; `max_concurrent` 0 is discovery-only.
#[derive(Clone, Debug, Serialize)]
pub struct FleetAdapter {
    pub name: String,
    pub models: Vec<String>,
    pub max_concurrent: i64,
    pub running: i64,
}

/// One device of the registry: its reading when it answered, else the error the hub got.
#[derive(Clone, Debug, Default, Serialize)]
pub struct FleetDevice {
    pub name: String,
    pub transport: String,
    pub error: Option<String>,
    pub hostname: String,
    pub os: String,
    pub cpus: Option<i64>,
    pub load: Vec<f64>,
    pub cpu_percent: Option<f64>,
    pub mem_used_bytes: Option<i64>,
    pub mem_total_bytes: Option<i64>,
    pub sampled_at: Option<i64>,
    pub gpus: Vec<FleetGpu>,
    pub adapters: Vec<FleetAdapter>,
    pub sessions_truncated: bool,
}

/// One agent session on a device. `state` is the state's kind, with the exit code, signal or
/// failure after it.
#[derive(Clone, Debug)]
pub struct FleetSession {
    pub device: String,
    pub id: String,
    pub adapter: String,
    pub origin: String,
    pub state: String,
    pub running: bool,
    pub pid: Option<i64>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub summary: String,
    pub labels: Vec<(String, String)>,
}

/// A parsed document. `devices` is none for a sessions document, which reports no device
/// readings; `errors` are `<device>: <code>: <message>` for each device that could not be asked.
#[derive(Clone, Debug)]
pub struct FleetReport {
    pub devices: Option<Vec<FleetDevice>>,
    pub sessions: Vec<FleetSession>,
    pub errors: Vec<String>,
    pub truncated: Vec<String>,
}

type Object = Map<String, Value>;

fn where_of(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

fn not_a(path: &str, key: &str, kind: &str) -> String {
    format!("fleet output: {} is not a {kind}", where_of(path, key))
}

/// A JSON number in the range fleet reports: an integer in 0..2^63, or a finite float of
/// magnitude at most 1e300. A whole number past 2^63 is out of range however it is written:
/// the JSON reader gives a long integer as a float, so the two cannot be told apart.
fn number(value: &Value) -> Option<f64> {
    const INT_LIMIT: f64 = 9_223_372_036_854_775_808.0;
    let Value::Number(number) = value else {
        return None;
    };
    if number.is_i64() || number.is_u64() {
        #[allow(clippy::cast_precision_loss)]
        return number
            .as_i64()
            .filter(|value| *value >= 0)
            .map(|value| value as f64);
    }
    number
        .as_f64()
        .filter(|value| value.is_finite() && value.abs() <= FLOAT_LIMIT)
        .filter(|value| !(value.fract() == 0.0 && value.abs() >= INT_LIMIT))
}

fn field<'d>(entry: &'d Object, key: &str, path: &str) -> Result<&'d Value, String> {
    entry
        .get(key)
        .ok_or_else(|| format!("fleet output: {} is missing", where_of(path, key)))
}

fn string(entry: &Object, key: &str, path: &str) -> Result<String, String> {
    match field(entry, key, path)? {
        Value::String(text) => Ok(text.clone()),
        _ => Err(not_a(path, key, "str")),
    }
}

fn int(entry: &Object, key: &str, path: &str) -> Result<i64, String> {
    field(entry, key, path)?
        .as_i64()
        .ok_or_else(|| not_a(path, key, "int"))
}

fn float(entry: &Object, key: &str, path: &str) -> Result<f64, String> {
    number(field(entry, key, path)?).ok_or_else(|| not_a(path, key, "float"))
}

fn boolean(entry: &Object, key: &str, path: &str) -> Result<bool, String> {
    field(entry, key, path)?
        .as_bool()
        .ok_or_else(|| not_a(path, key, "bool"))
}

fn list<'d>(entry: &'d Object, key: &str, path: &str) -> Result<&'d Vec<Value>, String> {
    field(entry, key, path)?
        .as_array()
        .ok_or_else(|| not_a(path, key, "list"))
}

fn dict<'d>(entry: &'d Object, key: &str, path: &str) -> Result<&'d Object, String> {
    field(entry, key, path)?
        .as_object()
        .ok_or_else(|| not_a(path, key, "dict"))
}

/// A field that may be `null`, read with `read` otherwise.
fn nullable<T>(
    entry: &Object,
    key: &str,
    path: &str,
    read: impl Fn(&Object, &str, &str) -> Result<T, String>,
) -> Result<Option<T>, String> {
    if field(entry, key, path)?.is_null() {
        return Ok(None);
    }
    read(entry, key, path).map(Some)
}

fn object<'d>(value: &'d Value, path: &str) -> Result<&'d Object, String> {
    value
        .as_object()
        .ok_or_else(|| format!("fleet output: {path} is not an object"))
}

fn strings(values: &[Value], place: &str) -> Result<Vec<String>, String> {
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("fleet output: {place} holds a non-string"))
        })
        .collect()
}

/// Parses the stdout of `fleet status --json` or `fleet sessions --json`.
pub fn parse(text: &str) -> Result<FleetReport, String> {
    let document: Value =
        serde_json::from_str(text).map_err(|error| format!("fleet output is not JSON: {error}"))?;
    let document = document
        .as_object()
        .ok_or("fleet output is not a JSON object")?;
    let version = int(document, "version", "")?;
    if version != OUTPUT_VERSION {
        return Err(format!(
            "fleet output version {version}, expected {OUTPUT_VERSION}"
        ));
    }
    int(document, "generated_at", "")?;
    if document.contains_key("devices") {
        let mut devices = Vec::new();
        let mut sessions = Vec::new();
        for (index, entry) in list(document, "devices", "")?.iter().enumerate() {
            let path = format!("devices[{index}]");
            let (device, device_sessions) = parse_device(object(entry, &path)?, &path)?;
            devices.push(device);
            sessions.extend(device_sessions);
        }
        let errors = devices
            .iter()
            .filter_map(|device| Some(format!("{}: {}", device.name, device.error.as_ref()?)))
            .collect();
        let truncated = devices
            .iter()
            .filter(|device| device.sessions_truncated)
            .map(|device| device.name.clone())
            .collect();
        return Ok(FleetReport {
            devices: Some(devices),
            sessions,
            errors,
            truncated,
        });
    }
    let mut sessions = Vec::new();
    for (index, entry) in list(document, "sessions", "")?.iter().enumerate() {
        let path = format!("sessions[{index}]");
        let item = object(entry, &path)?;
        sessions.push(parse_session(
            &string(item, "device", &path)?,
            dict(item, "session", &path)?,
            &format!("{path}.session"),
        )?);
    }
    let mut errors = Vec::new();
    for (index, entry) in list(document, "errors", "")?.iter().enumerate() {
        let path = format!("errors[{index}]");
        let item = object(entry, &path)?;
        errors.push(format!(
            "{}: {}",
            string(item, "device", &path)?,
            parse_error(dict(item, "error", &path)?, &format!("{path}.error"))?
        ));
    }
    let truncated = list(document, "truncated", "")?
        .iter()
        .map(|name| {
            name.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "fleet output: truncated holds a non-string".to_owned())
        })
        .collect::<Result<_, _>>()?;
    Ok(FleetReport {
        devices: None,
        sessions,
        errors,
        truncated,
    })
}

fn parse_device(entry: &Object, path: &str) -> Result<(FleetDevice, Vec<FleetSession>), String> {
    let name = string(entry, "name", path)?;
    let transport = string(entry, "transport", path)?;
    if !["local", "ssh", "tls"].contains(&transport.as_str()) {
        return Err(format!("fleet output: {path}.transport is {transport:?}"));
    }
    let mut sessions = Vec::new();
    for (index, item) in list(entry, "sessions", path)?.iter().enumerate() {
        let session_path = format!("{path}.sessions[{index}]");
        sessions.push(parse_session(
            &name,
            object(item, &session_path)?,
            &session_path,
        )?);
    }
    let sessions_truncated = boolean(entry, "sessions_truncated", path)?;
    if !boolean(entry, "ok", path)? {
        let error = parse_error(dict(entry, "error", path)?, &format!("{path}.error"))?;
        return Ok((
            FleetDevice {
                name,
                transport,
                error: Some(error),
                sessions_truncated,
                ..FleetDevice::default()
            },
            sessions,
        ));
    }
    let status_path = format!("{path}.status");
    let status = dict(entry, "status", path)?;
    let info = dict(status, "device", &status_path)?;
    let metrics_path = format!("{status_path}.metrics");
    let metrics = dict(status, "metrics", &status_path)?;
    let memory = dict(metrics, "memory", &metrics_path)?;
    let listed = list(metrics, "load", &metrics_path)?;
    let load: Vec<f64> = listed.iter().map_while(number).collect();
    if listed.len() != 3 || load.len() != 3 {
        return Err(format!(
            "fleet output: {metrics_path}.load is not three numbers"
        ));
    }
    let mut gpus = Vec::new();
    for (index, item) in list(metrics, "gpus", &metrics_path)?.iter().enumerate() {
        let gpu_path = format!("{metrics_path}.gpus[{index}]");
        let gpu = object(item, &gpu_path)?;
        gpus.push(FleetGpu {
            name: string(gpu, "name", &gpu_path)?,
            util_pct: nullable(gpu, "util_percent", &gpu_path, float)?,
            mem_used_bytes: nullable(gpu, "memory_used_bytes", &gpu_path, int)?,
            mem_total_bytes: nullable(gpu, "memory_total_bytes", &gpu_path, int)?,
        });
    }
    let mut adapters = Vec::new();
    for (index, item) in list(status, "adapters", &status_path)?.iter().enumerate() {
        let adapter_path = format!("{status_path}.adapters[{index}]");
        let adapter = object(item, &adapter_path)?;
        adapters.push(FleetAdapter {
            name: string(adapter, "name", &adapter_path)?,
            models: strings(
                list(adapter, "models", &adapter_path)?,
                &format!("{adapter_path}.models"),
            )?,
            max_concurrent: int(adapter, "max_concurrent", &adapter_path)?,
            running: int(adapter, "running", &adapter_path)?,
        });
    }
    let device_path = format!("{status_path}.device");
    let memory_path = format!("{metrics_path}.memory");
    Ok((
        FleetDevice {
            name,
            transport,
            error: None,
            hostname: string(info, "hostname", &device_path)?,
            os: string(info, "os", &device_path)?,
            cpus: Some(int(info, "cpus", &device_path)?),
            load,
            cpu_percent: Some(float(metrics, "cpu_percent", &metrics_path)?),
            mem_used_bytes: Some(int(memory, "used_bytes", &memory_path)?),
            mem_total_bytes: Some(int(memory, "total_bytes", &memory_path)?),
            sampled_at: Some(int(metrics, "sampled_at", &metrics_path)?),
            gpus,
            adapters,
            sessions_truncated,
        },
        sessions,
    ))
}

fn parse_session(device: &str, entry: &Object, path: &str) -> Result<FleetSession, String> {
    let origin = string(entry, "origin", path)?;
    if !["dispatched", "discovered"].contains(&origin.as_str()) {
        return Err(format!("fleet output: {path}.origin is {origin:?}"));
    }
    let state_path = format!("{path}.state");
    let state = dict(entry, "state", path)?;
    let kind = string(state, "kind", &state_path)?;
    let state_text = match kind.as_str() {
        "running" | "cancelled" | "timed_out" => kind.clone(),
        "exited" => format!("{kind} {}", int(state, "code", &state_path)?),
        "signalled" => format!("{kind} {}", int(state, "signal", &state_path)?),
        "failed" => format!("{kind} {}", string(state, "message", &state_path)?),
        _ => return Err(format!("fleet output: {state_path}.kind is {kind:?}")),
    };
    let labels = dict(entry, "labels", path)?;
    let mut label_pairs = Vec::new();
    for (key, value) in labels {
        let value = value
            .as_str()
            .ok_or_else(|| format!("fleet output: {path}.labels holds a non-string"))?;
        label_pairs.push((key.clone(), value.to_owned()));
    }
    label_pairs.sort();
    // The issue label becomes a number: only a short ASCII one is taken.
    if let Some((_, issue)) = label_pairs.iter().find(|(key, _)| key == "issue")
        && !((1..=9).contains(&issue.len()) && issue.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(format!(
            "fleet output: {path}.labels.issue is not an issue number"
        ));
    }
    Ok(FleetSession {
        device: device.to_owned(),
        id: string(entry, "id", path)?,
        adapter: string(entry, "adapter", path)?,
        origin,
        running: kind == "running",
        state: state_text,
        pid: nullable(entry, "pid", path, int)?,
        started_at: int(entry, "started_at", path)?,
        ended_at: nullable(entry, "ended_at", path, int)?,
        cwd: nullable(entry, "cwd", path, string)?,
        model: nullable(entry, "model", path, string)?,
        summary: string(entry, "summary", path)?,
        labels: label_pairs,
    })
}

fn parse_error(error: &Object, path: &str) -> Result<String, String> {
    Ok(format!(
        "{}: {}",
        string(error, "code", path)?,
        string(error, "message", path)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::testkit::fleet_fixture_text;

    #[test]
    fn parses_the_status_document() {
        let report = parse(&fleet_fixture_text("status.json")).expect("parses");
        let devices = report.devices.as_ref().expect("devices");
        let summary: Vec<(&str, &str, Option<&str>)> = devices
            .iter()
            .map(|device| {
                (
                    device.name.as_str(),
                    device.transport.as_str(),
                    device.error.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("dev-a", "tls", None),
                ("dev-b", "ssh", Some("unreachable: ssh exited 255"))
            ]
        );
        let dev_a = &devices[0];
        assert_eq!(
            (
                dev_a.cpus,
                dev_a.load[0],
                dev_a.mem_used_bytes,
                dev_a.gpus[0].util_pct
            ),
            (Some(16), 4.0, Some(17_179_869_184), Some(50.0))
        );
        let adapters: Vec<(&str, i64, i64)> = dev_a
            .adapters
            .iter()
            .map(|adapter| {
                (
                    adapter.name.as_str(),
                    adapter.running,
                    adapter.max_concurrent,
                )
            })
            .collect();
        assert_eq!(adapters, [("claude", 1, 2), ("aider", 0, 0)]);
        let states: Vec<(&str, Option<i64>)> = report
            .sessions
            .iter()
            .map(|session| (session.state.as_str(), session.pid))
            .collect();
        assert_eq!(
            states,
            [
                ("running", Some(4242)),
                ("running", Some(5151)),
                ("exited 3", None)
            ]
        );
        let labels: Vec<usize> = report
            .sessions
            .iter()
            .map(|session| session.labels.len())
            .collect();
        assert_eq!(labels, [1, 0, 0]);
        assert_eq!(
            report.sessions[0].labels[0],
            ("issue".to_owned(), "42".to_owned())
        );
        assert_eq!(report.errors, ["dev-b: unreachable: ssh exited 255"]);
    }

    #[test]
    fn the_largest_generated_at_reads() {
        let mut document: Value =
            serde_json::from_str(&fleet_fixture_text("status.json")).expect("fixture");
        document["generated_at"] = Value::from(i64::MAX);
        assert!(parse(&document.to_string()).is_ok());
    }
}
