use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use fleet_proto::{Connection, Hello, Origin, Response, ResponseBody, Role, SessionState};
use serde_json::json;
use tokio::net::UnixStream;

use crate::support::{
    TempDir, adapter, base_config, connect, connect_at, dispatch, error_code, eventually, next,
    request, start, wait,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn roles_are_enforced_per_request() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script("agent", "true\n");
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, "");
    let running = start(&toml).await;

    let (mut conn, granted) = connect(&running, Role::Monitor).await;
    assert_eq!(granted, Role::Monitor, "the grant never exceeds the ask");
    let resp = request(&mut conn, 7, json!({"op": "status"})).await;
    assert!(matches!(resp.body, ResponseBody::Status(_)));
    let spec = json!({"op": "dispatch", "adapter": "a", "workdir": root, "prompt": "p"});
    let resp = request(&mut conn, 8, spec.clone()).await;
    assert_eq!((resp.id, error_code(&resp.body)), (8, "unauthorized"));
    let resp = request(
        &mut conn,
        9,
        json!({"op": "cancel", "session": "d-1-00000000"}),
    )
    .await;
    assert_eq!((resp.id, error_code(&resp.body)), (9, "unauthorized"));
    // The refusal leaves the connection usable.
    let resp = request(&mut conn, 10, json!({"op": "sessions"})).await;
    assert!(matches!(resp.body, ResponseBody::Sessions { .. }));
    running.shutdown().await;

    let capped = toml.replace(
        "kill_grace_s = 1",
        "kill_grace_s = 1\n[unix]\nmax_role = \"monitor\"",
    );
    let running = start(&capped).await;
    let (mut conn, granted) = connect(&running, Role::Dispatch).await;
    assert_eq!(
        granted,
        Role::Monitor,
        "the grant never exceeds [unix] max_role"
    );
    let resp = request(&mut conn, 11, spec).await;
    assert_eq!(error_code(&resp.body), "unauthorized");
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_the_device() {
    let dir = TempDir::new();
    let running = start(&base_config(&dir)).await;
    let (mut conn, _) = connect(&running, Role::Monitor).await;
    let resp = request(&mut conn, 1, json!({"op": "status"})).await;
    let ResponseBody::Status(report) = resp.body else {
        panic!("{resp:?}")
    };
    assert_eq!(report.device.name.as_str(), "dev-a");
    assert!(report.device.cpus > 0);
    assert_eq!(report.device.fleetd_version, env!("CARGO_PKG_VERSION"));
    assert!(report.metrics.memory.total_bytes > 0);
    assert!(report.metrics.sampled_at > 0);
    assert_eq!(report.metrics.disks.len(), 1);
    assert_eq!(report.metrics.disks[0].mount, "/");
    assert!(report.metrics.gpus.is_empty());
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gpu_probe_is_parsed_and_bounded_by_its_timeout() {
    let dir = TempDir::new();
    let probe = dir.script(
        "probe",
        "echo '0, Example GPU, 50, 1024, 512, 61'\necho 'garbage'\necho '1, Other, [N/A], , , 40'\n",
    );
    let toml = base_config(&dir)
        + &format!(
            "[metrics]\ngpu_probe = [\"/bin/sh\", \"{}\"]\n",
            probe.display()
        );
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Monitor).await;
    let mut gpus = Vec::new();
    for _ in 0..50 {
        let resp = request(&mut conn, 1, json!({"op": "status"})).await;
        let ResponseBody::Status(report) = resp.body else {
            panic!("{resp:?}")
        };
        gpus = report.metrics.gpus;
        if !gpus.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(gpus.len(), 2);
    assert_eq!(gpus[0].name, "Example GPU");
    assert_eq!(gpus[0].memory_used_bytes, Some(512 << 20));
    assert_eq!(gpus[1].util_percent, None);
    running.shutdown().await;

    let slow = dir.script("slow", "sleep 30\necho '0, Late, 1, 1, 1, 1'\n");
    let toml = base_config(&dir)
        + &format!(
            "[metrics]\ngpu_probe = [\"/bin/sh\", \"{}\"]\ngpu_probe_timeout_s = 1\n",
            slow.display()
        );
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Monitor).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let resp = request(&mut conn, 1, json!({"op": "status"})).await;
    let ResponseBody::Status(report) = resp.body else {
        panic!("{resp:?}")
    };
    assert!(report.metrics.gpus.is_empty());
    running.shutdown().await;
}

/// A process discovery can match by name: a copy of fleetd, blocked in `--stdio` until its
/// stdin closes. (macOS kills a relocated copy of a system binary such as `sleep`.)
fn fake_agent(dir: &TempDir) -> PathBuf {
    let exe = dir.join("fakeagent");
    std::fs::copy(env!("CARGO_BIN_EXE_fleetd"), &exe).unwrap();
    exe
}

/// Retries while the new copy is still open for writing in a process another test forked.
fn spawn_retrying(cmd: &mut Command) -> std::process::Child {
    for _ in 0..50 {
        match cmd.spawn() {
            Ok(child) => return child,
            Err(e) if e.raw_os_error() == Some(26) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => panic!("{e}"),
        }
    }
    panic!("the executable stayed busy");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_reports_matching_processes_without_argv() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let exe = fake_agent(&dir);
    let config = dir.join("fleetd.toml");
    // A dispatched session whose processes also match is reported once, as dispatched.
    let agent = dir.script(
        "agent",
        &format!(
            "sleep 60 | {} --stdio --config {}\n",
            exe.display(),
            config.display()
        ),
    );
    let toml = base_config(&dir)
        + "[adapters.watch]\nmatch_exe = [\"fakeagent\"]\n"
        + &adapter("a", &agent, &[], &root, "");
    std::fs::write(&config, &toml).unwrap();
    let mut stray = spawn_retrying(
        Command::new(&exe)
            .args(["--stdio", "--role", "monitor", "--config"])
            .arg(&config)
            .current_dir(dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let resp = request(&mut conn, 2, json!({"op": "sessions"})).await;
    let ResponseBody::Sessions {
        sessions,
        truncated,
    } = resp.body
    else {
        panic!("{resp:?}")
    };
    assert!(!truncated);
    let found: Vec<_> = sessions
        .iter()
        .filter(|s| s.origin == Origin::Discovered)
        .collect();
    assert_eq!(found.len(), 1, "{sessions:?}");
    let s = found[0];
    assert_eq!(s.pid, Some(stray.id()));
    assert_eq!(s.adapter.as_str(), "watch");
    assert_eq!(s.state, SessionState::Running {});
    assert_eq!(
        s.summary, "fakeagent --stdio --role --config",
        "flag names only"
    );
    assert_eq!(s.exe.as_deref(), Some(exe.to_str().unwrap()));
    assert_eq!(s.cwd.as_deref(), Some(dir.path().to_str().unwrap()));
    assert!(s.id.as_str().starts_with(&format!("p-{}-", stray.id())));
    let mine: Vec<_> = sessions.iter().filter(|s| s.id.as_str() == id).collect();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].state, SessionState::Running {});

    let resp = request(&mut conn, 3, json!({"op": "logs", "session": s.id})).await;
    assert_eq!(error_code(&resp.body), "forbidden");
    let resp = request(&mut conn, 4, json!({"op": "cancel", "session": s.id})).await;
    assert_eq!(error_code(&resp.body), "forbidden");
    assert!(
        stray.try_wait().unwrap().is_none(),
        "discovery never signals"
    );

    stray.kill().unwrap();
    stray.wait().unwrap();
    request(&mut conn, 5, json!({"op": "cancel", "session": id})).await;
    wait(&mut conn, &id).await;
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_version_mismatch_is_reported_after_the_hello() {
    let dir = TempDir::new();
    let running = start(&base_config(&dir)).await;
    let mut conn = Connection::new(UnixStream::connect(running.socket()).await.unwrap());
    conn.send(&json!({"protocol_version": 99, "role": "superuser", "extra": true}))
        .await
        .unwrap();
    let hello: Hello = conn.recv().await.unwrap().unwrap();
    assert_eq!(hello.protocol_version, 1);
    assert_eq!(hello.role, Role::Monitor);
    let resp: Response = conn.recv().await.unwrap().unwrap();
    assert_eq!((resp.id, error_code(&resp.body)), (0, "version_mismatch"));
    assert!(conn.recv::<Response>().await.unwrap().is_none());
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_frame_closes_the_connection_with_an_error() {
    let dir = TempDir::new();
    let running = start(&base_config(&dir)).await;
    let (mut conn, _) = connect(&running, Role::Monitor).await;
    let resp = request(&mut conn, 1, json!({"op": "status", "verbose": true})).await;
    assert_eq!((resp.id, error_code(&resp.body)), (0, "bad_request"));
    assert!(conn.recv::<Response>().await.unwrap().is_none());

    let mut conn = Connection::new(UnixStream::connect(running.socket()).await.unwrap());
    conn.send(&json!(["not", "a", "hello"])).await.unwrap();
    let hello: Hello = conn.recv().await.unwrap().unwrap();
    assert_eq!(hello.role, Role::Monitor);
    let resp: Response = conn.recv().await.unwrap().unwrap();
    assert_eq!((resp.id, error_code(&resp.body)), (0, "bad_request"));
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_during_a_stream_breaks_the_connection() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script("agent", "exec sleep 60\n");
    let toml = base_config(&dir) + &adapter("a", &agent, &[], &root, "");
    let running = start(&toml).await;
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await
    .unwrap();
    conn.send(&json!({"id": 2, "body": {"op": "wait", "session": id}}))
        .await
        .unwrap();
    conn.send(&json!({"id": 3, "body": {"op": "status"}}))
        .await
        .unwrap();
    let resp = next(&mut conn).await;
    assert_eq!((resp.id, error_code(&resp.body)), (0, "bad_request"));

    // The session outlives the stream that watched it.
    let (mut conn, _) = connect(&running, Role::Dispatch).await;
    let resp = request(&mut conn, 4, json!({"op": "cancel", "session": id})).await;
    assert!(matches!(resp.body, ResponseBody::Cancelled { .. }));
    let (_, state) = wait(&mut conn, &id).await;
    assert_eq!(state, SessionState::Cancelled {});
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_limits_and_idle_timeout_apply() {
    let dir = TempDir::new();
    let toml = base_config(&dir).replace(
        "kill_grace_s = 1",
        "kill_grace_s = 1\nmax_connections = 1\nidle_timeout_s = 1",
    );
    let running = start(&toml).await;
    let (mut first, _) = connect(&running, Role::Monitor).await;
    let (mut second, granted) = connect(&running, Role::Dispatch).await;
    assert_eq!(granted, Role::Monitor);
    let resp = next(&mut second).await;
    assert_eq!((resp.id, error_code(&resp.body)), (0, "busy"));

    let closed = tokio::time::timeout(Duration::from_secs(5), first.recv::<Response>()).await;
    assert!(
        matches!(closed, Ok(Ok(None))),
        "an idle connection is closed"
    );
    // Its slot is free again.
    let (mut third, _) = connect(&running, Role::Monitor).await;
    let resp = request(&mut third, 1, json!({"op": "status"})).await;
    assert!(matches!(resp.body, ResponseBody::Status(_)));
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_daemon_on_the_socket_is_refused_and_a_stale_socket_replaced() {
    let dir = TempDir::new();
    let toml = base_config(&dir);
    let running = start(&toml).await;
    let cfg = fleetd::config::Config::parse(&toml).unwrap();
    let err = fleetd::server::start(cfg, None).await.err().unwrap();
    assert!(
        err.to_string().contains("another fleetd is running"),
        "{err}"
    );
    let socket = running.socket().to_owned();
    running.shutdown().await;

    // A socket file nothing listens on is a leftover.
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    let running = start(&toml).await;
    connect(&running, Role::Monitor).await;
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_state_dir_must_be_private() {
    let dir = TempDir::new();
    let state = dir.mkdir("state");
    std::fs::set_permissions(&state, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let cfg = fleetd::config::Config::parse(&base_config(&dir)).unwrap();
    let err = fleetd::server::start(cfg, None).await.err().unwrap();
    assert!(format!("{err:#}").contains("0700"), "{err:#}");
}

/// The binary, with a chosen environment, against which the agent's environment is checked;
/// then SIGTERM shuts it down cleanly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_child_gets_a_cleared_environment_and_sigterm_exits_cleanly() {
    let dir = TempDir::new();
    let root = dir.mkdir("root");
    let agent = dir.script("agent", "env | cut -d= -f1 | sort\n");
    let toml = base_config(&dir)
        + &adapter(
            "a",
            &agent,
            &[],
            &root,
            "env_allow = [\"FLEET_TEST_ALLOWED\", \"FLEET_TEST_UNSET\"]",
        );
    let config = dir.join("fleetd.toml");
    std::fs::write(&config, toml).unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_fleetd"))
        .arg("--config")
        .arg(&config)
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", "/usr/bin:/bin")
        .env("FLEET_TEST_ALLOWED", "yes")
        .env("FLEET_TEST_SECRET", "no")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = dir.join("state/fleetd.sock");
    eventually("the daemon's socket", || {
        std::os::unix::net::UnixStream::connect(&socket).is_ok()
    })
    .await;

    let (mut conn, _) = connect_at(&socket, Role::Dispatch).await;
    let id = dispatch(
        &mut conn,
        json!({"adapter": "a", "workdir": root, "prompt": "p"}),
    )
    .await
    .unwrap();
    let (out, _) = wait(&mut conn, &id).await;
    // The shell adds a few of its own.
    let shell: BTreeSet<&str> = ["PWD", "SHLVL", "_", "OLDPWD"].into();
    let names: BTreeSet<&str> = out.lines().filter(|n| !shell.contains(n)).collect();
    assert_eq!(
        names,
        [
            "FLEET_ADAPTER",
            "FLEET_SESSION",
            "FLEET_SESSION_TICK",
            "FLEET_TEST_ALLOWED",
            "HOME",
            "PATH"
        ]
        .into()
    );

    let pid = rustix::process::Pid::from_child(&daemon);
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    let status = tokio::task::spawn_blocking(move || daemon.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "{status}");
    assert!(!socket.exists(), "the socket is removed on shutdown");
    assert!(conn.recv::<Response>().await.unwrap_or(None).is_none());
}
