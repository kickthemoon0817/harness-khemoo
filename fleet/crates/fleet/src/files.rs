//! Bounded reads of local files, and the permission rules for private keys.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use rustix::fs::OFlags;

/// fleet.toml and every PEM file are far smaller; the cap keeps a wrong path (a device
/// node, a huge log) from being read whole.
pub const MAX_CONFIG_LEN: u64 = 1024 * 1024;

pub fn read_capped(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    read_open(open_regular(path)?, max)
}

/// Opens a regular file. `O_NONBLOCK` keeps the open of a FIFO from waiting for a writer, so
/// the type can be checked on the handle rather than on a path that may be swapped between a
/// check and the open. It has no effect on reads of a regular file.
fn open_regular(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OFlags::NONBLOCK.bits().cast_signed())
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    Ok(file)
}

fn read_open(file: File, max: u64) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    file.take(max + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        return Err(io::Error::other(format!("larger than {max} bytes")));
    }
    Ok(buf)
}

/// Reads a private key, refusing one that group or others can access. The mode is checked
/// on the open file, so it is the mode of the bytes read.
pub fn read_private_key(path: &Path) -> io::Result<Vec<u8>> {
    let file = open_regular(path)?;
    let mode = file.metadata()?.mode();
    if mode & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "mode {:04o} has group or other permission bits; chmod 600 it",
            mode & 0o7777
        )));
    }
    read_open(file, MAX_CONFIG_LEN)
}

/// Creates `path` with `mode`, failing if anything already exists there.
pub fn create_new(path: &Path, mode: u32) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fifo_is_refused_without_waiting_for_a_writer() {
        let dir = std::env::temp_dir().join(format!("fleet-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("fifo");
        let _ = std::fs::remove_file(&fifo);
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || tx.send(read_capped(&path, MAX_CONFIG_LEN)).unwrap());
        let result = rx.recv_timeout(std::time::Duration::from_secs(5));
        std::fs::remove_dir_all(&dir).unwrap();
        let err = result.expect("opening a FIFO blocked").unwrap_err();
        assert_eq!(err.to_string(), "not a regular file");
    }
}
