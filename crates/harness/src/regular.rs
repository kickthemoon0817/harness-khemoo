//! Opening a file that must be a regular one: a config or state file that another process could
//! swap for a symlink, a FIFO or a device.

use std::fs::File;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Opens `path` for reading only when it is a regular file. A symlink is refused rather than
/// followed, and the open never blocks: a FIFO would otherwise hang the reader until a writer came.
pub fn open(path: &Path) -> io::Result<File> {
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    #[allow(clippy::cast_possible_wrap)]
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits() as i32)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_regular_file_opens() {
        let dir = std::env::temp_dir().join(format!("regular-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("plain");
        std::fs::write(&plain, "x").unwrap();
        std::os::unix::fs::symlink(&plain, dir.join("link")).unwrap();
        let fifo = dir.join("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        assert!(open(&plain).is_ok());
        assert!(open(&dir.join("link")).is_err());
        assert!(open(&fifo).is_err(), "a FIFO is refused without blocking");
        assert!(open(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
