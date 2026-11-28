//! The documents `fleet status --json` and `fleet sessions --json` print. They are the hub's
//! contract with scripts and the dashboard, versioned by [`OUTPUT_VERSION`] apart from the
//! wire protocol.

use serde::{Deserialize, Serialize};

use crate::error::WireError;
use crate::model::{Session, StatusReport};
use crate::valid::DeviceName;

/// Bumped when a field is removed or changes meaning. Added fields do not bump it, so
/// scripts reading these documents ignore fields they do not know. The nested wire types
/// decode strictly in Rust; they change only with [`PROTOCOL_VERSION`](crate::PROTOCOL_VERSION).
pub const OUTPUT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    Local,
    Ssh,
    Tls,
}

/// `fleet status --json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusOutput {
    pub version: u32,
    /// Unix seconds.
    pub generated_at: u64,
    /// In registry order.
    pub devices: Vec<DeviceReport>,
}

/// One device in `fleet status`. Exactly one of `error` and `status` is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceReport {
    /// The registry name, the key under `[devices]` in fleet.toml.
    pub name: DeviceName,
    pub transport: TransportKind,
    pub ok: bool,
    pub error: Option<WireError>,
    pub status: Option<StatusReport>,
    /// Empty when `ok` is false.
    pub sessions: Vec<Session>,
    /// Whether fleetd truncated `sessions` at [`MAX_SESSIONS`](crate::MAX_SESSIONS).
    pub sessions_truncated: bool,
}

/// `fleet sessions --json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionsOutput {
    pub version: u32,
    /// Unix seconds.
    pub generated_at: u64,
    /// Ordered by device in registry order, then by `started_at`.
    pub sessions: Vec<DeviceSession>,
    /// The devices that could not be asked.
    pub errors: Vec<DeviceError>,
    /// The devices whose list fleetd truncated at [`MAX_SESSIONS`](crate::MAX_SESSIONS).
    pub truncated: Vec<DeviceName>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceSession {
    pub device: DeviceName,
    pub session: Session,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceError {
    pub device: DeviceName,
    pub error: WireError,
}
