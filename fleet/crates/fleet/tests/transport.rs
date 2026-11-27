mod common;

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    FakeSsh, MockDevice, Pki, SESSION, TempDir, fake_ssh, make_pki, recorded_argv, serve_silent,
    serve_tls, serve_unix, session, tls_server_config, write_config,
};
use fleet::client::{Chunk, Client};
use fleet::query;
use fleet::registry::{Device, Registry};
use fleet_proto::{Connection, ErrorCode, Hello, RequestBody, Role, SessionState, WireError};
use serde_json::json;
use tokio::net::UnixListener;

fn load(path: &Path) -> Registry {
    let env: HashMap<&str, OsString> = HashMap::from([("HOME", OsString::from("/home/dev"))]);
    Registry::load(path, &|k| env.get(k).cloned()).unwrap_or_else(|e| panic!("{}", e.0))
}

fn local(name: &str, socket: &Path) -> String {
    format!(
        "[devices.{name}]\ntransport = \"local\"\nsocket = \"{}\"\n",
        socket.display()
    )
}

async fn connect(reg: &Registry, name: &str, role: Role) -> Result<Client, WireError> {
    Client::connect(reg.device(name).unwrap(), reg.timeouts, role).await
}

#[tokio::test]
async fn unix_status_sessions_and_streams() {
    let dir = TempDir::new();
    let mock = Arc::new(MockDevice::new("dev-a"));
    serve_unix(&dir.join("s"), Arc::clone(&mock));
    let reg = load(&write_config(dir.path(), &local("dev-a", &dir.join("s"))));

    let mut c = connect(&reg, "dev-a", Role::Monitor).await.unwrap();
    assert_eq!(c.status().await.unwrap(), mock.status);
    assert_eq!(c.sessions().await.unwrap(), (mock.sessions.clone(), false));
    c.stream(RequestBody::Logs {
        session: SESSION.parse().unwrap(),
        lines: fleet_proto::LogLines::default(),
        follow: true,
    })
    .await
    .unwrap();
    let mut chunks = Vec::new();
    loop {
        let chunk = c.next_chunk().await.unwrap();
        let end = matches!(chunk, Chunk::End(_));
        chunks.push(chunk);
        if end {
            break;
        }
    }
    assert_eq!(
        chunks,
        [
            Chunk::Data("hello ".into()),
            Chunk::Data("world\n".into()),
            Chunk::End(SessionState::Exited { code: 0 }),
        ]
    );
    // The connection is still good after a stream.
    assert_eq!(c.status().await.unwrap(), mock.status);

    let mut d = connect(&reg, "dev-a", Role::Dispatch).await.unwrap();
    let id = SESSION.parse().unwrap();
    assert_eq!(d.cancel(id).await.unwrap().as_str(), SESSION);
    assert_eq!(mock.roles(), [Role::Monitor, Role::Dispatch]);
}

#[tokio::test]
async fn a_refusal_after_hello_is_reported_with_its_code() {
    let dir = TempDir::new();
    let mut mock = MockDevice::new("dev-a");
    mock.refuse = Some(WireError::new(ErrorCode::Unavailable, "no daemon"));
    serve_unix(&dir.join("s"), Arc::new(mock));
    let reg = load(&write_config(dir.path(), &local("dev-a", &dir.join("s"))));
    let mut c = connect(&reg, "dev-a", Role::Monitor).await.unwrap();
    let e = c.status().await.unwrap_err();
    assert_eq!(e, WireError::new(ErrorCode::Unavailable, "no daemon"));
}

#[tokio::test]
async fn unix_failures_map_to_hub_codes() {
    let dir = TempDir::new();
    serve_silent(&dir.join("quiet"));
    let text = format!(
        "connect_timeout_s = 1\n{}{}",
        local("gone", &dir.join("absent")),
        local("quiet", &dir.join("quiet"))
    );
    let reg = load(&write_config(dir.path(), &text));
    let e = connect(&reg, "gone", Role::Monitor).await.err().unwrap();
    assert_eq!(e.code, ErrorCode::Unreachable, "{e}");
    let started = Instant::now();
    let e = connect(&reg, "quiet", Role::Monitor).await.err().unwrap();
    assert_eq!(e.code, ErrorCode::Timeout, "{e}");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn a_version_mismatch_is_reported() {
    let dir = TempDir::new();
    let listener = UnixListener::bind(dir.join("s")).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = Connection::new(stream);
        let _: Option<serde_json::Value> = conn.recv().await.unwrap();
        conn.send(&Hello {
            protocol_version: 99,
            role: Role::Monitor,
        })
        .await
        .unwrap();
    });
    let reg = load(&write_config(dir.path(), &local("dev-a", &dir.join("s"))));
    let e = connect(&reg, "dev-a", Role::Monitor).await.err().unwrap();
    assert_eq!(e.code, ErrorCode::VersionMismatch, "{e}");
}

fn tls_config(pki: &Pki, ca: &Path, address: &str, server_name: Option<&str>) -> String {
    let mut text = format!(
        "[tls]\nca = \"{}\"\ncert = \"{}\"\nkey = \"{}\"\n[devices.dev-a]\ntransport = \"tls\"\naddress = \"{address}\"\n",
        ca.display(),
        pki.client_cert.display(),
        pki.client_key.display(),
    );
    if let Some(n) = server_name {
        let _ = writeln!(text, "server_name = \"{n}\"");
    }
    text
}

#[tokio::test]
async fn tls_loopback_with_mutual_authentication() {
    let dir = TempDir::new();
    let pki = make_pki(&dir.join("pki"));
    let mock = Arc::new(MockDevice::new("dev-a"));
    let addr = serve_tls(tls_server_config(&pki), Arc::clone(&mock)).await;
    let reg = load(&write_config(
        dir.path(),
        &tls_config(&pki, &pki.ca, &addr.to_string(), None),
    ));
    let mut c = connect(&reg, "dev-a", Role::Monitor).await.unwrap();
    assert_eq!(c.status().await.unwrap(), mock.status);
}

#[tokio::test]
async fn tls_refuses_a_server_from_another_ca() {
    let dir = TempDir::new();
    let pki = make_pki(&dir.join("pki"));
    let other = make_pki(&dir.join("other"));
    let addr = serve_tls(tls_server_config(&pki), Arc::new(MockDevice::new("dev-a"))).await;
    let reg = load(&write_config(
        dir.path(),
        &tls_config(&pki, &other.ca, &addr.to_string(), None),
    ));
    let e = connect(&reg, "dev-a", Role::Monitor).await.err().unwrap();
    assert_eq!(e.code, ErrorCode::Unreachable);
    assert!(e.message.contains("UnknownIssuer"), "{e}");
}

#[tokio::test]
async fn tls_refuses_a_server_with_another_name() {
    let dir = TempDir::new();
    let pki = make_pki(&dir.join("pki"));
    let addr = serve_tls(tls_server_config(&pki), Arc::new(MockDevice::new("dev-a"))).await;
    let text = tls_config(&pki, &pki.ca, &addr.to_string(), Some("dev-b"));
    let reg = load(&write_config(dir.path(), &text));
    let e = connect(&reg, "dev-a", Role::Monitor).await.err().unwrap();
    assert_eq!(e.code, ErrorCode::Unreachable);
    assert!(e.message.contains("not valid for name"), "{e}");
}

#[tokio::test]
async fn tls_client_without_a_trusted_certificate_is_refused_by_the_server() {
    let dir = TempDir::new();
    let pki = make_pki(&dir.join("pki"));
    let other = make_pki(&dir.join("other"));
    let addr = serve_tls(tls_server_config(&pki), Arc::new(MockDevice::new("dev-a"))).await;
    // Trusts the right CA but presents a client certificate from another one.
    let text = format!(
        "[tls]\nca = \"{}\"\ncert = \"{}\"\nkey = \"{}\"\n[devices.dev-a]\ntransport = \"tls\"\naddress = \"{addr}\"\n",
        pki.ca.display(),
        other.client_cert.display(),
        other.client_key.display(),
    );
    let reg = load(&write_config(dir.path(), &text));
    let e = connect(&reg, "dev-a", Role::Monitor).await.err().unwrap();
    assert_eq!(e.code, ErrorCode::Unreachable, "{e}");
}

fn ssh_config(ssh: &Path) -> String {
    format!(
        "ssh = \"{}\"\n[devices.dev-c]\ntransport = \"ssh\"\ntarget = \"dev@dev-c.example.com\"\nssh_options = [\"Port=2222\"]\n",
        ssh.display()
    )
}

#[tokio::test]
async fn ssh_speaks_the_protocol_over_the_child_stdio() {
    let dir = TempDir::new();
    let ssh = fake_ssh(dir.path(), FakeSsh::Mock("dev-c"));
    let reg = load(&write_config(dir.path(), &ssh_config(&ssh)));
    let mut c = connect(&reg, "dev-c", Role::Monitor).await.unwrap();
    assert_eq!(c.status().await.unwrap().device.name.as_str(), "dev-c");
    assert_eq!(c.sessions().await.unwrap().0.len(), 1);
    assert_eq!(
        recorded_argv(dir.path()),
        [
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
            "-o",
            "Port=2222",
            "--",
            "dev@dev-c.example.com",
            "fleetd",
            "--stdio"
        ]
    );
}

/// Runs the real ssh's `-G` on fleet's argv under a config that turns every forwarding on,
/// and checks fleet's options win. Skipped where ssh -G cannot run.
#[tokio::test]
async fn ssh_config_cannot_turn_forwarding_back_on() {
    let dir = TempDir::new();
    let ssh = fake_ssh(dir.path(), FakeSsh::Mock("dev-c"));
    let reg = load(&write_config(dir.path(), &ssh_config(&ssh)));
    connect(&reg, "dev-c", Role::Monitor).await.unwrap();
    let config = dir.join("ssh_config");
    std::fs::write(
        &config,
        "Host *\n  ForwardAgent yes\n  ForwardX11 yes\n  LocalForward 1234 localhost:22\n  \
         RemoteForward 2345 localhost:22\n  DynamicForward 3456\n  PermitLocalCommand yes\n  \
         LocalCommand touch pwned\n",
    )
    .unwrap();
    let resolve = |args: &[String]| {
        std::process::Command::new("ssh")
            .arg("-F")
            .arg(&config)
            .arg("-G")
            .args(args)
            .output()
    };
    // ssh -G also fails where the uid has no passwd entry, as in some containers.
    match resolve(&["probe".to_owned()]) {
        Ok(probe) if probe.status.success() => {}
        probe => {
            eprintln!("skipped: ssh -G cannot run here: {probe:?}");
            return;
        }
    }
    let out = resolve(&recorded_argv(dir.path())).unwrap();
    assert!(out.status.success(), "{out:?}");
    let resolved = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = resolved.lines().collect();
    for want in [
        "forwardagent no",
        "forwardx11 no",
        "clearallforwardings yes",
        "permitlocalcommand no",
        "port 2222",
    ] {
        assert!(lines.contains(&want), "{want:?} not in {resolved}");
    }
    for forward in ["localforward", "remoteforward", "dynamicforward"] {
        assert!(
            !lines.iter().any(|l| l.starts_with(forward)),
            "{forward} in {resolved}"
        );
    }
}

#[tokio::test]
async fn ssh_failure_reports_its_exit_status_and_stderr() {
    let dir = TempDir::new();
    let ssh = fake_ssh(
        dir.path(),
        FakeSsh::Fail("dev@dev-c: Permission denied (publickey)."),
    );
    let reg = load(&write_config(dir.path(), &ssh_config(&ssh)));
    let e = connect(&reg, "dev-c", Role::Monitor).await.err().unwrap();
    assert_eq!(
        e,
        WireError::new(
            ErrorCode::Unreachable,
            "ssh exited 255: dev@dev-c: Permission denied (publickey)."
        )
    );
}

#[tokio::test]
async fn ssh_reports_a_handshake_broken_by_the_far_side() {
    let dir = TempDir::new();
    let ssh = fake_ssh(dir.path(), FakeSsh::Banner);
    let reg = load(&write_config(dir.path(), &ssh_config(&ssh)));
    let e = connect(&reg, "dev-c", Role::Monitor).await.err().unwrap();
    assert_eq!(e.code, ErrorCode::Unreachable);
    assert!(e.message.starts_with("handshake over ssh: "), "{e}");
}

#[tokio::test]
async fn ssh_that_cannot_run_is_unreachable() {
    let dir = TempDir::new();
    let reg = load(&write_config(
        dir.path(),
        &ssh_config(&dir.join("no-such-ssh")),
    ));
    let e = connect(&reg, "dev-c", Role::Monitor).await.err().unwrap();
    assert_eq!(e.code, ErrorCode::Unreachable);
    assert!(e.message.contains("cannot run"), "{e}");
}

fn devices(reg: &Registry) -> Vec<&Device> {
    reg.devices.iter().collect()
}

/// Two live devices around a dead one, over unix and ssh.
fn mixed_registry(dir: &TempDir) -> Registry {
    let mut a = MockDevice::new("dev-a");
    a.sessions = vec![
        session(
            "d-1700000500-00000002",
            1_700_000_500,
            SessionState::Running {},
        ),
        session(
            "d-1700000000-00000001",
            1_700_000_000,
            SessionState::Exited { code: 0 },
        ),
    ];
    a.truncated = true;
    serve_unix(&dir.join("a"), Arc::new(a));
    let ssh = fake_ssh(dir.path(), FakeSsh::Mock("dev-c"));
    let text = format!(
        "ssh = \"{}\"\n{}{}[devices.dev-c]\ntransport = \"ssh\"\ntarget = \"dev-c\"\n",
        ssh.display(),
        local("dev-a", &dir.join("a")),
        local("dev-b", &dir.join("dead")),
    );
    load(&write_config(dir.path(), &text))
}

#[tokio::test]
async fn status_isolates_a_dead_device_and_matches_the_schema() {
    let dir = TempDir::new();
    let reg = mixed_registry(&dir);
    let doc = query::status(&devices(&reg), reg.timeouts, 1_700_000_000).await;
    let mut value = serde_json::to_value(&doc).unwrap();
    let message = value["devices"][1]["error"]["message"].take();
    assert!(message.as_str().unwrap().contains("dead"), "{message}");

    let dev_a = MockDevice::new("dev-a");
    let dev_c = MockDevice::new("dev-c");
    let s = |id: &str, at, state| serde_json::to_value(session(id, at, state)).unwrap();
    let expected = json!({
        "version": 1,
        "generated_at": 1_700_000_000,
        "devices": [
            {
                "name": "dev-a", "transport": "local", "ok": true, "error": null,
                "status": serde_json::to_value(&dev_a.status).unwrap(),
                "sessions": [
                    s("d-1700000500-00000002", 1_700_000_500, SessionState::Running {}),
                    s("d-1700000000-00000001", 1_700_000_000, SessionState::Exited { code: 0 }),
                ],
                "sessions_truncated": true
            },
            {
                "name": "dev-b", "transport": "local", "ok": false,
                "error": {"code": "unreachable", "message": null},
                "status": null, "sessions": [], "sessions_truncated": false
            },
            {
                "name": "dev-c", "transport": "ssh", "ok": true, "error": null,
                "status": serde_json::to_value(&dev_c.status).unwrap(),
                "sessions": serde_json::to_value(&dev_c.sessions).unwrap(),
                "sessions_truncated": false
            }
        ]
    });
    assert_eq!(value, expected);
    // The nested status keeps the documented shape, not just whatever the mock sends.
    assert_eq!(
        value["devices"][0]["status"]["device"],
        json!({"name": "dev-a", "hostname": "dev-a.example.com", "os": "Ubuntu 24.04",
               "kernel": "6.8.0", "arch": "x86_64", "cpus": 16, "fleetd_version": "0.1.0"})
    );
    assert_eq!(
        value["devices"][0]["status"]["adapters"],
        json!([{"name": "claude", "models": ["sonnet", "opus"], "max_concurrent": 2, "running": 1}])
    );
}

#[tokio::test]
async fn sessions_are_ordered_by_device_then_start_and_match_the_schema() {
    let dir = TempDir::new();
    let reg = mixed_registry(&dir);
    let doc = query::sessions(&devices(&reg), reg.timeouts, 1_700_000_000, false).await;
    let mut value = serde_json::to_value(&doc).unwrap();
    value["errors"][0]["error"]["message"].take();
    let s = |id: &str, at, state| serde_json::to_value(session(id, at, state)).unwrap();
    assert_eq!(
        value,
        json!({
            "version": 1,
            "generated_at": 1_700_000_000,
            "sessions": [
                {"device": "dev-a", "session": s("d-1700000000-00000001", 1_700_000_000, SessionState::Exited { code: 0 })},
                {"device": "dev-a", "session": s("d-1700000500-00000002", 1_700_000_500, SessionState::Running {})},
                {"device": "dev-c", "session": s(SESSION, 1_700_000_000, SessionState::Running {})},
            ],
            "errors": [{"device": "dev-b", "error": {"code": "unreachable", "message": null}}],
            "truncated": ["dev-a"]
        })
    );
    let running = query::sessions(&devices(&reg), reg.timeouts, 0, true).await;
    assert_eq!(running.sessions.len(), 2);
    assert!(
        running
            .sessions
            .iter()
            .all(|s| s.session.state.is_running())
    );
}

#[tokio::test]
async fn devices_are_asked_concurrently() {
    let dir = TempDir::new();
    let mut text = String::new();
    for i in 0..6 {
        let mut mock = MockDevice::new(&format!("dev-{i}"));
        mock.delay = Duration::from_millis(500);
        let socket = dir.join(format!("s{i}"));
        serve_unix(&socket, Arc::new(mock));
        text.push_str(&local(&format!("dev-{i}"), &socket));
    }
    let reg = load(&write_config(dir.path(), &text));
    let started = Instant::now();
    let doc = query::status(&devices(&reg), reg.timeouts, 0).await;
    assert!(doc.devices.iter().all(|d| d.ok));
    let names: Vec<&str> = doc.devices.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        ["dev-0", "dev-1", "dev-2", "dev-3", "dev-4", "dev-5"]
    );
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
}
