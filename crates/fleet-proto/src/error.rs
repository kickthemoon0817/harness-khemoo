use std::fmt;

use serde::{Deserialize, Serialize};

/// A stable, machine-readable error code. Scripts and the dashboard match on these strings,
/// so a code keeps its meaning once published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The connection's role does not permit the request, or the peer failed authentication.
    Unauthorized,
    /// The device's policy refuses the request: a workdir outside the adapter's roots, a
    /// model not in its allowlist, a discovery-only adapter, or `logs`, `wait` or `cancel`
    /// on a discovered session.
    Forbidden,
    /// The frame did not decode, or a value failed validation.
    BadRequest,
    /// No such adapter or session, or the workdir does not exist.
    NotFound,
    /// The adapter is at `max_concurrent`, or the daemon at its connection limit.
    Busy,
    /// A frame exceeded the 1 MiB cap.
    TooLarge,
    /// The peers speak different protocol versions.
    VersionMismatch,
    /// `fleetd --stdio` found no running daemon to relay to.
    Unavailable,
    /// A fault inside the daemon; the message is for its operator.
    Internal,
    /// The hub could not connect to the device. Hub only.
    Unreachable,
    /// The device did not answer in time. Hub only.
    Timeout,
    /// The device broke the protocol: a bad frame or an out-of-order reply. Hub only.
    Protocol,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::BadRequest => "bad_request",
            Self::NotFound => "not_found",
            Self::Busy => "busy",
            Self::TooLarge => "too_large",
            Self::VersionMismatch => "version_mismatch",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
            Self::Unreachable => "unreachable",
            Self::Timeout => "timeout",
            Self::Protocol => "protocol",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error as it crosses the wire. `message` is human-readable and never contains secrets,
/// prompt text or environment values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(deny_unknown_fields)]
#[error("{code}: {message}")]
pub struct WireError {
    pub code: ErrorCode,
    pub message: String,
}

impl WireError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
