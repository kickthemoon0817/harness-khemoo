use std::process::Stdio;

use fleet_proto::{Connection, Hello, Response, ResponseBody, Role};
use serde_json::json;
use tokio::process::{Child, Command};

use crate::support::{TempDir, adapter, base_config, error_code, next, request, start};

fn relay(config: &std::path::Path, role: Option<&str>) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_fleetd"));
    cmd.arg("--stdio").arg("--config").arg(config);
    if let Some(role) = role {
        cmd.args(["--role", role]);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

fn stdio(child: &mut Child) -> impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + use<> {
    tokio::io::join(child.stdout.take().unwrap(), child.stdin.take().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_carries_the_protocol_with_its_role_capped() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script("agent", "echo relayed\n");
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, "");
    let config = dir.join("fleetd.toml");
    std::fs::write(&config, &toml).unwrap();
    let running = start(&toml).await;
    let spec = json!({"op": "dispatch", "adapter": "a", "workdir": root, "prompt": "p"});

    let mut child = relay(&config, None);
    let mut conn = Connection::new(stdio(&mut child));
    assert_eq!(
        conn.client_hello(Role::Dispatch).await.unwrap(),
        Role::Dispatch
    );
    let resp = request(&mut conn, 1, spec.clone()).await;
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
    assert_eq!(out, "relayed\n");
    drop(conn);
    assert!(
        child.wait().await.unwrap().success(),
        "the relay exits 0 when the client closes"
    );

    let mut child = relay(&config, Some("monitor"));
    let mut conn = Connection::new(stdio(&mut child));
    assert_eq!(
        conn.client_hello(Role::Dispatch).await.unwrap(),
        Role::Monitor
    );
    let resp = request(&mut conn, 3, spec.clone()).await;
    assert_eq!((resp.id, error_code(&resp.body)), (3, "unauthorized"));
    let resp = request(&mut conn, 4, json!({"op": "status"})).await;
    assert!(matches!(resp.body, ResponseBody::Status(_)));
    drop(conn);
    child.wait().await.unwrap();

    // [stdio] max_role caps it too, whatever --role says.
    let capped = toml.replace(
        "kill_grace_s = 1",
        "kill_grace_s = 1\n[stdio]\nmax_role = \"monitor\"",
    );
    std::fs::write(&config, capped).unwrap();
    let mut child = relay(&config, Some("dispatch"));
    let mut conn = Connection::new(stdio(&mut child));
    assert_eq!(
        conn.client_hello(Role::Dispatch).await.unwrap(),
        Role::Monitor
    );
    drop(conn);
    child.wait().await.unwrap();
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_daemon_the_relay_reports_unavailable() {
    let dir = TempDir::new();
    let config = dir.join("fleetd.toml");
    std::fs::write(&config, base_config(&dir)).unwrap();
    let mut child = relay(&config, None);
    let mut conn = Connection::new(stdio(&mut child));
    assert_eq!(
        conn.client_hello(Role::Dispatch).await.unwrap(),
        Role::Monitor
    );
    let resp: Response = next(&mut conn).await;
    assert_eq!((resp.id, error_code(&resp.body)), (0, "unavailable"));
    assert_eq!(child.wait().await.unwrap().code(), Some(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_reports_a_version_mismatch() {
    let dir = TempDir::new();
    let config = dir.join("fleetd.toml");
    std::fs::write(&config, base_config(&dir)).unwrap();
    let mut child = relay(&config, None);
    let mut conn = Connection::new(stdio(&mut child));
    conn.send(&json!({"protocol_version": 2, "role": "monitor"}))
        .await
        .unwrap();
    let hello: Hello = conn.recv().await.unwrap().unwrap();
    assert_eq!((hello.protocol_version, hello.role), (1, Role::Monitor));
    let resp: Response = conn.recv().await.unwrap().unwrap();
    assert_eq!((resp.id, error_code(&resp.body)), (0, "version_mismatch"));
    assert_eq!(child.wait().await.unwrap().code(), Some(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relay_whose_config_does_not_load_still_answers() {
    let dir = TempDir::new();
    let config = dir.join("fleetd.toml");
    std::fs::write(&config, "device = \"Not A Name\"\n").unwrap();
    let mut child = relay(&config, None);
    let mut conn = Connection::new(stdio(&mut child));
    assert_eq!(
        conn.client_hello(Role::Dispatch).await.unwrap(),
        Role::Monitor
    );
    let resp: Response = next(&mut conn).await;
    assert_eq!((resp.id, error_code(&resp.body)), (0, "internal"));
    assert_eq!(child.wait().await.unwrap().code(), Some(2));
}
