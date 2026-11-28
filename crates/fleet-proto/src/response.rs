use serde::{Deserialize, Serialize};

use crate::error::{ErrorCode, WireError};
use crate::model::{Session, SessionState, StatusReport};
use crate::valid::SessionId;
use crate::{MAX_FRAME_LEN, MAX_SESSIONS};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    /// The id of the request this answers, or 0 for a connection-level error, after which
    /// the server closes the connection.
    pub id: u64,
    pub body: ResponseBody,
}

impl ResponseBody {
    /// A `sessions` reply holding the longest prefix of `sessions` that stays within
    /// [`MAX_SESSIONS`] and, measured as serialized, within one frame for any request id.
    /// A count alone cannot bound the size: one session's labels and paths, all escaped,
    /// run to tens of KiB.
    pub fn sessions(mut sessions: Vec<Session>) -> Self {
        let empty = Response {
            id: u64::MAX,
            body: Self::Sessions {
                sessions: Vec::new(),
                truncated: false,
            },
        };
        let mut used = json_len(&empty);
        let mut keep = 0;
        for session in sessions.iter().take(MAX_SESSIONS) {
            // One more byte for the separating comma.
            let len = json_len(session).saturating_add(1);
            if used.saturating_add(len) > MAX_FRAME_LEN {
                break;
            }
            used += len;
            keep += 1;
        }
        let truncated = keep < sessions.len();
        sessions.truncate(keep);
        Self::Sessions {
            sessions,
            truncated,
        }
    }
}

/// The serialized size; a value that cannot be serialized never fits.
fn json_len<T: Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |v| v.len())
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
        /// [`MAX_SESSIONS`](crate::MAX_SESSIONS) or one frame.
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::MAX_REPORTED_STR;
    use crate::model::Origin;
    use crate::valid::{AdapterName, Labels, ModelName};

    /// The largest session a device can report: every name at its longest, every free
    /// string at [`MAX_REPORTED_STR`] of a character JSON escapes as six bytes, and every
    /// label value at its longest of `"`, the worst a label may hold.
    fn worst_session(n: usize) -> Session {
        let name =
            |prefix: &str, len: usize| format!("{prefix}{n:0>width$}", width = len - prefix.len());
        let free = "\u{1}".repeat(MAX_REPORTED_STR);
        let labels: BTreeMap<String, String> = (0..Labels::MAX_ENTRIES)
            .map(|i| {
                (
                    name(&format!("k{i}-"), 64),
                    "\"".repeat(Labels::MAX_VALUE_LEN),
                )
            })
            .collect();
        Session {
            id: name("d-", 64).parse().unwrap(),
            adapter: name("a", 32).parse::<AdapterName>().unwrap(),
            origin: Origin::Dispatched,
            state: SessionState::Failed {
                message: free.clone(),
            },
            pid: Some(u32::MAX),
            started_at: u64::MAX,
            ended_at: Some(u64::MAX),
            cwd: Some(free.clone()),
            exe: Some(free.clone()),
            model: Some(name("m", 128).parse::<ModelName>().unwrap()),
            summary: free,
            labels: Labels::try_from(labels).unwrap(),
        }
    }

    #[test]
    fn the_worst_sessions_reply_fits_one_frame_and_says_it_is_truncated() {
        let all: Vec<Session> = (0..MAX_SESSIONS).map(worst_session).collect();
        let body = ResponseBody::sessions(all);
        let ResponseBody::Sessions {
            sessions,
            truncated,
        } = &body
        else {
            unreachable!()
        };
        assert!(*truncated);
        assert!(!sessions.is_empty());
        let reply = Response { id: u64::MAX, body };
        assert!(json_len(&reply) <= MAX_FRAME_LEN, "{}", json_len(&reply));
    }

    #[test]
    fn a_sessions_reply_is_cut_at_max_sessions() {
        let small = Session {
            state: SessionState::Running {},
            cwd: None,
            exe: None,
            summary: "agent".into(),
            labels: Labels::default(),
            ..worst_session(0)
        };
        let body = ResponseBody::sessions(vec![small.clone(); MAX_SESSIONS]);
        assert!(
            matches!(&body, ResponseBody::Sessions { sessions, truncated: false } if sessions.len() == MAX_SESSIONS)
        );
        let body = ResponseBody::sessions(vec![small; MAX_SESSIONS + 1]);
        assert!(
            matches!(&body, ResponseBody::Sessions { sessions, truncated: true } if sessions.len() == MAX_SESSIONS)
        );
    }
}
