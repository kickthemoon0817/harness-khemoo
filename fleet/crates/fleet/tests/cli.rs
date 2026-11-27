//! The `fleet` binary: exit codes, stdin handling, output formats and placement.

mod common;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{
    FakeSsh, MockDevice, SESSION, TempDir, fake_ssh, run_fleet, serve_unix, status_report,
    write_config,
};
use fleet_proto::{DispatchSpec, ErrorCode, Prompt, RequestBody, SessionState, WireError};
use serde_json::{Value, json};

/// Serves each mock on its own socket and writes a registry listing them in order.
fn registry(dir: &TempDir, mocks: Vec<(&str, MockDevice)>) -> (PathBuf, Vec<Arc<MockDevice>>) {
    let mut text = String::new();
    let mut served = Vec::new();
    for (name, mock) in mocks {
        let socket = dir.join(name);
        let mock = Arc::new(mock);
        serve_unix(&socket, Arc::clone(&mock));
        served.push(mock);
        let _ = writeln!(
            text,
            "[devices.{name}]\ntransport = \"local\"\nsocket = \"{}\"",
            socket.display()
        );
    }
    (write_config(dir.path(), &text), served)
}

fn dispatch_args<'a>(device: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "dispatch",
        "--device",
        device,
        "--adapter",
        "claude",
        "--workdir",
        "/srv/work/repo-42",
    ];
    args.extend_from_slice(extra);
    args
}

fn dispatched_spec(mock: &MockDevice) -> DispatchSpec {
    mock.requests()
        .into_iter()
        .find_map(|r| match r {
            RequestBody::Dispatch(spec) => Some(spec),
            _ => None,
        })
        .expect("no dispatch request")
}

#[tokio::test(flavor = "multi_thread")]
async fn status_reports_partial_results_and_exits_1() {
    let dir = TempDir::new();
    let (config, _) = registry(&dir, vec![("dev-a", MockDevice::new("dev-a"))]);
    let text = std::fs::read_to_string(&config).unwrap()
        + &format!(
            "[devices.dev-b]\ntransport = \"local\"\nsocket = \"{}\"\n",
            dir.join("dead").display()
        );
    std::fs::write(&config, text).unwrap();

    let run = run_fleet(&config, &["status", "--json"], b"", &[]).await;
    assert_eq!(run.code, 1, "{}", run.stderr);
    let doc: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["devices"][0]["name"], "dev-a");
    assert_eq!(doc["devices"][0]["ok"], true);
    assert_eq!(doc["devices"][1]["name"], "dev-b");
    assert_eq!(doc["devices"][1]["ok"], false);
    assert_eq!(doc["devices"][1]["error"]["code"], "unreachable");
    assert!(
        run.stderr.starts_with("fleet: dev-b: unreachable: "),
        "{}",
        run.stderr
    );

    let run = run_fleet(&config, &["status", "--device", "dev-a"], b"", &[]).await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stdout.starts_with("DEVICE"), "{}", run.stdout);
    assert!(run.stdout.contains("claude 1/2"), "{}", run.stdout);
    assert!(!run.stdout.contains("dev-b"));

    let run = run_fleet(&config, &["status", "--device", "dev-x"], b"", &[]).await;
    assert_eq!(run.code, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_json_and_table() {
    let dir = TempDir::new();
    let (config, _) = registry(&dir, vec![("dev-a", MockDevice::new("dev-a"))]);
    let run = run_fleet(&config, &["sessions", "--json", "--running"], b"", &[]).await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    let doc: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(doc["sessions"][0]["device"], "dev-a");
    assert_eq!(doc["sessions"][0]["session"]["id"], SESSION);
    assert_eq!(doc["errors"], json!([]));
    assert_eq!(doc["truncated"], json!([]));

    let run = run_fleet(&config, &["sessions"], b"", &[]).await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    let line = run.stdout.lines().nth(1).unwrap();
    for field in [
        "dev-a",
        SESSION,
        "claude",
        "running",
        "4242",
        "sonnet",
        "42",
        "/srv/work/repo-42",
    ] {
        assert!(line.contains(field), "{field} missing from {line:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn device_strings_are_cleaned_for_the_terminal() {
    let dir = TempDir::new();
    let mut mock = MockDevice::new("dev-a");
    mock.sessions[0].cwd = Some("/srv/\x1b]0;owned\x07".into());
    let (config, _) = registry(&dir, vec![("dev-a", mock)]);
    let run = run_fleet(&config, &["sessions"], b"", &[]).await;
    assert!(!run.stdout.contains('\x1b'), "{:?}", run.stdout);
    assert!(run.stdout.contains("/srv/\u{FFFD}]0;owned\u{FFFD}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_sends_the_request_and_prints_the_session() {
    let dir = TempDir::new();
    let (config, mocks) = registry(&dir, vec![("dev-a", MockDevice::new("dev-a"))]);
    let args = dispatch_args(
        "dev-a",
        &[
            "--model",
            "opus",
            "--timeout",
            "600",
            "--label",
            "issue=42",
            "--json",
        ],
    );
    let run = run_fleet(&config, &args, b"Fix the test.\n", &[]).await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    let out: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(out, json!({"device": "dev-a", "session": SESSION}));

    let spec = dispatched_spec(&mocks[0]);
    assert_eq!(spec.adapter.as_str(), "claude");
    assert_eq!(spec.workdir.as_str(), "/srv/work/repo-42");
    assert_eq!(spec.model.unwrap().as_str(), "opus");
    assert_eq!(spec.prompt.as_str(), "Fix the test.\n");
    assert_eq!(spec.timeout_s.unwrap().get(), 600);
    assert_eq!(spec.labels.get("issue"), Some("42"));
    assert_eq!(mocks[0].roles(), [fleet_proto::Role::Dispatch]);
}

async fn wait_code(end: SessionState) -> (i32, String, String) {
    let dir = TempDir::new();
    let mut mock = MockDevice::new("dev-a");
    mock.end = end;
    let (config, _) = registry(&dir, vec![("dev-a", mock)]);
    let run = run_fleet(&config, &dispatch_args("dev-a", &["--wait"]), b"go", &[]).await;
    (run.code, run.stdout, run.stderr)
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_wait_streams_output_and_exits_with_the_agent_status() {
    let (code, stdout, stderr) = wait_code(SessionState::Exited { code: 3 }).await;
    assert_eq!(code, 3, "{stderr}");
    assert_eq!(stdout, "hello world\n");
    assert!(stderr.contains(SESSION), "{stderr}");

    assert_eq!(wait_code(SessionState::Exited { code: 0 }).await.0, 0);
    assert_eq!(
        wait_code(SessionState::Signalled { signal: 9 }).await.0,
        137
    );
    assert_eq!(wait_code(SessionState::TimedOut {}).await.0, 124);
    assert_eq!(wait_code(SessionState::Cancelled {}).await.0, 130);
    let (code, _, stderr) = wait_code(SessionState::Failed {
        message: "spawn failed".into(),
    })
    .await;
    assert_eq!(code, 125);
    assert!(stderr.contains("spawn failed"), "{stderr}");
    // A device claiming an impossible status is not passed through as success.
    assert_eq!(wait_code(SessionState::Exited { code: 256 }).await.0, 125);
    assert_eq!(wait_code(SessionState::Running {}).await.0, 125);
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_wait_json_prints_one_object_per_line() {
    let dir = TempDir::new();
    let (config, _) = registry(&dir, vec![("dev-a", MockDevice::new("dev-a"))]);
    let run = run_fleet(
        &config,
        &dispatch_args("dev-a", &["--wait", "--json"]),
        b"go",
        &[],
    )
    .await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    let lines: Vec<Value> = run
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        lines,
        [
            json!({"data": "hello "}),
            json!({"data": "world\n"}),
            json!({"state": {"kind": "exited", "code": 0}}),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_dispatch_fails_with_its_code() {
    let dir = TempDir::new();
    let mut mock = MockDevice::new("dev-a");
    mock.dispatch_error = Some(WireError::new(
        ErrorCode::Forbidden,
        "workdir outside the roots",
    ));
    let (config, _) = registry(&dir, vec![("dev-a", mock)]);
    let run = run_fleet(&config, &dispatch_args("dev-a", &[]), b"go", &[]).await;
    assert_eq!(run.code, 1);
    assert_eq!(
        run.stderr,
        "fleet: dev-a: forbidden: workdir outside the roots\n"
    );
    let run = run_fleet(&config, &dispatch_args("dev-a", &["--wait"]), b"go", &[]).await;
    assert_eq!(run.code, 125);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_prompt_is_checked_before_any_connection() {
    let dir = TempDir::new();
    let (config, mocks) = registry(&dir, vec![("dev-a", MockDevice::new("dev-a"))]);
    let args = dispatch_args("dev-a", &["--wait"]);

    let oversize = vec![b'x'; Prompt::MAX_LEN + 1];
    let run = run_fleet(&config, &args, &oversize, &[]).await;
    assert_eq!(run.code, 2);
    assert!(
        run.stderr.contains("exceeds 262144 bytes"),
        "{}",
        run.stderr
    );

    for bad in [&b""[..], b"\xff\xfe", b"ring \x07 the bell"] {
        let run = run_fleet(&config, &args, bad, &[]).await;
        assert_eq!(run.code, 2, "{bad:?}: {}", run.stderr);
    }
    assert_eq!(mocks[0].connections(), 0);

    let exact = vec![b'x'; Prompt::MAX_LEN];
    let run = run_fleet(&config, &dispatch_args("dev-a", &[]), &exact, &[]).await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        dispatched_spec(&mocks[0]).prompt.as_str().len(),
        Prompt::MAX_LEN
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_arguments_are_usage_errors() {
    let dir = TempDir::new();
    let (config, _) = registry(&dir, vec![("dev-a", MockDevice::new("dev-a"))]);
    for extra in [
        &["--workdir", "relative/path"][..],
        &["--model", "-rf"],
        &["--timeout", "0"],
        &["--label", "Issue=42"],
        &["--label", "noequals"],
    ] {
        let run = run_fleet(&config, &dispatch_args("dev-a", extra), b"go", &[]).await;
        assert_eq!(run.code, 2, "{extra:?}: {}", run.stderr);
    }
    let run = run_fleet(&config, &dispatch_args("nowhere", &[]), b"go", &[]).await;
    assert_eq!(run.code, 2);
}

fn adapters(cpus: u32, load: f64, max: u32, running: u32) -> MockDevice {
    let mut mock = MockDevice::new("x");
    mock.status = status_report("x", cpus, load, &[("claude", &["sonnet"], max, running)]);
    mock
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_picks_the_least_loaded_device_with_capacity() {
    let dir = TempDir::new();
    let (config, mocks) = registry(
        &dir,
        vec![
            ("full", adapters(16, 0.0, 1, 1)),
            ("busy", adapters(4, 3.0, 2, 0)),
            ("idle", adapters(16, 1.0, 2, 1)),
        ],
    );
    let run = run_fleet(&config, &dispatch_args("auto", &["--json"]), b"go", &[]).await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    let out: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(out["device"], "idle");
    assert!(
        mocks[2]
            .requests()
            .iter()
            .any(|r| matches!(r, RequestBody::Dispatch(_)))
    );
    assert!(
        !mocks[1]
            .requests()
            .iter()
            .any(|r| matches!(r, RequestBody::Dispatch(_)))
    );

    // A model no device allows.
    let run = run_fleet(
        &config,
        &dispatch_args("auto", &["--model", "opus"]),
        b"go",
        &[],
    )
    .await;
    assert_eq!(run.code, 1);
    assert!(
        run.stderr.starts_with("fleet: not_found: "),
        "{}",
        run.stderr
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_moves_on_when_a_device_refuses() {
    let dir = TempDir::new();
    let mut first = adapters(16, 0.0, 1, 0);
    first.dispatch_error = Some(WireError::new(ErrorCode::Busy, "at max_concurrent"));
    let (config, _) = registry(
        &dir,
        vec![("first", first), ("second", adapters(16, 8.0, 1, 0))],
    );
    let run = run_fleet(&config, &dispatch_args("auto", &["--json"]), b"go", &[]).await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    let out: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(out["device"], "second");
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_reports_busy_and_unreachable() {
    let dir = TempDir::new();
    let (config, _) = registry(&dir, vec![("full", adapters(16, 0.0, 1, 1))]);
    let run = run_fleet(&config, &dispatch_args("auto", &["--wait"]), b"go", &[]).await;
    assert_eq!(run.code, 125);
    assert!(run.stderr.starts_with("fleet: busy: "), "{}", run.stderr);

    let dead = TempDir::new();
    let config = write_config(
        dead.path(),
        &format!(
            "[devices.gone]\ntransport = \"local\"\nsocket = \"{}\"\n[devices.skipped]\ntransport = \"local\"\nauto = false\nsocket = \"{}\"\n",
            dead.join("absent").display(),
            dir.join("full").display(),
        ),
    );
    let run = run_fleet(&config, &dispatch_args("auto", &[]), b"go", &[]).await;
    assert_eq!(run.code, 1);
    assert!(
        run.stderr.starts_with("fleet: unreachable: "),
        "{}",
        run.stderr
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn logs_and_cancel() {
    let dir = TempDir::new();
    let (config, mocks) = registry(&dir, vec![("dev-a", MockDevice::new("dev-a"))]);
    let run = run_fleet(
        &config,
        &["logs", "dev-a", SESSION, "--lines", "5", "--follow"],
        b"",
        &[],
    )
    .await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.stdout, "hello world\n");
    assert!(mocks[0].requests().contains(&RequestBody::Logs {
        session: SESSION.parse().unwrap(),
        lines: fleet_proto::LogLines::try_from(5).unwrap(),
        follow: true,
    }));

    let run = run_fleet(&config, &["logs", "dev-a", SESSION, "--json"], b"", &[]).await;
    assert_eq!(
        run.stdout.lines().last().unwrap(),
        r#"{"state":{"code":0,"kind":"exited"}}"#
    );

    let run = run_fleet(&config, &["cancel", "dev-a", SESSION, "--json"], b"", &[]).await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    let out: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(out, json!({"device": "dev-a", "session": SESSION}));

    let run = run_fleet(&config, &["logs", "dev-a", "bad id"], b"", &[]).await;
    assert_eq!(run.code, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn ssh_is_found_on_path() {
    let dir = TempDir::new();
    let bin = dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    fake_ssh(&bin, FakeSsh::Mock("dev-c"));
    let config = write_config(
        dir.path(),
        "[devices.dev-c]\ntransport = \"ssh\"\ntarget = \"dev-c\"\n",
    );
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = run_fleet(&config, &["status", "--json"], b"", &[("PATH", &path)]).await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    let doc: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(doc["devices"][0]["transport"], "ssh");
    assert_eq!(doc["devices"][0]["status"]["device"]["name"], "dev-c");
}

#[tokio::test(flavor = "multi_thread")]
async fn config_errors_exit_2() {
    let dir = TempDir::new();
    let config = write_config(dir.path(), "colour = \"blue\"\n");
    let run = run_fleet(&config, &["status"], b"", &[]).await;
    assert_eq!(run.code, 2);
    assert!(run.stderr.contains("colour"), "{}", run.stderr);
    let run = run_fleet(&dir.join("absent.toml"), &["sessions", "--json"], b"", &[]).await;
    assert_eq!(run.code, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn pki_writes_beside_the_config_by_default() {
    let dir = TempDir::new();
    let config = dir.join("fleet.toml");
    let run = run_fleet(&config, &["pki", "init"], b"", &[]).await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(dir.join("pki/ca.pem").exists() && dir.join("pki/ca.key").exists());
    assert!(run.stdout.contains("serial "), "{}", run.stdout);
    let run = run_fleet(
        &config,
        &["pki", "issue-client", "hub", "--role", "monitor"],
        b"",
        &[],
    )
    .await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    let run = run_fleet(&config, &["pki", "init"], b"", &[]).await;
    assert_eq!(run.code, 1);
    assert!(
        run.stderr.contains("refusing to overwrite"),
        "{}",
        run.stderr
    );
    let run = run_fleet(
        &config,
        &["pki", "issue-server", "dev-a", "--days", "0"],
        b"",
        &[],
    )
    .await;
    assert_eq!(run.code, 2);
    assert!(!Path::new(&dir.join("pki/dev-a.pem")).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signal_while_waiting_cancels_the_session() {
    let dir = TempDir::new();
    let mut mock = MockDevice::new("dev-a");
    mock.hang_stream = true;
    let (config, mocks) = registry(&dir, vec![("dev-a", mock)]);
    let prompt = dir.join("prompt");
    std::fs::write(&prompt, "go").unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_fleet"))
        .arg("--config")
        .arg(&config)
        .args(dispatch_args("dev-a", &["--wait"]))
        .stdin(std::fs::File::open(&prompt).unwrap())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let waiting = || {
        mocks[0]
            .requests()
            .iter()
            .any(|r| matches!(r, RequestBody::Wait { .. }))
    };
    for _ in 0..200 {
        if waiting() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(waiting(), "fleet never sent wait");
    let pid = child.id().unwrap().to_string();
    let killed = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    assert!(killed.success());
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(143));
    assert!(mocks[0].requests().contains(&RequestBody::Cancel {
        session: SESSION.parse().unwrap()
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_does_not_retry_a_dispatch_that_may_have_started() {
    let dir = TempDir::new();
    let mut first = adapters(16, 0.0, 1, 0);
    first.drop_dispatch = true;
    let (config, mocks) = registry(
        &dir,
        vec![("first", first), ("second", adapters(16, 8.0, 1, 0))],
    );
    let run = run_fleet(&config, &dispatch_args("auto", &[]), b"go", &[]).await;
    assert_eq!(run.code, 1);
    assert!(
        run.stderr.starts_with("fleet: first: unreachable: "),
        "{}",
        run.stderr
    );
    assert!(
        !mocks[1]
            .requests()
            .iter()
            .any(|r| matches!(r, RequestBody::Dispatch(_)))
    );
}
