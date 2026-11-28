//! Shared fixtures: temp dirs, fake agents, an in-process daemon, and a protocol client.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use fleet_proto::{Connection, Response, ResponseBody, Role, SessionState};
use fleetd::config::Config;
use fleetd::server::Running;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;

/// A directory removed on drop. Short and canonical: unix socket paths are limited to about
/// 100 bytes, and macOS's temp dir is behind a symlink.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let path = base.join(format!(
            "fd{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, p: &str) -> PathBuf {
        self.0.join(p)
    }

    pub fn mkdir(&self, p: &str) -> PathBuf {
        let path = self.join(p);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Writes a shell script for an adapter to run as `/bin/sh <script>`, so the test never
    /// executes a file it has just written (which can fail with ETXTBSY under parallel forks).
    pub fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The top of a config for `dir`, with a short kill grace so tests run quickly.
pub fn base_config(dir: &TempDir) -> String {
    format!(
        "device = \"dev-a\"\nstate_dir = \"{}\"\nkill_grace_s = 1\n",
        dir.join("state").display()
    )
}

/// An adapter `name` that runs `script` under `/bin/sh` with `args` after it, rooted at
/// `root`. `extra` adds TOML lines to the table.
pub fn adapter(name: &str, script: &Path, args: &[&str], root: &Path, extra: &str) -> String {
    let mut argv = vec![script.display().to_string()];
    argv.extend(args.iter().map(|a| (*a).to_owned()));
    format!(
        "\n[adapters.{name}]\nexe = \"/bin/sh\"\nargs = {}\nworkdir_roots = [\"{}\"]\nmatch_exe = [\"fleetd-test-none\"]\n{extra}\n",
        toml_list(&argv),
        root.display()
    )
}

pub fn toml_list(items: &[String]) -> String {
    serde_json::to_string(items).unwrap()
}

pub async fn start(toml: &str) -> Running {
    let cfg = Config::parse(toml).unwrap();
    let tls = fleetd::tls::load(&cfg).unwrap();
    fleetd::server::start(cfg, tls).await.unwrap()
}

pub async fn connect(running: &Running, role: Role) -> (Connection<UnixStream>, Role) {
    connect_at(running.socket(), role).await
}

pub async fn connect_at(socket: &Path, role: Role) -> (Connection<UnixStream>, Role) {
    let mut conn = Connection::new(UnixStream::connect(socket).await.unwrap());
    let granted = conn.client_hello(role).await.unwrap();
    (conn, granted)
}

/// Sends a request given as JSON and returns the first response frame to it.
pub async fn request<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<T>,
    id: u64,
    body: Value,
) -> Response {
    conn.send(&json!({"id": id, "body": body})).await.unwrap();
    next(conn).await
}

pub async fn next<T: AsyncRead + AsyncWrite + Unpin>(conn: &mut Connection<T>) -> Response {
    tokio::time::timeout(Duration::from_secs(30), conn.recv::<Response>())
        .await
        .expect("no response within 30s")
        .unwrap()
        .expect("connection closed")
}

/// Dispatches and returns the new session id, or the error frame's body.
pub async fn dispatch<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<T>,
    spec: Value,
) -> Result<String, ResponseBody> {
    let mut body = json!({"op": "dispatch"});
    body.as_object_mut()
        .unwrap()
        .extend(spec.as_object().unwrap().clone());
    match request(conn, 1, body).await.body {
        ResponseBody::Dispatched { session } => Ok(session.into_string()),
        other => Err(other),
    }
}

/// Reads a stream already requested with `id` to its end: the joined output and final state.
pub async fn collect<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<T>,
    id: u64,
) -> (String, SessionState) {
    let mut out = String::new();
    loop {
        let resp = next(conn).await;
        assert_eq!(resp.id, id);
        match resp.body {
            ResponseBody::Event { data } => out.push_str(&data),
            ResponseBody::End { state } => return (out, state),
            other => panic!("unexpected frame in a stream: {other:?}"),
        }
    }
}

/// Waits for a session to end, over `wait`, and returns its whole output and final state.
pub async fn wait<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<T>,
    session: &str,
) -> (String, SessionState) {
    conn.send(&json!({"id": 9, "body": {"op": "wait", "session": session}}))
        .await
        .unwrap();
    collect(conn, 9).await
}

pub fn error_code(body: &ResponseBody) -> &'static str {
    match body {
        ResponseBody::Error(e) => e.code.as_str(),
        other => panic!("expected an error, got {other:?}"),
    }
}

/// Polls `f` until it holds, for up to ten seconds.
pub async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..100 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

pub fn alive(pid: i32) -> bool {
    rustix::process::Pid::from_raw(pid)
        .is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
}
