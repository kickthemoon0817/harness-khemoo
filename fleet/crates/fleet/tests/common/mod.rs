//! Helpers shared by the hub's integration tests: temporary directories, a test PKI, a
//! mock device speaking the protocol over any stream, servers for each transport, a fake
//! `ssh`, and a runner for the `fleet` binary. An end-to-end suite against a real fleetd
//! reuses everything here but the mock device.

// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fleet::pki;
use fleet_proto::{
    AdapterInfo, Connection, DeviceInfo, Labels, Memory, Metrics, Origin, Request, RequestBody,
    Response, ResponseBody, Role, Session, SessionState, StatusReport, WireError,
};
use rustls::RootCertStore;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::pem::PemObject;
use rustls::server::{ServerConfig, WebPkiClientVerifier};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};
use tokio_rustls::TlsAcceptor;

/// The id the mock gives every dispatched session.
pub const SESSION: &str = "d-1700000000-1a2b3c4d";

/// A directory under the system temp dir, removed on drop. Names stay short because a
/// unix socket path inside it must fit `sun_path` (104 bytes on macOS).
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("fl-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, p: impl AsRef<Path>) -> PathBuf {
        self.0.join(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn status_report(
    name: &str,
    cpus: u32,
    load: f64,
    adapters: &[(&str, &[&str], u32, u32)],
) -> StatusReport {
    StatusReport {
        device: DeviceInfo {
            name: name.parse().unwrap(),
            hostname: format!("{name}.example.com"),
            os: "Ubuntu 24.04".into(),
            kernel: "6.8.0".into(),
            arch: "x86_64".into(),
            cpus,
            fleetd_version: "0.1.0".into(),
        },
        metrics: Metrics {
            sampled_at: 1_700_000_000,
            uptime_s: 86_400,
            load: [load, 0.25, 0.125],
            cpu_percent: 12.5,
            memory: Memory {
                total_bytes: 68_719_476_736,
                used_bytes: 8_589_934_592,
            },
            swap: Memory {
                total_bytes: 0,
                used_bytes: 0,
            },
            disks: vec![],
            gpus: vec![],
        },
        adapters: adapters
            .iter()
            .map(|(name, models, max, running)| AdapterInfo {
                name: name.parse().unwrap(),
                models: models.iter().map(|m| m.parse().unwrap()).collect(),
                max_concurrent: *max,
                running: *running,
            })
            .collect(),
    }
}

pub fn session(id: &str, started_at: u64, state: SessionState) -> Session {
    let labels: BTreeMap<String, String> = [("issue".to_owned(), "42".to_owned())].into();
    Session {
        id: id.parse().unwrap(),
        adapter: "claude".parse().unwrap(),
        origin: Origin::Dispatched,
        state,
        pid: Some(4242),
        started_at,
        ended_at: None,
        cwd: Some("/srv/work/repo-42".into()),
        exe: Some("/usr/local/bin/claude".into()),
        model: Some("sonnet".parse().unwrap()),
        summary: "claude -p --model".into(),
        labels: Labels::try_from(labels).unwrap(),
    }
}

/// A device that answers from fixed data, and records what it was asked.
pub struct MockDevice {
    pub status: StatusReport,
    pub sessions: Vec<Session>,
    pub truncated: bool,
    /// The `event` data a `wait` or `logs` stream sends before `end`.
    pub output: Vec<String>,
    pub end: SessionState,
    /// Returned in place of `dispatched`.
    pub dispatch_error: Option<WireError>,
    /// Sent with id 0 right after the `Hello`, as fleetd does when it refuses a peer.
    pub refuse: Option<WireError>,
    /// How long `status` takes to answer.
    pub delay: Duration,
    /// Whether the connection drops on a `dispatch` without answering it.
    pub drop_dispatch: bool,
    /// Whether a `wait` or `logs` stream stays open after its output instead of ending.
    pub hang_stream: bool,
    pub connections: AtomicUsize,
    /// The role granted on each connection.
    pub roles: Mutex<Vec<Role>>,
    pub requests: Mutex<Vec<RequestBody>>,
}

impl MockDevice {
    pub fn new(name: &str) -> Self {
        Self {
            status: status_report(name, 16, 0.5, &[("claude", &["sonnet", "opus"], 2, 1)]),
            sessions: vec![session(SESSION, 1_700_000_000, SessionState::Running {})],
            truncated: false,
            output: vec!["hello ".into(), "world\n".into()],
            end: SessionState::Exited { code: 0 },
            dispatch_error: None,
            refuse: None,
            delay: Duration::ZERO,
            hang_stream: false,
            drop_dispatch: false,
            connections: AtomicUsize::new(0),
            roles: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<RequestBody> {
        self.requests.lock().unwrap().clone()
    }

    pub fn roles(&self) -> Vec<Role> {
        self.roles.lock().unwrap().clone()
    }
}

/// Serves one connection as fleetd would, from `mock`'s data.
pub async fn serve<T: AsyncRead + AsyncWrite + Unpin>(io: T, mock: Arc<MockDevice>) {
    mock.connections.fetch_add(1, Ordering::SeqCst);
    let mut conn = Connection::new(io);
    let Ok(role) = conn.server_hello(Role::Dispatch).await else {
        return;
    };
    mock.roles.lock().unwrap().push(role);
    if let Some(e) = &mock.refuse {
        let _ = conn
            .send(&Response {
                id: 0,
                body: ResponseBody::Error(e.clone()),
            })
            .await;
        return;
    }
    while let Ok(Some(req)) = conn.recv::<Request>().await {
        let id = req.id.get();
        let streams = matches!(
            req.body,
            RequestBody::Logs { .. } | RequestBody::Wait { .. }
        );
        mock.requests.lock().unwrap().push(req.body.clone());
        if mock.drop_dispatch && matches!(req.body, RequestBody::Dispatch(_)) {
            return;
        }
        let replies = if role.permits(req.body.op()) {
            answer(&mock, req.body).await
        } else {
            vec![ResponseBody::Error(WireError::new(
                fleet_proto::ErrorCode::Unauthorized,
                "role",
            ))]
        };
        for body in replies {
            if conn.send(&Response { id, body }).await.is_err() {
                return;
            }
        }
        if streams && mock.hang_stream {
            return hang(&mut conn).await;
        }
    }
}

/// Holds a stream open, as fleetd does while a session runs, until the client goes away.
async fn hang<T: AsyncRead + AsyncWrite + Unpin>(conn: &mut Connection<T>) {
    let _ = conn.recv::<Request>().await;
}

async fn answer(mock: &MockDevice, body: RequestBody) -> Vec<ResponseBody> {
    match body {
        RequestBody::Status {} => {
            tokio::time::sleep(mock.delay).await;
            vec![ResponseBody::Status(Box::new(mock.status.clone()))]
        }
        RequestBody::Sessions {} => vec![ResponseBody::Sessions {
            sessions: mock.sessions.clone(),
            truncated: mock.truncated,
        }],
        RequestBody::Dispatch(_) => vec![match &mock.dispatch_error {
            Some(e) => ResponseBody::Error(e.clone()),
            None => ResponseBody::Dispatched {
                session: SESSION.parse().unwrap(),
            },
        }],
        RequestBody::Cancel { session } => vec![ResponseBody::Cancelled { session }],
        RequestBody::Logs { .. } | RequestBody::Wait { .. } => {
            // A keep-alive first: readers must skip empty events.
            let mut v = vec![ResponseBody::Event {
                data: String::new(),
            }];
            v.extend(
                mock.output
                    .iter()
                    .map(|d| ResponseBody::Event { data: d.clone() }),
            );
            if !mock.hang_stream {
                v.push(ResponseBody::End {
                    state: mock.end.clone(),
                });
            }
            v
        }
    }
}

/// Listens on a unix socket at `socket` and serves every connection with `mock`.
pub fn serve_unix(socket: &Path, mock: Arc<MockDevice>) {
    let listener = UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(serve(stream, Arc::clone(&mock)));
        }
    });
}

/// A unix socket that accepts connections and never says anything.
pub fn serve_silent(socket: &Path) {
    let listener = UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
}

/// The files of a test PKI: a CA, a server certificate for `dev-a` (also valid for
/// 127.0.0.1) and a dispatch client certificate `hub`.
pub struct Pki {
    pub dir: PathBuf,
    pub ca: PathBuf,
    pub server_cert: PathBuf,
    pub server_key: PathBuf,
    pub client_cert: PathBuf,
    pub client_key: PathBuf,
}

pub fn make_pki(dir: &Path) -> Pki {
    pki::init(dir, 30).unwrap();
    let server = pki::issue_server(
        dir,
        &"dev-a".parse().unwrap(),
        &[],
        &[IpAddr::V4(Ipv4Addr::LOCALHOST)],
        30,
    )
    .unwrap();
    let client = pki::issue_client(dir, &"hub".parse().unwrap(), Role::Dispatch, 30).unwrap();
    Pki {
        dir: dir.to_path_buf(),
        ca: dir.join("ca.pem"),
        server_cert: server.cert,
        server_key: server.key,
        client_cert: client.cert,
        client_key: client.key,
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A TLS 1.3 server config that requires a client certificate chaining to `pki.ca`.
pub fn tls_server_config(pki: &Pki) -> Arc<ServerConfig> {
    let mut roots = RootCertStore::empty();
    for c in fleet::transport::read_certs(&pki.ca).unwrap() {
        roots.add(c).unwrap();
    }
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider())
        .build()
        .unwrap();
    let chain = fleet::transport::read_certs(&pki.server_cert).unwrap();
    let key = PrivateKeyDer::from_pem_file(&pki.server_key).unwrap();
    Arc::new(
        ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain, key)
            .unwrap(),
    )
}

/// Listens on a loopback port with TLS and serves every connection with `mock`.
pub async fn serve_tls(config: Arc<ServerConfig>, mock: Arc<MockDevice>) -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let acceptor = TlsAcceptor::from(config);
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let mock = Arc::clone(&mock);
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(tcp).await {
                    serve(tls, mock).await;
                }
            });
        }
    });
    addr
}

/// What a fake `ssh` does.
#[derive(Clone, Copy)]
pub enum FakeSsh<'a> {
    /// Runs this test binary's `mock_stdio_server`, serving a mock device with this name.
    Mock(&'a str),
    /// Runs `exec "$@"` on the remote command, as a pinned key would: for a real fleetd.
    Exec,
    /// Writes this to stderr and exits 255, as ssh does when it cannot connect.
    Fail(&'a str),
    /// Prints a login banner on stdout and stays connected, as a misconfigured remote shell does.
    Banner,
}

/// Writes an executable `ssh` into `dir` that records its argv in `dir/argv`, one per
/// line, and then does what `mode` says. Returns its path.
pub fn fake_ssh(dir: &Path, mode: FakeSsh<'_>) -> PathBuf {
    let argv = dir.join("argv");
    let action = match mode {
        // fd 3 carries the frames; the test harness's own chatter goes to stderr.
        FakeSsh::Mock(name) => format!(
            "FLEET_MOCK_STDIO={name} exec '{}' --ignored --exact common::mock_stdio_server \
             --nocapture 3>&1 1>&2",
            std::env::current_exe().unwrap().display()
        ),
        FakeSsh::Exec => "while [ \"$1\" != -- ]; do shift; done; shift 2; exec \"$@\"".into(),
        FakeSsh::Fail(stderr) => format!("echo '{stderr}' >&2; exit 255"),
        FakeSsh::Banner => "echo 'Welcome to dev-c'; exec sleep 30".into(),
    };
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n{action}\n",
        argv.display()
    );
    let path = dir.join("ssh");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

pub fn recorded_argv(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("argv"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The mock device behind [`FakeSsh::Mock`]. It does nothing unless a fake ssh started
/// this binary for it.
#[test]
#[ignore = "run only by a fake ssh"]
fn mock_stdio_server() {
    let Some(name) = std::env::var_os("FLEET_MOCK_STDIO") else {
        return;
    };
    let name = name.into_string().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let out = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/fd/3")
            .unwrap();
        let io = tokio::io::join(tokio::io::stdin(), tokio::fs::File::from_std(out));
        serve(io, Arc::new(MockDevice::new(&name))).await;
    });
    std::process::exit(0);
}

pub fn write_config(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("fleet.toml");
    std::fs::write(&path, text).unwrap();
    path
}

/// The outcome of running the `fleet` binary.
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `fleet --config <config> <args>` with `stdin` as its standard input and `env`
/// added to a clean fleet environment.
pub async fn run_fleet(config: &Path, args: &[&str], stdin: &[u8], env: &[(&str, &str)]) -> Run {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_fleet"));
    cmd.arg("--config")
        .arg(config)
        .args(args)
        .env_remove("FLEET_CONFIG")
        .env_remove("HARNESS_INSTANCE_DIR")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let stdin = stdin.to_vec();
    // fleet may stop reading early (an oversize prompt), so a failed write is expected.
    tokio::spawn(async move {
        let _ = input.write_all(&stdin).await;
    });
    let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("fleet did not finish")
        .unwrap();
    Run {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}
