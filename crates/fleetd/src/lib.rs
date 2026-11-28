//! fleetd: the device daemon. `docs/FLEET.md` specifies its behaviour.

#![forbid(unsafe_code)]

pub mod config;
pub mod discovery;
pub mod dispatch;
pub mod inventory;
mod logs;
pub mod relay;
pub mod server;
pub mod tls;

use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use fleet_proto::MAX_REPORTED_STR;

/// A device-sourced string made fit to report: control characters become U+FFFD, and it is
/// cut at [`MAX_REPORTED_STR`] on a character boundary.
pub(crate) fn report_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_REPORTED_STR));
    for c in s.chars() {
        let c = if c.is_control() { '\u{FFFD}' } else { c };
        if out.len() + c.len_utf8() > MAX_REPORTED_STR {
            break;
        }
        out.push(c);
    }
    out
}

pub(crate) fn report_path(p: &Path) -> String {
    report_str(&p.to_string_lossy())
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Resolves once shutdown has begun (or its sender is gone).
pub(crate) async fn stopped(shutdown: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

pub(crate) fn own_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// Creates `dir` with mode 0700 if it is missing, and refuses one that is not a real
/// directory owned by fleetd's user without group or other permission bits: anyone else who
/// could enter it could read session logs or reach the socket. Only `dir` itself is
/// checked: whoever can write a parent can replace it, so the parents must be the
/// operator's.
pub(crate) fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            std::fs::symlink_metadata(dir)?
        }
        Err(e) => return Err(e),
    };
    let problem = if !meta.is_dir() {
        Some("is not a directory")
    } else if meta.uid() != own_uid() {
        Some("is owned by another user")
    } else if meta.mode() & 0o077 != 0 {
        Some("has group or other permission bits; it must be mode 0700")
    } else {
        None
    };
    match problem {
        Some(p) => Err(io::Error::other(format!("{}: {p}", dir.display()))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_str_replaces_controls_and_caps_length() {
        assert_eq!(report_str("a\u{1b}[31mb\n"), "a\u{FFFD}[31mb\u{FFFD}");
        let long = "é".repeat(MAX_REPORTED_STR);
        let cut = report_str(&long);
        assert!(cut.len() <= MAX_REPORTED_STR);
        assert_eq!(cut.len() % 2, 0, "cut on a character boundary");
    }
}
