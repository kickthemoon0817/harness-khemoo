use serde::{Deserialize, Serialize};

use crate::error::{ErrorCode, WireError};
use crate::model::{Session, SessionState, StatusReport};
use crate::valid::SessionId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    /// The id of the request this answers, or 0 for a connection-level error, after which
    /// the server closes the connection.
    pub id: u64,
    pub body: ResponseBody,
}

impl Response {
    pub fn error(id: u64, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            id,
            body: ResponseBody::Error(WireError::new(code, message)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResponseBody {
    Status(Box<StatusReport>),
    Sessions {
        sessions: Vec<Session>,
        /// Set when fleetd left sessions out to stay within
        /// [`MAX_SESSIONS`](crate::MAX_SESSIONS).
        truncated: bool,
    },
    Dispatched {
        session: SessionId,
    },
    Cancelled {
        session: SessionId,
    },
    /// A chunk of a session's combined stdout and stderr, at most
    /// [`MAX_EVENT_DATA`](crate::MAX_EVENT_DATA) bytes. Bytes that are not UTF-8 arrive
    /// as U+FFFD.
    Event {
        data: String,
    },
    /// The last frame of a `logs` or `wait` stream: the session's state when it closed.
    End {
        state: SessionState,
    },
    /// The request failed. For a stream, this frame ends it in place of `end`.
    Error(WireError),
}
