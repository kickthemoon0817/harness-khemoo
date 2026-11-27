//! The fleet wire protocol: every type that crosses a connection between the hub and fleetd,
//! the length-delimited JSON framing, and the hub's machine-readable output.
//!
//! Decoding is validation. Every inbound string with a format is a newtype whose
//! deserializer enforces it, so a [`Request`] that decodes is one a daemon may act on, apart
//! from the device's own policy (adapter allowlists, workdir roots, capacity).
//!
//! `docs/FLEET.md` in the repository is the prose specification of this crate.

#![forbid(unsafe_code)]

mod error;
mod frame;
mod hello;
mod model;
pub mod output;
mod request;
mod response;
mod valid;

pub use error::{ErrorCode, WireError};
pub use frame::{Connection, ProtoError};
pub use hello::{Hello, Op, Role, UnknownRole};
pub use model::{
    AdapterInfo, DeviceInfo, Disk, Gpu, Memory, Metrics, Origin, Session, SessionState,
    StatusReport,
};
pub use request::{DispatchSpec, Request, RequestBody, RequestId};
pub use response::{Response, ResponseBody};
pub use valid::{
    AdapterName, DeviceName, Invalid, LabelKey, Labels, LogLines, ModelName, Prompt, SessionId,
    TimeoutSecs, Workdir,
};

/// The protocol version both peers send in their [`Hello`]. Peers with different versions
/// do not talk: the connection closes after the hellos are exchanged.
pub const PROTOCOL_VERSION: u32 = 1;

/// The largest frame payload either side sends or accepts, in bytes.
pub const MAX_FRAME_LEN: usize = 1024 * 1024;

/// The largest `data` a daemon puts in one [`ResponseBody::Event`], in bytes, so an event
/// frame stays well under [`MAX_FRAME_LEN`] even when every byte needs JSON escaping.
pub const MAX_EVENT_DATA: usize = 64 * 1024;

/// The most sessions one [`ResponseBody::Sessions`] carries. fleetd lists running sessions
/// first, then ended ones newest first, and sets `truncated` when it left any out.
pub const MAX_SESSIONS: usize = 256;

/// The longest a reported path (`Session::cwd`, `Session::exe`, `Disk::mount`) or other
/// free-text string in a report may be, in bytes; fleetd cuts longer ones at a character
/// boundary. With [`MAX_SESSIONS`], this keeps every reply under [`MAX_FRAME_LEN`].
pub const MAX_REPORTED_STR: usize = 1024;
