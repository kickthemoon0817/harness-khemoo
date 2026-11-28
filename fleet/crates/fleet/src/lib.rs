//! fleet: the hub. `docs/FLEET.md` specifies its behaviour.

#![forbid(unsafe_code)]

pub mod cli;
pub mod client;
mod files;
pub mod pki;
pub mod placement;
pub mod query;
pub mod registry;
pub mod render;
pub mod transport;
