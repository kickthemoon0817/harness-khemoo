//! The `fleet` binary against real fleetd daemons, over the local, tls and ssh transports.

mod common;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::{FakeSsh, Pki, Run, TempDir, fake_ssh, make_pki, run_fleet, write_config};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};

/// The agent every daemon runs. Its prompt's first line says what to do: `exit N` prints
/// its prompt to stdout and a line to stderr and exits N; `sleep` records its pid and
/// sleeps until it is killed.
const AGENT: &str = r#"cat > prompt.txt
read -r cmd arg < prompt.txt
case "$cmd" in
  sleep) echo $$ > agent.pid; exec sleep 60 ;;
  exit) printf 'agent read: %s\n' "$(cat prompt.txt)"; echo 'agent stderr' >&2; exit "$arg" ;;
esac
exit 99
"#;

/// fleetd's binary. Cargo hands a test only its own package's binaries, so this builds
/// fleetd with the profile under test; cargo makes that a no-op when the build is fresh.
fn fleetd_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
        let mut cmd = std::process::Command::new(env!("CARGO"));
        cmd.args([
            "build", "--quiet", "--locked", "-p", "fleetd", "--bin", "fleetd",
        ])
        .args(["--message-format", "json-render-diagnostics"])
        .arg("--manifest-path")
        .arg(manifest)
        .stderr(Stdio::inherit());
        if !cfg!(debug_assertions) {
            cmd.arg("--release");
        }
        let out = cmd.output().expect("running cargo");
        assert!(out.status.success(), "building fleetd failed");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["reason"] == "compiler-artifact" && v["target"]["name"] == "fleetd")
            // The library shares the name; only the binary has an executable.
            .find_map(|v| v["executable"].as_str().map(PathBuf::from))
            .expect("cargo reported no fleetd executable")
    })
}

/// A running fleetd, killed on drop.
struct Daemon {
    config: PathBuf,
    socket: PathBuf,
    tls_port: Option<u16>,
    work: PathBuf,
    _child: Child,
}

impl Daemon {
    /// Starts fleetd as `device` in `dir` with one adapter, `agent`, running [`AGENT`] at
    /// most `max_concurrent` at a time. `tls` adds a `[tls]` table on a loopback port,
    /// refusing `revoked` serials.
    async fn start(
        dir: &Path,
        device: &str,
        max_concurrent: u32,
        tls: Option<(&Pki, &[&str])>,
    ) -> Self {
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let agent = dir.join("agent.sh");
        std::fs::write(&agent, AGENT).unwrap();
        let state = dir.join("state");
        let mut toml = format!(
            "device = \"{device}\"\nstate_dir = \"{}\"\nkill_grace_s = 1\n",
            state.display()
        );
        if let Some((pki, revoked)) = tls {
            let _ = write!(
                toml,
                "\n[tls]\nlisten = \"127.0.0.1:0\"\nca = \"{}\"\ncert = \"{}\"\nkey = \"{}\"\n\
                 revoked_serials = {}\n",
                pki.ca.display(),
                pki.server_cert.display(),
                pki.server_key.display(),
                serde_json::to_string(revoked).unwrap(),
            );
        }
        let _ = write!(
            toml,
            "\n[adapters.agent]\nexe = \"/bin/sh\"\nargs = [\"{}\"]\nworkdir_roots = [\"{}\"]\n\
             match_exe = [\"fleet-e2e-none\"]\nmax_concurrent = {max_concurrent}\n",
            agent.display(),
            work.display()
        );
        let config = dir.join("fleetd.toml");
        std::fs::write(&config, toml).unwrap();

        let mut child = Command::new(fleetd_bin())
            .arg("--config")
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let tls_port = ready(child.stderr.take().unwrap()).await;
        Self {
            config,
            socket: state.join("fleetd.sock"),
            tls_port,
            work,
            _child: child,
        }
    }

    /// A fresh working directory under the adapter's root.
    fn workdir(&self, name: &str) -> PathBuf {
        let p = self.work.join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

/// Waits for fleetd's startup line and returns its tls port, if it listens on one. The rest
/// of fleetd's log is drained so it never blocks on a full pipe.
async fn ready(stderr: ChildStderr) -> Option<u16> {
    let mut lines = BufReader::new(stderr).lines();
    let started = async {
        loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("fleetd exited before it started");
            if line.contains("fleetd started") {
                return line;
            }
        }
    };
    let line = tokio::time::timeout(Duration::from_secs(60), started)
        .await
        .expect("fleetd did not start");
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    line.split_once("tls=Some(")
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(addr, _)| addr.rsplit_once(':').unwrap().1.parse().unwrap())
}

fn local(name: &str, d: &Daemon) -> String {
    format!(
        "[devices.{name}]\ntransport = \"local\"\nsocket = \"{}\"\n",
        d.socket.display()
    )
}

fn tls(name: &str, d: &Daemon, pki: &Pki, client: &str) -> String {
    format!(
        "[devices.{name}]\ntransport = \"tls\"\naddress = \"127.0.0.1:{}\"\nserver_name = \"dev-a\"\n\
         ca = \"{}\"\ncert = \"{}\"\nkey = \"{}\"\n",
        d.tls_port.unwrap(),
        pki.ca.display(),
        pki.dir.join(format!("{client}.pem")).display(),
        pki.dir.join(format!("{client}.key")).display(),
    )
}

fn ssh(name: &str, d: &Daemon) -> String {
    format!(
        "[devices.{name}]\ntransport = \"ssh\"\ntarget = \"dev@{name}\"\nremote_command = {}\n",
        serde_json::to_string(&[
            fleetd_bin().to_str().unwrap(),
            "--stdio",
            "--config",
            d.config.to_str().unwrap(),
        ])
        .unwrap()
    )
}

fn json(run: &Run) -> Value {
    serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}\nstderr: {}", run.stdout, run.stderr))
}

fn dispatch<'a>(device: &'a str, workdir: &'a Path, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "dispatch",
        "--device",
        device,
        "--adapter",
        "agent",
        "--workdir",
        workdir.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    args
}

/// Issues a client certificate with `fleet pki` and returns the serial it printed.
async fn issue_client(config: &Path, pki: &Pki, name: &str, role: &str) -> String {
    let dir = pki.dir.to_str().unwrap();
    let args = ["pki", "issue-client", name, "--role", role, "--dir", dir];
    let run = run_fleet(config, &args, b"", &[]).await;
    assert_eq!(run.code, 0, "{}", run.stderr);
    run.stdout
        .lines()
        .find_map(|l| l.strip_prefix("serial "))
        .unwrap()
        .to_owned()
}

/// Polls for a file the agent writes.
async fn wait_for_file(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(s) = std::fs::read_to_string(path)
            && s.ends_with('\n')
        {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The id of the one running session. The agent can start writing before fleetd lists its session,
/// so the list is polled rather than read once.
async fn running_session_id(config: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let running =
            json(&run_fleet(config, &["sessions", "--json", "--running"], b"", &[]).await);
        if let Some(id) = running["sessions"][0]["session"]["id"].as_str() {
            return id.to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "no running session was listed: {running}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// The registry for one daemon reached three ways: `dev-a` over its socket, `tls-a` with
/// the `hub` dispatch certificate, and `ssh-a` through a fake ssh that runs the relay.
async fn three_ways(dir: &TempDir) -> (Daemon, Pki, PathBuf) {
    let pki = make_pki(&dir.join("pki"));
    let d = Daemon::start(&dir.join("a"), "dev-a", 4, Some((&pki, &[]))).await;
    let ssh_path = fake_ssh(dir.path(), FakeSsh::Exec);
    let text = format!(
        "ssh = \"{}\"\n{}{}{}",
        ssh_path.display(),
        local("dev-a", &d),
        tls("tls-a", &d, &pki, "hub"),
        ssh("ssh-a", &d)
    );
    (d, pki, write_config(dir.path(), &text))
}

#[tokio::test(flavor = "multi_thread")]
async fn status_and_sessions_over_every_transport() {
    let dir = TempDir::new();
    let (d, _pki, config) = three_ways(&dir).await;
    let work = d.workdir("s");
    let ran = run_fleet(
        &config,
        &dispatch("dev-a", &work, &["--wait"]),
        b"exit 0\n",
        &[],
    )
    .await;
    assert_eq!(ran.code, 0, "{}", ran.stderr);

    let status = run_fleet(&config, &["status", "--json"], b"", &[]).await;
    assert_eq!(status.code, 0, "{}", status.stderr);
    let doc = json(&status);
    assert_eq!(doc["version"], 1);
    let devices = doc["devices"].as_array().unwrap();
    let seen: Vec<(&str, &str)> = devices
        .iter()
        .map(|d| {
            (
                d["name"].as_str().unwrap(),
                d["transport"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [("dev-a", "local"), ("tls-a", "tls"), ("ssh-a", "ssh")]
    );
    for dev in devices {
        assert_eq!(dev["ok"], true, "{dev}");
        assert_eq!(dev["status"]["device"]["name"], "dev-a");
        let adapter = &dev["status"]["adapters"][0];
        assert_eq!(adapter["name"], "agent");
        assert_eq!(adapter["max_concurrent"], 4);
        assert_eq!(dev["sessions"][0]["state"]["kind"], "exited");
    }

    let sessions = run_fleet(&config, &["sessions", "--json"], b"", &[]).await;
    assert_eq!(sessions.code, 0, "{}", sessions.stderr);
    let doc = json(&sessions);
    let listed: Vec<&str> = doc["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["device"].as_str().unwrap())
        .collect();
    assert_eq!(listed, ["dev-a", "tls-a", "ssh-a"]);
    let ids: Vec<&Value> = doc["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| &s["session"]["id"])
        .collect();
    assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
    assert_eq!(doc["errors"], serde_json::json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_wait_streams_the_output_and_exits_with_the_agent_status() {
    let dir = TempDir::new();
    let (d, _pki, config) = three_ways(&dir).await;
    for (i, device) in ["dev-a", "tls-a", "ssh-a"].into_iter().enumerate() {
        let work = d.workdir(&format!("w{i}"));
        let prompt = format!("exit 7\nfix the {device} test\n");
        let run = run_fleet(
            &config,
            &dispatch(device, &work, &["--wait", "--label", "issue=42"]),
            prompt.as_bytes(),
            &[],
        )
        .await;
        assert_eq!(run.code, 7, "{device}: {}", run.stderr);
        assert_eq!(
            std::fs::read_to_string(work.join("prompt.txt")).unwrap(),
            prompt
        );
        assert!(
            run.stdout
                .contains(&format!("agent read: exit 7\nfix the {device} test\n")),
            "{device}: {}",
            run.stdout
        );
        assert!(run.stdout.contains("agent stderr\n"), "{}", run.stdout);
        assert!(
            run.stderr
                .starts_with(&format!("fleet: {device}: session d-")),
            "{}",
            run.stderr
        );
    }
    let sessions = json(
        &run_fleet(
            &config,
            &["sessions", "--json", "--device", "dev-a"],
            b"",
            &[],
        )
        .await,
    );
    let all = sessions["sessions"].as_array().unwrap();
    assert_eq!(all.len(), 3);
    for s in all {
        assert_eq!(s["session"]["labels"]["issue"], "42");
        assert_eq!(
            s["session"]["state"],
            serde_json::json!({"kind": "exited", "code": 7})
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn tls_certificates_from_fleet_pki_are_checked_for_role_and_revocation() {
    let dir = TempDir::new();
    let pki = make_pki(&dir.join("pki"));
    let config = dir.join("fleet.toml");
    issue_client(&config, &pki, "mon", "monitor").await;
    let serial = issue_client(&config, &pki, "gone", "dispatch").await;
    // Written the way an operator might paste it: upper case, colons, a leading zero byte.
    let pasted = format!(
        "00:{}",
        serial
            .to_uppercase()
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join(":")
    );
    let d = Daemon::start(&dir.join("a"), "dev-a", 1, Some((&pki, &[pasted.as_str()]))).await;
    let text =
        tls("hub", &d, &pki, "hub") + &tls("mon", &d, &pki, "mon") + &tls("gone", &d, &pki, "gone");
    let config = write_config(dir.path(), &text);
    let work = d.workdir("t");

    let status = run_fleet(&config, &["status", "--json", "--device", "mon"], b"", &[]).await;
    assert_eq!(status.code, 0, "{}", status.stderr);
    let refused = run_fleet(&config, &dispatch("mon", &work, &[]), b"exit 0\n", &[]).await;
    assert_eq!(refused.code, 1);
    assert!(
        refused
            .stderr
            .starts_with("fleet: mon: unauthorized: the monitor role does not permit"),
        "{}",
        refused.stderr
    );
    let waited = run_fleet(
        &config,
        &dispatch("mon", &work, &["--wait"]),
        b"exit 0\n",
        &[],
    )
    .await;
    assert_eq!(waited.code, 125, "{}", waited.stderr);

    let revoked = run_fleet(&config, &["status", "--json", "--device", "gone"], b"", &[]).await;
    assert_eq!(revoked.code, 1);
    let dev = &json(&revoked)["devices"][0];
    assert_eq!(dev["ok"], false);
    assert_eq!(dev["error"]["code"], "unauthorized");
    assert_eq!(dev["error"]["message"], "the certificate is revoked");

    let ok = run_fleet(
        &config,
        &dispatch("hub", &work, &["--wait"]),
        b"exit 3\n",
        &[],
    )
    .await;
    assert_eq!(ok.code, 3, "{}", ok.stderr);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_kills_the_session_and_ends_the_wait() {
    let dir = TempDir::new();
    let d = Daemon::start(&dir.join("a"), "dev-a", 1, None).await;
    let config = write_config(dir.path(), &local("dev-a", &d));
    let work = d.workdir("c");

    let mut waiting = Command::new(env!("CARGO_BIN_EXE_fleet"))
        .arg("--config")
        .arg(&config)
        .args(dispatch("dev-a", &work, &["--wait"]))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    {
        use tokio::io::AsyncWriteExt as _;
        let mut stdin = waiting.stdin.take().unwrap();
        stdin.write_all(b"sleep\n").await.unwrap();
    }
    let pid = wait_for_file(&work.join("agent.pid")).await;
    assert!(alive(&pid));

    let id = running_session_id(&config).await;
    let cancel = run_fleet(&config, &["cancel", "dev-a", &id, "--json"], b"", &[]).await;
    assert_eq!(cancel.code, 0, "{}", cancel.stderr);
    assert_eq!(
        json(&cancel),
        serde_json::json!({"device": "dev-a", "session": id})
    );

    let status = tokio::time::timeout(Duration::from_secs(20), waiting.wait())
        .await
        .expect("the wait did not end")
        .unwrap();
    assert_eq!(status.code(), Some(130));
    let mut stderr = String::new();
    waiting
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .await
        .unwrap();
    assert_eq!(stderr, format!("fleet: dev-a: session {id}\n"));
    assert!(!alive(&pid), "the agent outlived its cancel");

    let ended = json(&run_fleet(&config, &["sessions", "--json"], b"", &[]).await);
    assert_eq!(
        ended["sessions"][0]["session"]["state"],
        serde_json::json!({"kind": "cancelled"})
    );
    let again = run_fleet(&config, &["cancel", "dev-a", &id], b"", &[]).await;
    assert_eq!(
        again.code, 0,
        "cancelling an ended session succeeds: {}",
        again.stderr
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_places_on_the_device_with_free_capacity() {
    let dir = TempDir::new();
    let a = Daemon::start(&dir.join("a"), "dev-a", 1, None).await;
    let b = Daemon::start(&dir.join("b"), "dev-b", 1, None).await;
    let config = write_config(dir.path(), &(local("dev-a", &a) + &local("dev-b", &b)));
    let first = run_fleet(
        &config,
        &dispatch("dev-a", &a.workdir("one"), &["--json"]),
        b"sleep\n",
        &[],
    )
    .await;
    assert_eq!(first.code, 0, "{}", first.stderr);
    let first = json(&first);

    let placed = run_fleet(
        &config,
        &dispatch("auto", &b.workdir("two"), &["--json"]),
        b"sleep\n",
        &[],
    )
    .await;
    assert_eq!(placed.code, 0, "{}", placed.stderr);
    let placed = json(&placed);
    assert_eq!(placed["device"], "dev-b");

    let full = run_fleet(
        &config,
        &dispatch("auto", &b.workdir("three"), &[]),
        b"sleep\n",
        &[],
    )
    .await;
    assert_eq!(full.code, 1);
    assert!(
        full.stderr
            .starts_with("fleet: busy: every auto device with adapter agent is at capacity"),
        "{}",
        full.stderr
    );

    for done in [&first, &placed] {
        let (device, id) = (
            done["device"].as_str().unwrap(),
            done["session"].as_str().unwrap(),
        );
        let run = run_fleet(&config, &["cancel", device, id], b"", &[]).await;
        assert_eq!(run.code, 0, "{}", run.stderr);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_device_leaves_status_partial() {
    let dir = TempDir::new();
    let a = Daemon::start(&dir.join("a"), "dev-a", 1, None).await;
    let dead = format!(
        "[devices.dev-gone]\ntransport = \"local\"\nsocket = \"{}\"\n",
        dir.join("gone.sock").display()
    );
    let config = write_config(dir.path(), &(local("dev-a", &a) + &dead));

    let status = run_fleet(&config, &["status", "--json"], b"", &[]).await;
    assert_eq!(status.code, 1);
    let doc = json(&status);
    assert_eq!(doc["devices"][0]["ok"], true);
    assert_eq!(doc["devices"][0]["status"]["device"]["name"], "dev-a");
    assert_eq!(doc["devices"][1]["ok"], false);
    assert_eq!(doc["devices"][1]["error"]["code"], "unreachable");
    assert!(
        status.stderr.starts_with("fleet: dev-gone: unreachable: "),
        "{}",
        status.stderr
    );

    let sessions = run_fleet(&config, &["sessions", "--json"], b"", &[]).await;
    assert_eq!(sessions.code, 1);
    assert_eq!(json(&sessions)["errors"][0]["device"], "dev-gone");
}
