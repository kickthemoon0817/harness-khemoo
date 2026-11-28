use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use fleet_proto::{Connection, Response, ResponseBody, Role};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType, SerialNumber,
};
use rustls::ClientConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use serde_json::json;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::support::{TempDir, adapter, base_config, connect, error_code, next, request, start};

type Ca = CertifiedIssuer<'static, KeyPair>;

fn new_ca() -> Ca {
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap()
}

/// A leaf signed by `ca`: PEM chain and PEM key.
fn leaf(
    ca: &Ca,
    sans: Vec<SanType>,
    usage: ExtendedKeyUsagePurpose,
    serial: u64,
) -> (String, String) {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.subject_alt_names = sans;
    params.extended_key_usages = vec![usage];
    params.serial_number = Some(SerialNumber::from(serial));
    let cert = params.signed_by(&key, &**ca).unwrap();
    (cert.pem(), key.serialize_pem())
}

fn uri(s: &str) -> SanType {
    SanType::URI(s.try_into().unwrap())
}

fn client_cert(ca: &Ca, role_uris: &[&str], serial: u64) -> (String, String) {
    leaf(
        ca,
        role_uris.iter().map(|u| uri(u)).collect(),
        ExtendedKeyUsagePurpose::ClientAuth,
        serial,
    )
}

struct Pki {
    dir: TempDir,
    ca: Ca,
    ca_path: PathBuf,
}

impl Pki {
    fn new() -> Self {
        let dir = TempDir::new();
        let ca = new_ca();
        let ca_path = dir.join("ca.pem");
        std::fs::write(&ca_path, ca.pem()).unwrap();
        let (cert, key) = leaf(
            &ca,
            vec![
                SanType::DnsName("dev-a".try_into().unwrap()),
                SanType::IpAddress("127.0.0.1".parse().unwrap()),
            ],
            ExtendedKeyUsagePurpose::ServerAuth,
            1,
        );
        std::fs::write(dir.join("server.pem"), cert).unwrap();
        let key_path = dir.join("server.key");
        std::fs::write(&key_path, key).unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        Self { dir, ca, ca_path }
    }

    fn config(&self, extra: &str) -> String {
        let root = self.dir.mkdir("root");
        let agent = self.dir.script("agent", "echo ok\n");
        base_config(&self.dir)
            + &format!(
                "[tls]\nlisten = \"127.0.0.1:0\"\nca = \"{}\"\ncert = \"{}\"\nkey = \"{}\"\n{extra}\n",
                self.ca_path.display(),
                self.dir.join("server.pem").display(),
                self.dir.join("server.key").display(),
            )
            + &adapter("a", &agent, &[], &root, "")
    }
}

fn client_config(ca: &Ca, identity: Option<(String, String)>) -> Arc<ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca.der().clone()).unwrap();
    let builder =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots);
    let cfg = match identity {
        Some((cert, key)) => {
            let chain = CertificateDer::pem_slice_iter(cert.as_bytes())
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let key = PrivateKeyDer::from_pem_slice(key.as_bytes()).unwrap();
            builder.with_client_auth_cert(chain, key).unwrap()
        }
        None => builder.with_no_client_auth(),
    };
    Arc::new(cfg)
}

use rustls_pki_types::pem::PemObject;

type Tls = tokio_rustls::client::TlsStream<TcpStream>;

/// Connects and runs the hello; `Err` if the transport or the handshake failed.
async fn session(
    addr: SocketAddr,
    cfg: Arc<ClientConfig>,
    role: Role,
) -> Result<(Connection<Tls>, Role), String> {
    let tcp = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let tls = TlsConnector::from(cfg)
        .connect(ServerName::try_from("dev-a").unwrap(), tcp)
        .await
        .map_err(|e| e.to_string())?;
    let mut conn = Connection::new(tls);
    let granted = conn.client_hello(role).await.map_err(|e| e.to_string())?;
    Ok((conn, granted))
}

/// Whether a full request round trip works, which in TLS 1.3 is where a refused client
/// certificate shows up.
async fn works(addr: SocketAddr, cfg: Arc<ClientConfig>) -> bool {
    let Ok((mut conn, _)) = session(addr, cfg, Role::Monitor).await else {
        return false;
    };
    if conn
        .send(&json!({"id": 1, "body": {"op": "status"}}))
        .await
        .is_err()
    {
        return false;
    }
    matches!(
        conn.recv::<Response>().await,
        Ok(Some(Response {
            body: ResponseBody::Status(_),
            ..
        }))
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dispatch_client_with_a_good_certificate_works() {
    let pki = Pki::new();
    let running = start(&pki.config("")).await;
    let addr = running.tls_addr().unwrap();
    let cfg = client_config(
        &pki.ca,
        Some(client_cert(&pki.ca, &["fleet:role:dispatch"], 10)),
    );
    let (mut conn, granted) = session(addr, cfg, Role::Dispatch).await.unwrap();
    assert_eq!(granted, Role::Dispatch);
    let root = pki.dir.join("root");
    let resp = request(
        &mut conn,
        1,
        json!({"op": "dispatch", "adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await;
    let ResponseBody::Dispatched { session } = resp.body else {
        panic!("{resp:?}")
    };
    conn.send(&json!({"id": 2, "body": {"op": "wait", "session": session}}))
        .await
        .unwrap();
    let mut out = String::new();
    loop {
        match next(&mut conn).await.body {
            ResponseBody::Event { data } => out.push_str(&data),
            ResponseBody::End { .. } => break,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(out, "ok\n");
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_monitor_client_cannot_dispatch() {
    let pki = Pki::new();
    let running = start(&pki.config("")).await;
    let addr = running.tls_addr().unwrap();
    let cfg = client_config(
        &pki.ca,
        Some(client_cert(&pki.ca, &["fleet:role:monitor"], 11)),
    );
    let (mut conn, granted) = session(addr, cfg, Role::Dispatch).await.unwrap();
    assert_eq!(granted, Role::Monitor, "the certificate caps the role");
    let root = pki.dir.join("root");
    let resp = request(
        &mut conn,
        3,
        json!({"op": "dispatch", "adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await;
    assert_eq!((resp.id, error_code(&resp.body)), (3, "unauthorized"));
    let resp = request(&mut conn, 4, json!({"op": "status"})).await;
    assert!(matches!(resp.body, ResponseBody::Status(_)));
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clients_without_a_certificate_from_the_ca_are_refused() {
    let pki = Pki::new();
    let running = start(&pki.config("")).await;
    let addr = running.tls_addr().unwrap();

    assert!(
        !works(addr, client_config(&pki.ca, None)).await,
        "no certificate"
    );

    let other = new_ca();
    let foreign = client_cert(&other, &["fleet:role:dispatch"], 12);
    assert!(
        !works(addr, client_config(&pki.ca, Some(foreign))).await,
        "another CA"
    );

    // A server certificate from the right CA is no client identity.
    let wrong_usage = leaf(
        &pki.ca,
        vec![uri("fleet:role:dispatch")],
        ExtendedKeyUsagePurpose::ServerAuth,
        13,
    );
    assert!(
        !works(addr, client_config(&pki.ca, Some(wrong_usage))).await,
        "serverAuth only"
    );

    let good = client_cert(&pki.ca, &["fleet:role:monitor"], 14);
    assert!(works(addr, client_config(&pki.ca, Some(good))).await);
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_or_roleless_certificates_are_refused_after_the_hello() {
    let pki = Pki::new();
    let running = start(&pki.config("revoked_serials = [\"00:00:AB:CD\"]")).await;
    let addr = running.tls_addr().unwrap();
    for (uris, serial) in [
        (vec!["fleet:role:dispatch"], 0xabcd),
        (vec![], 20),
        (vec!["fleet:role:dispatch", "fleet:role:monitor"], 21),
        (vec!["fleet:role:admin"], 22),
    ] {
        let cfg = client_config(&pki.ca, Some(client_cert(&pki.ca, &uris, serial)));
        let (mut conn, granted) = session(addr, cfg, Role::Dispatch).await.unwrap();
        assert_eq!(granted, Role::Monitor);
        let resp = next(&mut conn).await;
        assert_eq!(
            (resp.id, error_code(&resp.body)),
            (0, "unauthorized"),
            "{uris:?}"
        );
        assert!(conn.recv::<Response>().await.unwrap_or(None).is_none());
    }
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peers_outside_allow_cidrs_are_closed_before_the_handshake() {
    let pki = Pki::new();
    let running = start(&pki.config("allow_cidrs = [\"192.0.2.0/24\"]")).await;
    let addr = running.tls_addr().unwrap();
    let cfg = client_config(
        &pki.ca,
        Some(client_cert(&pki.ca, &["fleet:role:dispatch"], 30)),
    );
    let err = session(addr, cfg, Role::Monitor).await.err().unwrap();
    assert!(!err.is_empty());
    running.shutdown().await;

    let running = start(&pki.config("allow_cidrs = [\"127.0.0.0/8\"]")).await;
    let addr = running.tls_addr().unwrap();
    let cfg = client_config(
        &pki.ca,
        Some(client_cert(&pki.ca, &["fleet:role:monitor"], 31)),
    );
    assert!(works(addr, cfg).await);
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_per_address_limit_applies() {
    let pki = Pki::new();
    let running = start(&pki.config("max_connections_per_ip = 1")).await;
    let addr = running.tls_addr().unwrap();
    let identity = client_cert(&pki.ca, &["fleet:role:monitor"], 40);
    let cfg = client_config(&pki.ca, Some(identity));
    let (_held, _) = session(addr, Arc::clone(&cfg), Role::Monitor)
        .await
        .unwrap();
    assert!(!works(addr, cfg).await);
    running.shutdown().await;
}

#[test]
fn the_tls_material_is_checked_at_load() {
    let pki = Pki::new();
    let cfg = |toml: &str| fleetd::config::Config::parse(toml).unwrap();

    let key = pki.dir.join("server.key");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = fleetd::tls::load(&cfg(&pki.config(""))).unwrap_err();
    assert!(err.to_string().contains("tls.key"), "{err}");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o400)).unwrap();
    fleetd::tls::load(&cfg(&pki.config(""))).unwrap();

    let other = pki
        .config("")
        .replace("device = \"dev-a\"", "device = \"dev-b\"");
    let err = fleetd::tls::load(&cfg(&other)).unwrap_err();
    assert!(err.to_string().contains("tls.cert"), "{err}");
}

/// Whether the server drops a fresh TCP connection without waiting out the handshake timeout.
async fn dropped_at_once(addr: SocketAddr) -> bool {
    use tokio::io::AsyncReadExt;
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(std::time::Duration::from_secs(3), tcp.read(&mut byte));
    matches!(read.await, Ok(Ok(0) | Err(_)))
}

/// Opens raw TCP connections that never start a handshake, and lets the server accept them.
async fn idle_peers(addr: SocketAddr, n: usize) -> Vec<TcpStream> {
    let mut held = Vec::new();
    for _ in 0..n {
        held.push(TcpStream::connect(addr).await.unwrap());
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    held
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_tls_peers_cannot_lock_out_unix_clients() {
    let pki = Pki::new();
    let toml = pki
        .config("max_connections = 2\nmax_connections_per_ip = 2\nmax_handshakes = 2")
        .replace("kill_grace_s = 1", "kill_grace_s = 1\nmax_connections = 1");
    let running = start(&toml).await;
    let addr = running.tls_addr().unwrap();
    let _held = idle_peers(addr, 2).await;
    assert!(dropped_at_once(addr).await, "the tls budget is full");

    let (mut conn, _) = connect(&running, Role::Monitor).await;
    let resp = request(&mut conn, 1, json!({"op": "status"})).await;
    assert!(
        matches!(resp.body, ResponseBody::Status(_)),
        "the unix budget is separate"
    );
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connections_past_max_handshakes_are_dropped_at_once() {
    let pki = Pki::new();
    let running = start(&pki.config("max_connections = 8\nmax_handshakes = 1")).await;
    let addr = running.tls_addr().unwrap();
    let held = idle_peers(addr, 1).await;
    assert!(
        dropped_at_once(addr).await,
        "one handshake is already in flight"
    );

    drop(held);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let cfg = client_config(
        &pki.ca,
        Some(client_cert(&pki.ca, &["fleet:role:monitor"], 50)),
    );
    let (_done, _) = session(addr, Arc::clone(&cfg), Role::Monitor)
        .await
        .unwrap();
    // A finished handshake gives its slot back while the connection stays open.
    assert!(works(addr, cfg).await);
    running.shutdown().await;
}
