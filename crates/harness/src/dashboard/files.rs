//! The dashboard's file reads and writes.
//!
//! Every read opens the file non-blocking and checks on the handle that it is a regular file, so
//! a FIFO or a device planted where a state or lock file belongs can neither hang a read nor feed
//! it without end; and every read is bounded.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rustix::fs::OFlags;

/// The most any whole-file read takes; a state file past it is refused.
pub const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

fn open_regular(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OFlags::NONBLOCK.bits().cast_signed())
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(file)
}

/// A whole file's text, invalid UTF-8 replaced; at most `limit` bytes.
pub fn read_text_limited(path: &Path, limit: u64) -> io::Result<String> {
    let file = open_regular(path)?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is over {limit} bytes", path.display()),
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// A whole state file's text, invalid UTF-8 replaced.
pub fn read_text(path: &Path) -> io::Result<String> {
    read_text_limited(path, MAX_FILE_BYTES)
}

/// A state file's text, or an empty string when it is missing or unreadable.
pub fn read_text_or_empty(path: &Path) -> String {
    read_text(path).unwrap_or_default()
}

/// The last `bytes` of a file from its first whole line, or an empty string when it is missing.
pub fn read_tail(path: &Path, bytes: u64) -> String {
    let Ok(mut file) = open_regular(path) else {
        return String::new();
    };
    let Ok(size) = file.seek(SeekFrom::End(0)) else {
        return String::new();
    };
    if file
        .seek(SeekFrom::Start(size.saturating_sub(bytes)))
        .is_err()
    {
        return String::new();
    }
    let mut data = Vec::new();
    if file.take(bytes).read_to_end(&mut data).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&data).into_owned();
    if data.len() as u64 >= bytes
        && let Some((_, rest)) = text.split_once('\n')
    {
        return rest.to_owned();
    }
    text
}

/// Writes a file only its owner can read, never through a symlink planted at its path.
pub fn write_private(path: &Path, text: &str) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(text.as_bytes())
}

/// Replaces a file of the dashboard's own whole: written beside it, then renamed over it.
pub fn write_replacing(path: &Path, text: &str) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".new");
    let temporary = Path::new(&temporary);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(OFlags::NOFOLLOW.bits().cast_signed())
        .open(temporary)?;
    file.write_all(text.as_bytes())?;
    drop(file);
    std::fs::rename(temporary, path)
}

/// Appends text to a file of the dashboard's own.
pub fn append(path: &Path, text: &str) -> io::Result<()> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .custom_flags(OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)?
        .write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dashboard::testkit::TempDir;

    #[test]
    fn private_file_is_owner_only_and_never_follows_a_symlink() {
        let directory = TempDir::new();
        let path = directory.path().join("dashboard.url");
        std::fs::write(&path, "old").ok();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
        write_private(&path, "http://127.0.0.1:1/?t=x\n").expect("writes");
        let mode = std::fs::metadata(&path)
            .map(|meta| meta.permissions().mode() & 0o777)
            .ok();
        assert_eq!(mode, Some(0o600));
        assert_eq!(
            std::fs::read_to_string(&path).ok().as_deref(),
            Some("http://127.0.0.1:1/?t=x\n")
        );
        let target = directory.path().join("elsewhere");
        std::fs::write(&target, "keep").ok();
        let link = directory.path().join("planted.url");
        std::os::unix::fs::symlink(&target, &link).ok();
        assert!(write_private(&link, "secret").is_err());
        assert_eq!(
            std::fs::read_to_string(&target).ok().as_deref(),
            Some("keep")
        );
    }

    #[test]
    fn a_fifo_or_a_device_is_refused_without_blocking() {
        let directory = TempDir::new();
        let fifo = directory.path().join("claim.1");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|status| status.success());
        if made {
            assert!(read_text(&fifo).is_err());
        }
        assert!(read_text(Path::new("/dev/zero")).is_err());
        assert_eq!(read_tail(Path::new("/dev/zero"), 10), "");
    }

    #[test]
    fn a_tail_starts_at_a_whole_line() {
        let directory = TempDir::new();
        let path = directory.path().join("log");
        std::fs::write(&path, "first line\nsecond\nthird\n").ok();
        assert_eq!(read_tail(&path, 12), "third\n");
        assert_eq!(read_tail(&path, 1000), "first line\nsecond\nthird\n");
    }
}
