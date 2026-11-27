//! Bounded reads of local files, and the permission rules for private keys.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// fleet.toml and every PEM file are far smaller; the cap keeps a wrong path (a device
/// node, a huge log) from being read whole.
pub const MAX_CONFIG_LEN: u64 = 1024 * 1024;

pub fn read_capped(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    read_open(open_regular(path)?, max)
}

/// Opens a regular file. The type is checked before opening because opening a FIFO blocks
/// until a writer appears; it is checked again on the handle in [`read_open`].
fn open_regular(path: &Path) -> io::Result<File> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    File::open(path)
}

fn read_open(file: File, max: u64) -> io::Result<Vec<u8>> {
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
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
