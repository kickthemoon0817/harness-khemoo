//! fleetd's integration tests: config validation, the dispatch sandbox, the servers and
//! their auth, against a daemon run in-process or as the built binary.

mod config;
mod dispatch;
mod relay;
mod server;
mod support;
mod tls;
