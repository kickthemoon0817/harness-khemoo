//! fleetd: the device daemon. `docs/FLEET.md` specifies its behaviour.

#![forbid(unsafe_code)]

pub mod config;
pub mod discovery;
pub mod dispatch;
pub mod inventory;
pub mod server;
