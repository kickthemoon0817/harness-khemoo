//! The local, ssh and tls transports, each yielding a handshaken connection.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use fleet_proto::{Connection, ErrorCode, ProtoError, Role, WireError};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, Join, ReadBuf};
use tokio::net::{TcpStream, UnixStream};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::files;
use crate::registry::{Device, Timeouts, Transport};

/// How much of ssh's stderr is kept for an error message; the rest is drained and dropped
/// so ssh never blocks on a full pipe.
const SSH_STDERR_TAIL: usize = 2048;

/// Given before the device's own options. ssh keeps the first value it sees for an option,
/// so neither those nor `~/.ssh/config` can turn agent or X11 forwarding, port forwards or a
/// local command back on: a compromised device must not reach the hub's ssh agent.
const SSH_HARDENING: &[&str] = &[
    "-T",
    "-a",
    "-x",
    "-o",
    "BatchMode=yes",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "ForwardAgent=no",
    "-o",
    "PermitLocalCommand=no",
];

/// The byte stream under a [`Connection`], whichever transport opened it.
pub enum Stream {
    Unix(UnixStream),
    Tls(Box<TlsStream<TcpStream>>),
    Ssh(Box<SshStream>),
}

/// An ssh child's stdout and stdin as one stream. Dropping it kills the child.
pub struct SshStream {
    io: Join<ChildStdout, ChildStdin>,
    child: Child,
    stderr: JoinHandle<Vec<u8>>,
}

/// Opens a connection to `device` and runs the client `Hello`, asking for `role`, all
/// within the connect timeout.
pub async fn connect(
    device: &Device,
    timeouts: &Timeouts,
    role: Role,
) -> Result<Connection<Stream>, WireError> {
    let attempt = async {
        let mut conn = Connection::new(open(&device.transport).await?);
        match conn.client_hello(role).await {
            Ok(_) => Ok(conn),
            Err(e) => Err(hello_failed(conn, e).await),
        }
    };
    timeout(timeouts.connect, attempt)
        .await
        .unwrap_or_else(|_| Err(no_answer(timeouts.connect)))
}

pub fn no_answer(after: Duration) -> WireError {
    WireError::new(
        ErrorCode::Timeout,
        format!("no answer in {}s", after.as_secs()),
    )
}

fn unreachable(msg: impl Into<String>) -> WireError {
    WireError::new(ErrorCode::Unreachable, msg)
}

async fn open(transport: &Transport) -> Result<Stream, WireError> {
    match transport {
        Transport::Local { socket } => UnixStream::connect(socket)
            .await
            .map(Stream::Unix)
            .map_err(|e| unreachable(format!("connect {}: {e}", socket.display()))),
        Transport::Tls {
            address,
            server_name,
            config,
        } => {
            let tcp = TcpStream::connect(address.as_str())
                .await
                .map_err(|e| unreachable(format!("connect {address}: {e}")))?;
            // Frames are small request/answer pairs; Nagle would only add latency.
            tcp.set_nodelay(true)
                .map_err(|e| unreachable(format!("connect {address}: {e}")))?;
            let tls = TlsConnector::from(Arc::clone(config))
                .connect(server_name.clone(), tcp)
                .await
                .map_err(|e| unreachable(format!("tls: {e}")))?;
            Ok(Stream::Tls(Box::new(tls)))
        }
        Transport::Ssh {
            program,
            target,
            options,
            remote_command,
        } => spawn_ssh(program, target, options, remote_command).map(|s| Stream::Ssh(Box::new(s))),
    }
}

fn spawn_ssh(
    program: &Path,
    target: &str,
    options: &[String],
    remote_command: &[String],
) -> Result<SshStream, WireError> {
    let mut cmd = Command::new(program);
    cmd.args(SSH_HARDENING);
    for o in options {
        cmd.arg("-o").arg(o);
    }
    cmd.arg("--")
        .arg(target)
        .args(remote_command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| unreachable(format!("cannot run {}: {e}", program.display())))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(unreachable("ssh started without its pipes"));
    };
    Ok(SshStream {
        io: tokio::io::join(stdout, stdin),
        child,
        stderr: tokio::spawn(drain_tail(stderr)),
    })
}

async fn drain_tail(mut stderr: ChildStderr) -> Vec<u8> {
    let mut tail = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n @ 1..) = stderr.read(&mut buf).await {
        tail.extend_from_slice(&buf[..n]);
        if tail.len() > SSH_STDERR_TAIL {
            tail.drain(..tail.len() - SSH_STDERR_TAIL);
        }
    }
    tail
}

async fn hello_failed(conn: Connection<Stream>, e: ProtoError) -> WireError {
    if let ProtoError::VersionMismatch { ours, theirs } = e {
        return WireError::new(
            ErrorCode::VersionMismatch,
            format!("device speaks protocol {theirs}, fleet speaks {ours}"),
        );
    }
    match conn.into_parts().0 {
        // ssh's own exit status and stderr say far more than a closed pipe.
        Stream::Ssh(ssh) => unreachable(ssh.diagnose(&e).await),
        _ => unreachable(format!("handshake: {e}")),
    }
}

impl SshStream {
    async fn diagnose(self, e: &ProtoError) -> String {
        let Self {
            io,
            mut child,
            stderr,
        } = self;
        drop(io);
        let status = timeout(Duration::from_secs(2), child.wait()).await;
        if status.is_err() {
            // Still running: ssh connected and the far side broke the handshake, such as a
            // shell banner on stdout. That error is the cause, not ssh.
            let _ = child.kill().await;
        }
        let tail = match timeout(Duration::from_secs(1), stderr).await {
            Ok(Ok(tail)) => tail,
            _ => Vec::new(),
        };
        let text = String::from_utf8_lossy(&tail);
        let detail = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        let head = match status {
            Ok(Ok(s)) => match s.code() {
                Some(code) => format!("ssh exited {code}"),
                None => "ssh was killed by a signal".to_owned(),
            },
            _ => format!("handshake over ssh: {e}"),
        };
        if detail.is_empty() {
            head
        } else {
            format!("{head}: {detail}")
        }
    }
}

/// Which file of a TLS identity a [`IdentityError`] is about, so the registry can name the key.
#[derive(Debug, Clone, Copy)]
pub enum IdentityFile {
    Ca,
    Cert,
    Key,
    /// The certificate and key together, such as a key that does not match.
    Pair,
}

#[derive(Debug)]
pub struct IdentityError {
    pub file: IdentityFile,
    pub message: String,
}

/// The client TLS config for one identity: TLS 1.3 only, servers verified against `ca`
/// alone (no webpki or system roots), and `cert` with `key` as the client certificate.
pub fn client_config(ca: &Path, cert: &Path, key: &Path) -> Result<ClientConfig, IdentityError> {
    let fail = |file, message: String| IdentityError { file, message };
    let mut roots = RootCertStore::empty();
    for c in read_certs(ca).map_err(|m| fail(IdentityFile::Ca, m))? {
        roots
            .add(c)
            .map_err(|e| fail(IdentityFile::Ca, format!("{}: {e}", ca.display())))?;
    }
    let chain = read_certs(cert).map_err(|m| fail(IdentityFile::Cert, m))?;
    let key_pem = files::read_private_key(key)
        .map_err(|e| fail(IdentityFile::Key, format!("{}: {e}", key.display())))?;
    let key_der = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|e| fail(IdentityFile::Key, format!("{}: {e}", key.display())))?;
    ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| fail(IdentityFile::Pair, e.to_string()))?
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key_der)
        .map_err(|e| fail(IdentityFile::Pair, format!("certificate and key: {e}")))
}

/// Every certificate in a PEM file; at least one.
pub fn read_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let pem = files::read_capped(path, files::MAX_CONFIG_LEN)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let certs = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if certs.is_empty() {
        return Err(format!("{}: no certificate in it", path.display()));
    }
    Ok(certs)
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Unix(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s).poll_read(cx, buf),
            Self::Ssh(s) => Pin::new(&mut s.io).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Unix(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s).poll_write(cx, buf),
            Self::Ssh(s) => Pin::new(&mut s.io).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Unix(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s).poll_flush(cx),
            Self::Ssh(s) => Pin::new(&mut s.io).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Unix(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s).poll_shutdown(cx),
            Self::Ssh(s) => Pin::new(&mut s.io).poll_shutdown(cx),
        }
    }
}
