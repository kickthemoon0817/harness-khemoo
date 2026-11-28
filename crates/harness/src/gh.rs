//! Runs `gh` with an argv, never through a shell, and reads what it prints up to a bound.

use std::io::{self, Read};
use std::process::{Command, ExitStatus, Stdio};

/// What `gh` printed on stdout, at most this much. A hundred issue bodies, each at GitHub's
/// limit, fit.
pub const STDOUT_LIMIT: u64 = 64 << 20;
/// What is kept of `gh`'s stderr for a message.
const STDERR_LIMIT: u64 = 64 << 10;

/// Where `gh`'s stderr goes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stderr {
    /// Kept for the caller's message.
    Capture,
    /// Dropped.
    Discard,
    /// Passed through to this process's stderr.
    Inherit,
}

pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Runs `gh` with `args`. Fails when it cannot be started or prints more than [`STDOUT_LIMIT`].
pub fn run(args: &[&str], stderr: Stderr) -> io::Result<Output> {
    let mut child = Command::new("gh")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(match stderr {
            Stderr::Capture => Stdio::piped(),
            Stderr::Discard => Stdio::null(),
            Stderr::Inherit => Stdio::inherit(),
        })
        .spawn()?;
    // Drained on its own thread: a full stderr pipe would stall gh while stdout is read.
    let reader = child.stderr.take().map(|pipe| {
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut pipe = pipe;
            let _ = (&mut pipe).take(STDERR_LIMIT).read_to_end(&mut kept);
            let _ = io::copy(&mut pipe, &mut io::sink());
            String::from_utf8_lossy(&kept).trim().to_owned()
        })
    });
    let mut stdout = Vec::new();
    let read = match child.stdout.take() {
        Some(pipe) => pipe
            .take(STDOUT_LIMIT + 1)
            .read_to_end(&mut stdout)
            .map(|_| ()),
        None => Ok(()),
    };
    if read.is_err() || stdout.len() as u64 > STDOUT_LIMIT {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let stderr = reader
        .and_then(|reader| reader.join().ok())
        .unwrap_or_default();
    read?;
    if stdout.len() as u64 > STDOUT_LIMIT {
        return Err(io::Error::other(format!(
            "gh printed more than {STDOUT_LIMIT} bytes"
        )));
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
