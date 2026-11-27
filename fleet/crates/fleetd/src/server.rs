//! The unix and TLS listeners, with their network policy and auth, and the request loop
//! every transport shares.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use fleet_proto::{
    Connection, DispatchSpec, ErrorCode, Hello, MAX_SESSIONS, ModelName, PROTOCOL_VERSION,
    ProtoError, Request, RequestBody, Response, ResponseBody, Role, Session, SessionId,
    StatusReport, WireError,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;

use crate::config::Config;
use crate::discovery::Discovery;
use crate::dispatch::Sessions;
use crate::inventory::Inventory;
use crate::logs::{self, Start};
use crate::tls::{self, PerIp};
use crate::{ensure_private_dir, own_uid, stopped, unix_now};

const PRUNE_EVERY: Duration = Duration::from_secs(60);
/// A pause after a failed `accept` (such as running out of descriptors), so the loop does
/// not spin while the cause persists.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The running daemon: its listeners and background tasks.
pub struct Running {
    socket: PathBuf,
    tls_addr: Option<SocketAddr>,
    shutdown: watch::Sender<bool>,
    tasks: JoinSet<()>,
}

impl Running {
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn tls_addr(&self) -> Option<SocketAddr> {
        self.tls_addr
    }

    /// Stops accepting and closes every connection. Dispatched sessions keep running.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown.send(true);
        while self.tasks.join_next().await.is_some() {}
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Starts the daemon: loads the sessions, takes a first metrics sample, and binds the unix
/// socket and, with `[tls]`, the TCP listener. `tls` is [`tls::load`]'s result.
pub async fn start(cfg: Config, tls: Option<Arc<rustls::ServerConfig>>) -> anyhow::Result<Running> {
    let cfg = Arc::new(cfg);
    let sessions = Sessions::open(Arc::clone(&cfg)).context("state_dir")?;
    let inventory = Arc::new(Inventory::new(cfg.device.clone(), cfg.metrics.clone()));
    let daemon = Arc::new(Daemon {
        discovery: Arc::new(Discovery::new(&cfg)),
        conns: Arc::new(Semaphore::new(cfg.max_connections)),
        cfg: Arc::clone(&cfg),
        sessions,
        inventory: Arc::clone(&inventory),
    });

    let unix = bind_unix(&cfg.unix.socket).await?;
    let tcp = match &cfg.tls {
        Some(t) => Some(
            TcpListener::bind(t.listen)
                .await
                .with_context(|| format!("binding {}", t.listen))?,
        ),
        None => None,
    };
    let tls_addr = tcp.as_ref().map(TcpListener::local_addr).transpose()?;

    let (shutdown, rx) = watch::channel(false);
    let mut tasks = JoinSet::new();
    tasks.spawn(Arc::clone(&daemon).unix_loop(unix, rx.clone()));
    if let (Some(tcp), Some(server_config), Some(t)) = (tcp, tls, &cfg.tls) {
        let policy = TlsPolicy {
            acceptor: TlsAcceptor::from(server_config),
            per_ip: PerIp::new(t.max_connections_per_ip),
            conns: Arc::new(Semaphore::new(t.max_connections)),
            handshakes: Arc::new(Semaphore::new(t.max_handshakes)),
        };
        tasks.spawn(Arc::clone(&daemon).tls_loop(tcp, policy, rx.clone()));
    }
    tasks.spawn(Arc::clone(&inventory).run(rx.clone()));
    tasks.spawn(prune_loop(Arc::clone(&daemon.sessions), rx));
    tracing::info!(
        device = %cfg.device,
        socket = %cfg.unix.socket.display(),
        tls = ?tls_addr,
        "fleetd started"
    );
    Ok(Running {
        socket: cfg.unix.socket.clone(),
        tls_addr,
        shutdown,
        tasks,
    })
}

/// Binds the socket 0600 in a private directory. A socket file nothing answers on is a
/// leftover and is replaced; one something answers on means another fleetd is running.
async fn bind_unix(path: &Path) -> anyhow::Result<UnixListener> {
    let dir = path
        .parent()
        .context("unix.socket has no parent directory")?;
    ensure_private_dir(dir).context("unix.socket")?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => {
            if UnixStream::connect(path).await.is_ok() {
                bail!("{}: another fleetd is running", path.display());
            }
            std::fs::remove_file(path)?;
        }
        Ok(_) => bail!("{}: exists and is not a socket", path.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

async fn prune_loop(sessions: Arc<Sessions>, mut shutdown: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(PRUNE_EVERY);
    loop {
        tokio::select! {
            () = stopped(&mut shutdown) => return,
            _ = tick.tick() => {
                let sessions = Arc::clone(&sessions);
                let _ = tokio::task::spawn_blocking(move || sessions.prune(unix_now())).await;
            }
        }
    }
}

/// Who is on the other end, for the audit log.
#[derive(Debug, Clone)]
enum Peer {
    Unix { uid: u32 },
    Tls(tls::Identity),
}

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix { uid } => write!(f, "unix uid {uid}"),
            Self::Tls(id) => write!(f, "tls subject {:?} serial {}", id.subject, id.serial),
        }
    }
}

/// Only fleetd's own user may use the socket: that user already controls its agents, and
/// no one else should.
pub(crate) fn peer_allowed(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid
}

struct TlsPolicy {
    acceptor: TlsAcceptor,
    per_ip: PerIp,
    /// One permit per tls connection, from accept to close.
    conns: Arc<Semaphore>,
    /// One permit per connection still in its TLS handshake or `Hello`.
    handshakes: Arc<Semaphore>,
}

struct Daemon {
    cfg: Arc<Config>,
    sessions: Arc<Sessions>,
    inventory: Arc<Inventory>,
    discovery: Arc<Discovery>,
    /// One permit per unix connection, the ssh relay's included.
    conns: Arc<Semaphore>,
}

impl Daemon {
    async fn unix_loop(
        self: Arc<Self>,
        listener: UnixListener,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut conns = JoinSet::new();
        loop {
            tokio::select! {
                () = stopped(&mut shutdown) => return,
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        conns.spawn(Arc::clone(&self).unix_conn(stream));
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "unix accept failed");
                        tokio::time::sleep(ACCEPT_BACKOFF).await;
                    }
                },
            }
        }
    }

    async fn unix_conn(self: Arc<Self>, stream: UnixStream) {
        let uid = match stream.peer_cred() {
            Ok(cred) => cred.uid(),
            Err(e) => {
                tracing::warn!(error = %e, "could not read a unix peer's credentials");
                return;
            }
        };
        if !peer_allowed(uid, own_uid()) {
            tracing::warn!(uid, "refused a unix peer of another user");
            return;
        }
        let mut conn = Connection::new(stream);
        let limit = self.cfg.handshake_timeout;
        let Ok(permit) = Arc::clone(&self.conns).try_acquire_owned() else {
            let _ = tokio::time::timeout(
                limit,
                refuse(&mut conn, ErrorCode::Busy, "fleetd is at max_connections"),
            )
            .await;
            return;
        };
        let Ok(Some(role)) =
            tokio::time::timeout(limit, handshake(&mut conn, self.cfg.unix.max_role)).await
        else {
            return;
        };
        self.serve(conn, role, &Peer::Unix { uid }, permit).await;
    }

    async fn tls_loop(
        self: Arc<Self>,
        listener: TcpListener,
        policy: TlsPolicy,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let policy = Arc::new(policy);
        let mut conns = JoinSet::new();
        loop {
            tokio::select! {
                () = stopped(&mut shutdown) => return,
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
                accepted = listener.accept() => match accepted {
                    Ok((tcp, addr)) => {
                        let ip = addr.ip().to_canonical();
                        let allow = &self.cfg.tls.as_ref().map_or(&[][..], |t| &t.allow_cidrs[..]);
                        if !allow.is_empty() && !allow.iter().any(|net| net.contains(&ip)) {
                            tracing::debug!(%ip, "refused a peer outside allow_cidrs");
                            continue;
                        }
                        let Ok(permit) = Arc::clone(&policy.conns).try_acquire_owned() else {
                            tracing::debug!(%ip, "refused a tls peer over tls.max_connections");
                            continue;
                        };
                        let Ok(handshaking) = Arc::clone(&policy.handshakes).try_acquire_owned()
                        else {
                            tracing::debug!(%ip, "refused a tls peer over tls.max_handshakes");
                            continue;
                        };
                        let Some(slot) = policy.per_ip.acquire(ip) else {
                            tracing::debug!(%ip, "refused a tls peer over max_connections_per_ip");
                            continue;
                        };
                        let this = Arc::clone(&self);
                        let policy = Arc::clone(&policy);
                        conns.spawn(async move {
                            let _slot = slot;
                            let _ = tcp.set_nodelay(true);
                            this.tls_conn(tcp, &policy.acceptor, permit, handshaking).await;
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "tcp accept failed");
                        tokio::time::sleep(ACCEPT_BACKOFF).await;
                    }
                },
            }
        }
    }

    async fn tls_conn(
        self: Arc<Self>,
        tcp: tokio::net::TcpStream,
        acceptor: &TlsAcceptor,
        permit: OwnedSemaphorePermit,
        handshaking: OwnedSemaphorePermit,
    ) {
        let Some(tls_cfg) = self.cfg.tls.as_ref() else {
            return;
        };
        let revoked = &tls_cfg.revoked_serials;
        let setup = async {
            let stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(error = %e, "tls handshake failed");
                    return None;
                }
            };
            let identity = stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|c| c.first())
                .ok_or("no client certificate")
                .and_then(|leaf| tls::identify(leaf, revoked));
            let mut conn = Connection::new(stream);
            match identity {
                Ok(id) => {
                    let role = handshake(&mut conn, id.role).await?;
                    Some((conn, role, id))
                }
                Err(why) => {
                    tracing::info!(reason = why, "refused a tls client certificate");
                    refuse(&mut conn, ErrorCode::Unauthorized, why).await;
                    None
                }
            }
        };
        let setup = tokio::time::timeout(self.cfg.handshake_timeout, setup).await;
        drop(handshaking);
        if let Ok(Some((conn, role, id))) = setup {
            self.serve(conn, role, &Peer::Tls(id), permit).await;
        }
    }

    /// The request loop. `_permit` holds the connection's place in its transport's budget.
    async fn serve<T: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut conn: Connection<T>,
        role: Role,
        peer: &Peer,
        _permit: OwnedSemaphorePermit,
    ) {
        let idle = self.cfg.idle_timeout;
        loop {
            let req = match tokio::time::timeout(idle, conn.recv::<Request>()).await {
                Err(_) | Ok(Ok(None)) => return,
                Ok(Err(e)) => {
                    fail(&mut conn, &e, idle).await;
                    return;
                }
                Ok(Ok(Some(req))) => req,
            };
            let id = req.id.get();
            if !role.permits(req.body.op()) {
                let refused = Response::error(
                    id,
                    ErrorCode::Unauthorized,
                    format!("the {role} role does not permit this request"),
                );
                if !send(&mut conn, &refused, idle).await {
                    return;
                }
                continue;
            }
            let ok = match req.body {
                RequestBody::Status {} => {
                    let body = ResponseBody::Status(Box::new(self.status()));
                    send(&mut conn, &Response { id, body }, idle).await
                }
                RequestBody::Sessions {} => {
                    let (sessions, truncated) = self.session_list().await;
                    let body = ResponseBody::Sessions {
                        sessions,
                        truncated,
                    };
                    send(&mut conn, &Response { id, body }, idle).await
                }
                RequestBody::Dispatch(spec) => {
                    let body = match self.dispatch(spec, peer).await {
                        Ok(session) => ResponseBody::Dispatched { session },
                        Err(e) => ResponseBody::Error(e),
                    };
                    send(&mut conn, &Response { id, body }, idle).await
                }
                RequestBody::Cancel { session } => {
                    let body = match self.cancel(&session, peer).await {
                        Ok(()) => ResponseBody::Cancelled { session },
                        Err(e) => ResponseBody::Error(e),
                    };
                    send(&mut conn, &Response { id, body }, idle).await
                }
                RequestBody::Logs {
                    session,
                    lines,
                    follow,
                } => {
                    self.stream(&mut conn, id, session, Start::Tail(lines.get()), follow)
                        .await
                }
                RequestBody::Wait { session } => {
                    self.stream(&mut conn, id, session, Start::Beginning, true)
                        .await
                }
            };
            if !ok {
                return;
            }
        }
    }

    fn status(&self) -> StatusReport {
        StatusReport {
            device: self.inventory.device().clone(),
            metrics: self.inventory.metrics(),
            adapters: self.sessions.adapter_infos(),
        }
    }

    /// Running sessions, then ended ones, each newest first, at most [`MAX_SESSIONS`].
    async fn session_list(&self) -> (Vec<Session>, bool) {
        let mut all = self.sessions.list();
        all.extend(self.discover().await);
        all.sort_by(|a, b| {
            b.state
                .is_running()
                .cmp(&a.state.is_running())
                .then(b.started_at.cmp(&a.started_at))
        });
        let truncated = all.len() > MAX_SESSIONS;
        all.truncate(MAX_SESSIONS);
        (all, truncated)
    }

    async fn discover(&self) -> Vec<Session> {
        let skip = self.sessions.running_groups();
        let discovery = Arc::clone(&self.discovery);
        tokio::task::spawn_blocking(move || discovery.scan(&skip))
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "discovery failed");
                Vec::new()
            })
    }

    /// Why a session fleetd did not dispatch cannot be streamed or cancelled.
    async fn not_dispatched(&self, id: &SessionId) -> WireError {
        let discovered =
            id.as_str().starts_with("p-") && self.discover().await.iter().any(|s| &s.id == id);
        if discovered {
            WireError::new(
                ErrorCode::Forbidden,
                "a discovered session has no log and cannot be cancelled",
            )
        } else {
            WireError::new(ErrorCode::NotFound, "no such session")
        }
    }

    async fn dispatch(&self, spec: DispatchSpec, peer: &Peer) -> Result<SessionId, WireError> {
        let adapter = spec.adapter.clone();
        let model = spec.model.clone();
        let result = self.sessions.dispatch(spec).await;
        match &result {
            Ok(session) => tracing::info!(
                %peer, %adapter, model = ?model.as_ref().map(ModelName::as_str), %session,
                "dispatched"
            ),
            Err(e) => {
                tracing::info!(%peer, %adapter, code = %e.code, reason = %e.message, "dispatch refused");
            }
        }
        result
    }

    async fn cancel(&self, id: &SessionId, peer: &Peer) -> Result<(), WireError> {
        if self.sessions.cancel(id) {
            tracing::info!(%peer, session = %id, "cancelled");
            Ok(())
        } else {
            Err(self.not_dispatched(id).await)
        }
    }

    /// Runs a `logs` or `wait` stream. A frame from the client while it runs breaks the
    /// protocol; the client closing ends the stream only. `false` if the connection is done.
    async fn stream<T: AsyncRead + AsyncWrite + Unpin>(
        &self,
        conn: &mut Connection<T>,
        id: u64,
        session: SessionId,
        start: Start,
        follow: bool,
    ) -> bool {
        let idle = self.cfg.idle_timeout;
        if !self.sessions.contains(&session) {
            let body = ResponseBody::Error(self.not_dispatched(&session).await);
            return send(conn, &Response { id, body }, idle).await;
        }
        // A small buffer, so a slow reader holds back the producer instead of growing memory.
        let (tx, mut rx) = mpsc::channel(4);
        let _producer = AbortOnDrop(tokio::spawn(logs::produce(
            Arc::clone(&self.sessions),
            session,
            start,
            follow,
            tx,
        )));
        loop {
            tokio::select! {
                body = rx.recv() => {
                    let Some(body) = body else { return false };
                    let last = matches!(body, ResponseBody::End { .. } | ResponseBody::Error(_));
                    if !send(conn, &Response { id, body }, idle).await {
                        return false;
                    }
                    if last {
                        return true;
                    }
                }
                frame = conn.recv::<serde::de::IgnoredAny>() => {
                    match frame {
                        Ok(Some(_)) => {
                            let e = Response::error(0, ErrorCode::BadRequest, "a request arrived while a stream was in progress");
                            send(conn, &e, idle).await;
                        }
                        Ok(None) => {}
                        Err(e) => fail(conn, &e, idle).await,
                    }
                    return false;
                }
            }
        }
    }
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Sends one frame within `limit`, so a peer that stops reading cannot hold a connection
/// open forever. `false` if the connection is done.
async fn send<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<T>,
    resp: &Response,
    limit: Duration,
) -> bool {
    matches!(
        tokio::time::timeout(limit, conn.send(resp)).await,
        Ok(Ok(()))
    )
}

/// Reports a connection-level fault with id 0, when there is a frame to send.
async fn fail<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<T>,
    e: &ProtoError,
    limit: Duration,
) {
    // The decoder's message can quote the frame, which may hold a prompt.
    let resp = match e {
        ProtoError::Decode(_) => {
            Response::error(0, ErrorCode::BadRequest, "the frame did not decode")
        }
        ProtoError::TooLarge => Response::error(0, ErrorCode::TooLarge, "the frame exceeds 1 MiB"),
        _ => return,
    };
    send(conn, &resp, limit).await;
}

/// The server side of the `Hello` exchange, reporting a failed one to the client where the
/// protocol says to. `None` if the connection is done.
pub(crate) async fn handshake<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<T>,
    max_role: Role,
) -> Option<Role> {
    match conn.server_hello(max_role).await {
        Ok(role) => Some(role),
        Err(ProtoError::VersionMismatch { theirs, .. }) => {
            let msg = format!("fleetd speaks protocol {PROTOCOL_VERSION}, the client {theirs}");
            let _ = conn
                .send(&Response::error(0, ErrorCode::VersionMismatch, msg))
                .await;
            None
        }
        Err(e @ (ProtoError::Decode(_) | ProtoError::TooLarge)) => {
            if send_hello(conn, Role::Monitor).await {
                fail(conn, &e, Duration::MAX).await;
            }
            None
        }
        Err(_) => None,
    }
}

pub(crate) async fn send_hello<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<T>,
    role: Role,
) -> bool {
    conn.send(&Hello {
        protocol_version: PROTOCOL_VERSION,
        role,
    })
    .await
    .is_ok()
}

/// Completes the `Hello` exchange granting `monitor`, then refuses the connection with an
/// id-0 error, so the client can decode why.
pub(crate) async fn refuse<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut Connection<T>,
    code: ErrorCode,
    message: &str,
) {
    if handshake(conn, Role::Monitor).await.is_some() {
        let _ = conn.send(&Response::error(0, code, message)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_own_uid_may_connect() {
        assert!(peer_allowed(501, 501));
        assert!(!peer_allowed(0, 501));
        assert!(!peer_allowed(502, 501));
    }
}
