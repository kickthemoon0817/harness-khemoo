use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The first frame each way. The client names the role it asks for; the server answers with
/// the role it grants, which is never more than the client asked for nor more than the
/// transport authenticated.
///
/// Unlike every other inbound type, `Hello` ignores unknown fields: a peer speaking a later
/// version must still be understood well enough to report the version mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub role: Role,
}

/// What a connection may do. `Monitor` reads; `Dispatch` also starts and cancels sessions.
/// The order is meaningful: a role permits everything a lesser role does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Monitor,
    Dispatch,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Monitor => "monitor",
            Self::Dispatch => "dispatch",
        }
    }

    pub fn permits(self, op: Op) -> bool {
        self >= op.required_role()
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Role {
    type Err = UnknownRole;

    fn from_str(s: &str) -> Result<Self, UnknownRole> {
        match s {
            "monitor" => Ok(Self::Monitor),
            "dispatch" => Ok(Self::Dispatch),
            _ => Err(UnknownRole),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("role must be monitor or dispatch")]
pub struct UnknownRole;

/// The kind of a request, without its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    Status,
    Sessions,
    Logs,
    Wait,
    Dispatch,
    Cancel,
}

impl Op {
    pub const ALL: [Op; 6] = [
        Op::Status,
        Op::Sessions,
        Op::Logs,
        Op::Wait,
        Op::Dispatch,
        Op::Cancel,
    ];

    /// The least role that may make this request. A hub asks for exactly this role, so a
    /// read-only command never holds dispatch rights.
    pub fn required_role(self) -> Role {
        match self {
            Op::Status | Op::Sessions | Op::Logs | Op::Wait => Role::Monitor,
            Op::Dispatch | Op::Cancel => Role::Dispatch,
        }
    }
}
