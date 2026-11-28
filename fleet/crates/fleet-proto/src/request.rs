use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

use crate::hello::Op;
use crate::valid::{
    AdapterName, Labels, LogLines, ModelName, Prompt, SessionId, TimeoutSecs, Workdir,
};

/// A request id, chosen by the client and echoed in every response frame to it. Zero is
/// reserved for errors that belong to no request.
pub type RequestId = NonZeroU64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: RequestId,
    pub body: RequestBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestBody {
    // Empty struct variants rather than unit variants: serde ignores unknown fields beside
    // the tag of a unit variant, even under deny_unknown_fields.
    /// Answered by one `status` frame.
    Status {},
    /// Answered by one `sessions` frame.
    Sessions {},
    /// Answered by one `dispatched` frame once the process has started.
    Dispatch(DispatchSpec),
    /// Answered by `event` frames holding the last `lines` lines of the session's log, then,
    /// with `follow`, the output as it arrives until the session ends; then `end`.
    Logs {
        session: SessionId,
        #[serde(default)]
        lines: LogLines,
        #[serde(default)]
        follow: bool,
    },
    /// Answered by one `cancelled` frame once SIGTERM has been sent to the session's
    /// process group.
    Cancel { session: SessionId },
    /// Answered by `event` frames holding the session's whole log from its start, then its
    /// output as it arrives, then `end` when it has ended.
    Wait { session: SessionId },
}

impl RequestBody {
    pub fn op(&self) -> Op {
        match self {
            Self::Status {} => Op::Status,
            Self::Sessions {} => Op::Sessions,
            Self::Dispatch(_) => Op::Dispatch,
            Self::Logs { .. } => Op::Logs,
            Self::Cancel { .. } => Op::Cancel,
            Self::Wait { .. } => Op::Wait,
        }
    }
}

/// What a dispatcher may choose. Everything else about the child (its executable, argv,
/// environment, limits) is the device operator's configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchSpec {
    pub adapter: AdapterName,
    pub workdir: Workdir,
    #[serde(default)]
    pub model: Option<ModelName>,
    pub prompt: Prompt,
    #[serde(default)]
    pub timeout_s: Option<TimeoutSecs>,
    #[serde(default)]
    pub labels: Labels,
}
