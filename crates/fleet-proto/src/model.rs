//! What a device reports about itself and its agent sessions.

use serde::{Deserialize, Serialize};

use crate::valid::{AdapterName, DeviceName, Labels, ModelName, SessionId};

/// The answer to a `status` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusReport {
    pub device: DeviceInfo,
    pub metrics: Metrics,
    pub adapters: Vec<AdapterInfo>,
}

/// The device's identity, which changes only when its software or configuration does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceInfo {
    /// The `device` key of fleetd.toml.
    pub name: DeviceName,
    pub hostname: String,
    /// The OS name and version, such as `Ubuntu 24.04`.
    pub os: String,
    pub kernel: String,
    /// The CPU architecture, such as `x86_64` or `aarch64`.
    pub arch: String,
    /// Logical CPUs.
    pub cpus: u32,
    pub fleetd_version: String,
}

/// A point-in-time sample.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metrics {
    /// Unix seconds when the sample was taken.
    pub sampled_at: u64,
    pub uptime_s: u64,
    /// The 1, 5 and 15 minute load averages.
    pub load: [f64; 3],
    /// Busy share of all CPUs since the previous sample, 0 to 100.
    pub cpu_percent: f32,
    pub memory: Memory,
    pub swap: Memory,
    pub disks: Vec<Disk>,
    pub gpus: Vec<Gpu>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Memory {
    pub total_bytes: u64,
    pub used_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Disk {
    pub mount: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

/// One GPU as the configured probe reports it. A field the probe cannot read is `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gpu {
    pub index: u32,
    pub name: String,
    pub util_percent: Option<f32>,
    pub memory_total_bytes: Option<u64>,
    pub memory_used_bytes: Option<u64>,
    pub temperature_c: Option<f32>,
}

/// An adapter the device has enabled, and how much of it is in use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterInfo {
    pub name: AdapterName,
    /// The allowlist; the first is the default when a dispatch names no model.
    pub models: Vec<ModelName>,
    pub max_concurrent: u32,
    /// Dispatched sessions of this adapter now running.
    pub running: u32,
}

/// An agent session on a device: one fleetd started, or an agent process it found.
///
/// A session never carries the process's argv or environment. `summary` is fleetd's
/// redacted one-line description. fleetd replaces control characters in every reported
/// string with U+FFFD, but a hub still treats them as untrusted when it displays them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub id: SessionId,
    pub adapter: AdapterName,
    pub origin: Origin,
    pub state: SessionState,
    /// `None` once a dispatched session's process is gone.
    pub pid: Option<u32>,
    /// Unix seconds.
    pub started_at: u64,
    /// Unix seconds; set once the session has ended.
    pub ended_at: Option<u64>,
    /// The canonical working directory, when known.
    pub cwd: Option<String>,
    /// The executable's absolute path, when known.
    pub exe: Option<String>,
    pub model: Option<ModelName>,
    pub summary: String,
    pub labels: Labels,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// fleetd started it in answer to a `dispatch` request. Only these have logs and can be
    /// waited on or cancelled.
    Dispatched,
    /// A process matching an adapter's `match_exe`, started by someone else.
    Discovered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionState {
    // Empty struct variants for the same reason as in `RequestBody`.
    Running {},
    /// The process exited on its own with this code.
    Exited {
        code: i32,
    },
    /// The process died of a signal nobody in fleet sent.
    Signalled {
        signal: i32,
    },
    /// A `cancel` request ended it.
    Cancelled {},
    /// The effective timeout ended it.
    TimedOut {},
    /// fleetd could not start it, or lost track of it across a restart.
    Failed {
        message: String,
    },
}

impl SessionState {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running {})
    }
}
