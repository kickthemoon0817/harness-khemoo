//! Runs the dashboard's external commands: always an argv, never through a shell, stdin closed,
//! with a timeout and a bound on the output kept.
//!
//! Each command runs in a process group of its own, so a timeout or an output past its bound
//! stops whatever it started too, and a grandchild holding its pipes open cannot hang the read.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::text::{basename, float_repr};

/// The most stdout a command may print before it is stopped.
pub const MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
/// How much of the end of stderr is kept, for an error message.
const STDERR_TAIL_BYTES: usize = 4096;
/// How long a stopped command's pipes may stay open before the read gives up.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// A finished command: its exit status (a signal reads as its negative number), stdout, and the
/// end of stderr.
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

/// Why a command gave no output.
#[derive(Debug)]
pub enum RunError {
    /// It could not start.
    Spawn { program: String, error: io::Error },
    /// It ran past its timeout.
    Timeout { program: String, timeout: Duration },
    /// Its stdout passed the bound.
    TooLarge(usize),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn { program, error } => {
                let kind = match error.kind() {
                    io::ErrorKind::NotFound => "FileNotFoundError",
                    io::ErrorKind::PermissionDenied => "PermissionError",
                    _ => "OSError",
                };
                write!(formatter, "{kind}: {error}: '{program}'")
            }
            Self::Timeout { program, timeout } => write!(
                formatter,
                "TimeoutExpired: Command '{program}' timed out after {} seconds",
                float_repr(timeout.as_secs_f64())
            ),
            Self::TooLarge(bound) => write!(formatter, "RuntimeError: output passed {bound} bytes"),
        }
    }
}

/// The environment a command gets.
pub enum Environment {
    /// The dashboard's own, with these set on top.
    Inherit(Vec<(String, String)>),
    /// Exactly these variables.
    Exactly(Vec<(String, String)>),
}

fn kill_group(child: &Child) {
    if let Ok(pid) = i32::try_from(child.id())
        && let Some(pid) = rustix::process::Pid::from_raw(pid)
    {
        rustix::process::kill_process_group(pid, rustix::process::Signal::KILL).ok();
    }
}

/// Kills the command's group while the command is still unreaped, so the group id is still its.
fn stop(child: &mut Child) {
    kill_group(child);
    child.kill().ok();
    child.wait().ok();
}

fn read_capped(mut pipe: impl Read, limit: usize, overflowed: &AtomicBool) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = vec![0u8; 65536];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => return kept,
            Ok(count) => {
                kept.extend_from_slice(&chunk[..count]);
                if kept.len() > limit {
                    overflowed.store(true, Ordering::SeqCst);
                    return kept;
                }
            }
        }
    }
}

fn read_tail(mut pipe: impl Read) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut chunk = vec![0u8; 65536];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) | Err(_) => return kept,
            Ok(count) => {
                kept.extend_from_slice(&chunk[..count]);
                if kept.len() > STDERR_TAIL_BYTES {
                    kept.drain(..kept.len() - STDERR_TAIL_BYTES);
                }
            }
        }
    }
}

/// Runs `argv` and waits at most `timeout` for it and its output.
pub fn run(
    argv: &[String],
    environment: Environment,
    timeout: Duration,
    max_stdout: usize,
) -> Result<Output, RunError> {
    let program = argv.first().map(String::as_str).unwrap_or_default();
    let name = basename(program).to_owned();
    if program.is_empty() {
        return Err(RunError::Spawn {
            program: name,
            error: io::Error::from(io::ErrorKind::NotFound),
        });
    }
    let mut command = std::process::Command::new(program);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    match environment {
        Environment::Inherit(extra) => {
            command.envs(extra);
        }
        Environment::Exactly(variables) => {
            command.env_clear().envs(variables);
        }
    }
    let deadline = Instant::now() + timeout;
    let mut child = command.spawn().map_err(|error| RunError::Spawn {
        program: name.clone(),
        error,
    })?;
    let overflowed = Arc::new(AtomicBool::new(false));
    let (stdout_sender, stdout_receiver) = mpsc::channel();
    let (stderr_sender, stderr_receiver) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        let overflowed = Arc::clone(&overflowed);
        thread::spawn(move || {
            stdout_sender
                .send(read_capped(stdout, max_stdout, &overflowed))
                .ok()
        });
    }
    if let Some(stderr) = child.stderr.take() {
        thread::spawn(move || stderr_sender.send(read_tail(stderr)).ok());
    }
    let status = loop {
        if overflowed.load(Ordering::SeqCst) {
            stop(&mut child);
            return Err(RunError::TooLarge(max_stdout));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                stop(&mut child);
                return Err(RunError::Spawn {
                    program: name,
                    error,
                });
            }
        }
        if Instant::now() >= deadline {
            stop(&mut child);
            return Err(RunError::Timeout {
                program: name,
                timeout,
            });
        }
        thread::sleep(Duration::from_millis(5));
    };
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(DRAIN_GRACE);
    let mut stragglers = false;
    let mut receive = |receiver: &mpsc::Receiver<Vec<u8>>, wait: Duration| {
        if let Ok(bytes) = receiver.recv_timeout(wait) {
            bytes
        } else {
            // Something the command started still holds the pipe, so its group still exists
            // and its id cannot have been reused.
            if !stragglers {
                stragglers = true;
                kill_group(&child);
            }
            receiver.recv_timeout(DRAIN_GRACE).unwrap_or_default()
        }
    };
    let stdout = receive(&stdout_receiver, remaining);
    if overflowed.load(Ordering::SeqCst) {
        return Err(RunError::TooLarge(max_stdout));
    }
    let stderr = receive(&stderr_receiver, DRAIN_GRACE);
    if stragglers && Instant::now() >= deadline {
        return Err(RunError::Timeout {
            program: name,
            timeout,
        });
    }
    let code = status.code().unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        -status.signal().unwrap_or(0)
    });
    Ok(Output {
        status: code,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn output_and_status_come_back() {
        let output = run(
            &argv(&["sh", "-c", "echo out; echo err >&2; exit 3"]),
            Environment::Inherit(vec![]),
            Duration::from_secs(10),
            1024,
        );
        let output = output.expect("runs");
        assert_eq!(
            (
                output.status,
                output.stdout.as_str(),
                output.stderr.as_str()
            ),
            (3, "out\n", "err\n")
        );
    }

    #[test]
    fn a_hang_stops_at_the_timeout_and_takes_its_children() {
        let started = Instant::now();
        let result = run(
            &argv(&["sh", "-c", "sleep 30 & sleep 30"]),
            Environment::Inherit(vec![]),
            Duration::from_millis(300),
            1024,
        );
        assert!(matches!(result, Err(RunError::Timeout { .. })));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn output_past_the_bound_stops_the_command() {
        let started = Instant::now();
        let result = run(
            &argv(&["yes"]),
            Environment::Inherit(vec![]),
            Duration::from_secs(20),
            100_000,
        );
        assert!(matches!(result, Err(RunError::TooLarge(100_000))));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_missing_program_is_a_spawn_error() {
        let result = run(
            &argv(&["/nonexistent/program"]),
            Environment::Exactly(vec![]),
            Duration::from_secs(1),
            10,
        );
        assert!(result.is_err_and(|error| error.to_string().starts_with("FileNotFoundError")));
    }
}
