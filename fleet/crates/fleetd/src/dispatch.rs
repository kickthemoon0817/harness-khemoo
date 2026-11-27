//! Launching, supervising, logging and cancelling dispatched sessions.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use fleet_proto::{
    AdapterInfo, AdapterName, DispatchSpec, ErrorCode, Origin, Session, SessionId, SessionState,
    WireError,
};
use rustix::process::{Pid, Signal};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::config::{Config, Launch};
use crate::discovery::summarize;
use crate::{ensure_private_dir, report_path, unix_now};

/// The most ended sessions kept, beyond which the oldest are deleted before their
/// retention runs out, so a busy device's state stays bounded.
const MAX_RETAINED: usize = 1024;
const TRUNCATED: &[u8] = b"[fleetd: log truncated]\n";
const RESTARTED: &str = "fleetd restarted while the session ran";
const SESSION_FILE: &str = "session.json";
const LOG_FILE: &str = "output.log";
/// A session record is a few KiB; anything larger is not one fleetd wrote.
const MAX_SESSION_FILE: u64 = 1024 * 1024;
const GROUP_POLL: Duration = Duration::from_millis(100);

/// fleetd's dispatched sessions, running and ended.
pub struct Sessions {
    cfg: Arc<Config>,
    root: PathBuf,
    table: Mutex<HashMap<SessionId, Entry>>,
    /// Held across a capacity check and the spawn it allows, so two dispatches cannot both
    /// take the last slot.
    launching: tokio::sync::Mutex<()>,
}

struct Entry {
    session: Session,
    dir: PathBuf,
    /// Bumped on every log write and state change; log followers wait on it.
    progress: Arc<watch::Sender<u64>>,
    /// Set while the process runs.
    run: Option<Run>,
}

struct Run {
    pgid: Pid,
    stop: Arc<Notify>,
    ending: Option<Ending>,
}

#[derive(Debug, Clone, Copy)]
enum Ending {
    Cancelled,
    TimedOut,
}

/// What a log reader needs: the file, and a receiver that changes when it grows or the
/// session ends.
pub(crate) struct LogHandle {
    pub path: PathBuf,
    pub progress: watch::Receiver<u64>,
}

impl Sessions {
    /// Loads the sessions recorded under `state_dir`. One recorded as running cannot be
    /// supervised again, so it becomes `failed`; its processes are left alone.
    pub fn open(cfg: Arc<Config>) -> io::Result<Arc<Self>> {
        ensure_private_dir(&cfg.state_dir)?;
        let root = cfg.state_dir.join("sessions");
        ensure_private_dir(&root)?;
        let now = unix_now();
        let mut table = HashMap::new();
        for dirent in std::fs::read_dir(&root)? {
            let dir = dirent?.path();
            let Some(id) = dir
                .file_name()
                .and_then(OsStr::to_str)
                .and_then(|n| n.parse::<SessionId>().ok())
            else {
                continue;
            };
            let mut session = match read_record(&dir) {
                Ok(s) if s.id == id && s.origin == Origin::Dispatched => s,
                Ok(_) | Err(_) => {
                    tracing::warn!(dir = %dir.display(), "skipping an unreadable session record");
                    continue;
                }
            };
            if session.state.is_running() {
                session.state = SessionState::Failed {
                    message: RESTARTED.to_owned(),
                };
                session.ended_at = Some(now);
                session.pid = None;
                write_record(&dir, &session);
            }
            table.insert(
                id,
                Entry {
                    session,
                    dir,
                    progress: Arc::new(watch::channel(0).0),
                    run: None,
                },
            );
        }
        let sessions = Arc::new(Self {
            cfg,
            root,
            table: Mutex::new(table),
            launching: tokio::sync::Mutex::new(()),
        });
        sessions.prune(now);
        Ok(sessions)
    }

    fn table(&self) -> MutexGuard<'_, HashMap<SessionId, Entry>> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Running sessions and those that ended within `session_retention_s`, including ones
    /// the next prune will delete.
    pub fn list(&self) -> Vec<Session> {
        let cutoff = unix_now().saturating_sub(self.cfg.session_retention.as_secs());
        self.table()
            .values()
            .filter(|e| e.session.ended_at.is_none_or(|t| t > cutoff))
            .map(|e| e.session.clone())
            .collect()
    }

    pub fn contains(&self, id: &SessionId) -> bool {
        self.table().contains_key(id)
    }

    pub fn state(&self, id: &SessionId) -> Option<SessionState> {
        self.table().get(id).map(|e| e.session.state.clone())
    }

    /// The process groups of running sessions, which discovery leaves out.
    pub fn running_groups(&self) -> HashSet<i32> {
        self.table()
            .values()
            .filter_map(|e| e.run.as_ref())
            .map(|r| r.pgid.as_raw_nonzero().get())
            .collect()
    }

    pub(crate) fn log(&self, id: &SessionId) -> Option<LogHandle> {
        self.table().get(id).map(|e| LogHandle {
            path: e.dir.join(LOG_FILE),
            progress: e.progress.subscribe(),
        })
    }

    pub fn adapter_infos(&self) -> Vec<AdapterInfo> {
        let table = self.table();
        self.cfg
            .adapters
            .iter()
            .map(|a| AdapterInfo {
                name: a.name.clone(),
                models: a.models.clone(),
                max_concurrent: a.launch.as_ref().map_or(0, |l| l.max_concurrent),
                running: running_of(&table, &a.name),
            })
            .collect()
    }

    /// Starts a session, or says why the device's policy refuses it. Returns once the
    /// process has started.
    pub async fn dispatch(self: &Arc<Self>, spec: DispatchSpec) -> Result<SessionId, WireError> {
        let adapter = self
            .cfg
            .adapter(&spec.adapter)
            .ok_or_else(|| WireError::new(ErrorCode::NotFound, "no such adapter"))?;
        let launch = adapter
            .launch
            .as_ref()
            .ok_or_else(|| WireError::new(ErrorCode::Forbidden, "the adapter is discovery-only"))?;
        let model = match spec.model {
            Some(m) if adapter.models.contains(&m) => Some(m),
            Some(_) => {
                return Err(WireError::new(
                    ErrorCode::Forbidden,
                    "the model is not in the adapter's allowlist",
                ));
            }
            None => adapter.models.first().cloned(),
        };
        let workdir = launch.resolve_workdir(spec.workdir.as_path())?;
        let timeout = spec.timeout_s.map_or(launch.timeout, |t| {
            launch.timeout.min(Duration::from_secs(t.get().into()))
        });
        let argv = launch.render(model.as_ref(), &workdir);

        let _slot = self.launching.lock().await;
        if running_of(&self.table(), &adapter.name) >= launch.max_concurrent {
            return Err(WireError::new(
                ErrorCode::Busy,
                "the adapter is at max_concurrent",
            ));
        }
        let now = unix_now();
        let (id, dir) = self.create_dir(now).map_err(|e| {
            tracing::error!(error = %e, "could not create a session directory");
            WireError::new(
                ErrorCode::Internal,
                "could not create the session directory",
            )
        })?;
        let mut session = Session {
            id: id.clone(),
            adapter: adapter.name.clone(),
            origin: Origin::Dispatched,
            state: SessionState::Running {},
            pid: None,
            started_at: now,
            ended_at: None,
            cwd: Some(report_path(&workdir)),
            exe: Some(report_path(&launch.exe)),
            model: model.clone(),
            summary: summarize(
                launch.exe.file_name().unwrap_or_default(),
                argv.iter().map(OsString::as_os_str),
            ),
            labels: spec.labels,
        };
        let progress = Arc::new(watch::channel(0).0);
        let spawned = create_log(&dir).and_then(|log| Ok((log, spawn(launch, &argv, &workdir)?)));
        let (log, (mut child, pgid)) = match spawned {
            Ok(ok) => ok,
            Err(e) => {
                tracing::warn!(session = %id, error = %e, "could not start the agent");
                session.state = SessionState::Failed {
                    message: format!("could not start the agent: {e}"),
                };
                session.ended_at = Some(now);
                write_record(&dir, &session);
                self.insert(session, dir, progress, None);
                return Err(WireError::new(
                    ErrorCode::Internal,
                    format!("could not start the agent: {e}"),
                ));
            }
        };
        apply_rlimits(pgid, launch.rlimit_nofile);
        if let Some(mut stdin) = child.stdin.take() {
            let prompt = spec.prompt;
            // Its own task, so an agent that never reads stdin cannot hold up the reply.
            tokio::spawn(async move {
                let _ = stdin.write_all(prompt.as_str().as_bytes()).await;
            });
        }
        let drain = tokio::spawn(drain(
            child.stdout.take(),
            child.stderr.take(),
            LogWriter::new(log, self.cfg.log_max_bytes, Arc::clone(&progress)),
        ));
        session.pid = u32::try_from(pgid.as_raw_nonzero().get()).ok();
        write_record(&dir, &session);
        let stop = Arc::new(Notify::new());
        self.insert(
            session,
            dir,
            progress,
            Some(Run {
                pgid,
                stop: Arc::clone(&stop),
                ending: None,
            }),
        );
        tokio::spawn(Arc::clone(self).supervise(id.clone(), child, pgid, stop, timeout, drain));
        Ok(id)
    }

    /// Cancels a running session: SIGTERM to its process group now, SIGKILL after
    /// `kill_grace_s`. An ended session is left as it is. `false` if there is no such
    /// session.
    pub fn cancel(&self, id: &SessionId) -> bool {
        self.stop(id, Ending::Cancelled)
    }

    fn stop(&self, id: &SessionId, why: Ending) -> bool {
        let mut table = self.table();
        let Some(entry) = table.get_mut(id) else {
            return false;
        };
        if let Some(run) = &mut entry.run
            && run.ending.is_none()
        {
            run.ending = Some(why);
            if let Err(e) = rustix::process::kill_process_group(run.pgid, Signal::TERM) {
                tracing::debug!(session = %id, error = %e, "SIGTERM to the group failed");
            }
            run.stop.notify_one();
        }
        true
    }

    fn insert(
        &self,
        session: Session,
        dir: PathBuf,
        progress: Arc<watch::Sender<u64>>,
        run: Option<Run>,
    ) {
        self.table().insert(
            session.id.clone(),
            Entry {
                session,
                dir,
                progress,
                run,
            },
        );
    }

    fn create_dir(&self, now: u64) -> io::Result<(SessionId, PathBuf)> {
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        loop {
            let id: SessionId = format!("d-{now}-{:08x}", random_u32())
                .parse()
                .map_err(io::Error::other)?;
            let dir = self.root.join(id.as_str());
            match builder.create(&dir) {
                Ok(()) => return Ok((id, dir)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
    }

    async fn supervise(
        self: Arc<Self>,
        id: SessionId,
        mut child: Child,
        pgid: Pid,
        stop: Arc<Notify>,
        timeout: Duration,
        mut drain: JoinHandle<()>,
    ) {
        let (status, on_its_own) = tokio::select! {
            status = child.wait() => (status, true),
            () = stop.notified() => (self.reap(&mut child, pgid).await, false),
            () = tokio::time::sleep(timeout) => {
                self.stop(&id, Ending::TimedOut);
                (self.reap(&mut child, pgid).await, false)
            }
        };
        // From here a cancel finds no process to signal.
        let ending = self
            .table()
            .get_mut(&id)
            .and_then(|e| e.run.take())
            .and_then(|r| r.ending);
        if on_its_own && group_alive(pgid) {
            // Helpers the agent left behind would otherwise run on with no timeout and no
            // way to cancel them.
            let _ = rustix::process::kill_process_group(pgid, Signal::TERM);
            clear_group(pgid, Instant::now() + self.cfg.kill_grace).await;
        }
        // Output still buffered in the pipes belongs in the log, but a process that left
        // the group could hold them open indefinitely.
        if tokio::time::timeout(self.cfg.kill_grace, &mut drain)
            .await
            .is_err()
        {
            drain.abort();
        }
        // A cancel racing a natural exit does not relabel it.
        let state = match ending {
            _ if on_its_own => exit_state(status),
            Some(Ending::Cancelled) => SessionState::Cancelled {},
            Some(Ending::TimedOut) => SessionState::TimedOut {},
            None => exit_state(status),
        };
        tracing::info!(session = %id, state = ?state, "session ended");
        self.finish(&id, state).await;
    }

    /// Waits for a signalled group to go: SIGKILL once `kill_grace_s` has passed, or as soon
    /// as the leader has exited and the rest of the group has had the same grace.
    async fn reap(&self, child: &mut Child, pgid: Pid) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + self.cfg.kill_grace;
        if let Ok(status) = tokio::time::timeout_at(deadline, child.wait()).await {
            clear_group(pgid, deadline).await;
            status
        } else {
            let _ = rustix::process::kill_process_group(pgid, Signal::KILL);
            child.wait().await
        }
    }

    async fn finish(self: &Arc<Self>, id: &SessionId, state: SessionState) {
        let now = unix_now();
        let record = {
            let mut table = self.table();
            table.get_mut(id).map(|e| {
                e.session.state = state;
                e.session.ended_at = Some(now);
                e.session.pid = None;
                e.progress.send_modify(|g| *g = g.wrapping_add(1));
                (e.dir.clone(), e.session.clone())
            })
        };
        // Off the lock and off the async workers: a sync or a large log's deletion can be slow.
        let this = Arc::clone(self);
        let _ = tokio::task::spawn_blocking(move || {
            if let Some((dir, session)) = record {
                write_record(&dir, &session);
            }
            this.prune(now);
        })
        .await;
    }

    /// Deletes ended sessions past `session_retention_s`, and the oldest beyond
    /// [`MAX_RETAINED`].
    pub fn prune(&self, now: u64) {
        let retention = self.cfg.session_retention.as_secs();
        let doomed: Vec<PathBuf> = {
            let mut table = self.table();
            let mut ended: Vec<(u64, SessionId)> = table
                .iter()
                .filter_map(|(id, e)| e.session.ended_at.map(|t| (t, id.clone())))
                .collect();
            ended.sort_unstable_by(|a, b| b.cmp(a));
            ended
                .into_iter()
                .enumerate()
                .filter(|(i, (at, _))| *i >= MAX_RETAINED || at.saturating_add(retention) <= now)
                .filter_map(|(_, (_, id))| table.remove(&id))
                .map(|e| e.dir)
                .collect()
        };
        for dir in doomed {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                tracing::warn!(dir = %dir.display(), error = %e, "could not delete a session");
            }
        }
    }
}

fn running_of(table: &HashMap<SessionId, Entry>, adapter: &AdapterName) -> u32 {
    let n = table
        .values()
        .filter(|e| e.run.is_some() && &e.session.adapter == adapter)
        .count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Starts the agent in its own process group, with a cleared environment, in the
/// canonical workdir, with stdin, stdout and stderr piped.
fn spawn(launch: &Launch, argv: &[OsString], workdir: &Path) -> io::Result<(Child, Pid)> {
    let mut cmd = Command::new(&launch.exe);
    cmd.args(argv)
        .current_dir(workdir)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for name in ["HOME", "PATH"]
        .into_iter()
        .chain(launch.env_allow.iter().map(String::as_str))
    {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    cmd.envs(&launch.env);
    let child = cmd.spawn()?;
    let pgid = child
        .id()
        .and_then(|p| i32::try_from(p).ok())
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::other("the agent exited before it could be tracked"))?;
    Ok((child, pgid))
}

#[cfg(target_os = "linux")]
fn apply_rlimits(pid: Pid, nofile: u64) {
    use rustix::process::{Resource, Rlimit, prlimit};
    let set = |resource, n| {
        let limit = Rlimit {
            current: Some(n),
            maximum: Some(n),
        };
        if let Err(e) = prlimit(Some(pid), resource, limit) {
            tracing::warn!(?resource, error = %e, "could not set the agent's rlimit");
        }
    };
    set(Resource::Core, 0);
    set(Resource::Nofile, nofile);
}

#[cfg(not(target_os = "linux"))]
fn apply_rlimits(_pid: Pid, _nofile: u64) {}

/// Gives the rest of a group until `deadline` to exit, then SIGKILLs what remains. Only
/// called once the leader is reaped: while any member lives the group id cannot be reused.
async fn clear_group(pgid: Pid, deadline: Instant) {
    while Instant::now() < deadline && group_alive(pgid) {
        tokio::time::sleep(GROUP_POLL).await;
    }
    if group_alive(pgid) {
        let _ = rustix::process::kill_process_group(pgid, Signal::KILL);
    }
}

fn group_alive(pgid: Pid) -> bool {
    !matches!(
        rustix::process::test_kill_process_group(pgid),
        Err(rustix::io::Errno::SRCH)
    )
}

fn exit_state(status: io::Result<ExitStatus>) -> SessionState {
    match status {
        Ok(s) => match (s.code(), s.signal()) {
            (Some(code), _) => SessionState::Exited { code },
            (None, Some(signal)) => SessionState::Signalled { signal },
            (None, None) => SessionState::Failed {
                message: format!("unrecognised exit status {s}"),
            },
        },
        Err(e) => SessionState::Failed {
            message: format!("lost track of the agent: {e}"),
        },
    }
}

/// A session id needs uniqueness, not secrecy: collisions are retried, and a session id
/// grants nothing a role does not.
fn random_u32() -> u32 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    #[allow(clippy::cast_possible_truncation)]
    let r = h.finish() as u32;
    r
}

fn create_log(dir: &Path) -> io::Result<tokio::fs::File> {
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(LOG_FILE))?;
    Ok(tokio::fs::File::from_std(file))
}

fn read_record(dir: &Path) -> io::Result<Session> {
    let file = std::fs::File::open(dir.join(SESSION_FILE))?;
    let mut buf = Vec::new();
    file.take(MAX_SESSION_FILE + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_SESSION_FILE {
        return Err(io::Error::other("session record too large"));
    }
    serde_json::from_slice(&buf).map_err(io::Error::other)
}

/// Replaces the record atomically, so a crash leaves the old one or the new one.
fn write_record(dir: &Path, session: &Session) {
    let result = (|| {
        let tmp = dir.join("session.json.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec(session).map_err(io::Error::other)?)?;
        file.sync_data()?;
        std::fs::rename(&tmp, dir.join(SESSION_FILE))
    })();
    if let Err(e) = result {
        tracing::warn!(session = %session.id, error = %e, "could not write the session record");
    }
}

/// Appends output to the session log up to the cap, then marks it truncated and drops the
/// rest, so the agent never blocks on a full pipe.
struct LogWriter {
    file: tokio::fs::File,
    written: u64,
    cap: u64,
    ends_in_newline: bool,
    full: bool,
    progress: Arc<watch::Sender<u64>>,
}

impl LogWriter {
    fn new(file: tokio::fs::File, cap: u64, progress: Arc<watch::Sender<u64>>) -> Self {
        Self {
            file,
            written: 0,
            cap,
            ends_in_newline: true,
            full: false,
            progress,
        }
    }

    async fn write(&mut self, chunk: &[u8]) {
        if self.full || chunk.is_empty() {
            return;
        }
        let room = usize::try_from(self.cap - self.written).unwrap_or(usize::MAX);
        let keep = &chunk[..chunk.len().min(room)];
        if let Err(e) = self.append(keep, keep.len() < chunk.len()).await {
            tracing::warn!(error = %e, "could not write the session log; dropping the rest");
            self.full = true;
        }
        self.progress.send_modify(|g| *g = g.wrapping_add(1));
    }

    async fn append(&mut self, keep: &[u8], overflow: bool) -> io::Result<()> {
        self.file.write_all(keep).await?;
        self.written += keep.len() as u64;
        if let Some(&last) = keep.last() {
            self.ends_in_newline = last == b'\n';
        }
        if overflow {
            self.full = true;
            if !self.ends_in_newline {
                self.file.write_all(b"\n").await?;
            }
            self.file.write_all(TRUNCATED).await?;
        }
        self.file.flush().await
    }
}

async fn drain(
    stdout: Option<impl AsyncRead + Unpin>,
    stderr: Option<impl AsyncRead + Unpin>,
    log: LogWriter,
) {
    let log = tokio::sync::Mutex::new(log);
    tokio::join!(pump(stdout, &log), pump(stderr, &log));
}

async fn pump(pipe: Option<impl AsyncRead + Unpin>, log: &tokio::sync::Mutex<LogWriter>) {
    let Some(mut pipe) = pipe else { return };
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => log.lock().await.write(&buf[..n]).await,
        }
    }
}
