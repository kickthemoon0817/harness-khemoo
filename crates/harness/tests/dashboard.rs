//! `harness dashboard` run as the built binary: `--once`, `--print-data-dir`, the server's
//! request log, bin/dashboard.sh, and the queue count's parity with `harness claimable`.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const FAKE_TOKEN: &str = "ghp_FAKEtokenFAKEtokenFAKEtoken0123456789";
const HARNESS: &str = env!("CARGO_BIN_EXE_harness");

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_default()
}

fn fixtures() -> PathBuf {
    repo_root().join("tests/fixtures/dashboard")
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "harness-dashboard-cli-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).expect("temporary directory");
        Self(path.canonicalize().unwrap_or(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

fn write_script(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, text).expect("script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).ok();
}

/// `harness dashboard` with none of the harness's variables from this test's environment.
fn dashboard() -> Command {
    let mut command = Command::new(HARNESS);
    command.arg("dashboard");
    for name in [
        "HARNESS_ENV",
        "HARNESS_INSTANCE_DIR",
        "HARNESS_STATE",
        "HARNESS_HOME",
        "HARNESS_BIN",
        "USAGE_CACHE",
        "GH_REPO",
        "GH_TOKEN",
        "DASHBOARD_DATA_DIR",
        "DASHBOARD_PORT",
    ] {
        command.env_remove(name);
    }
    command.stdin(Stdio::null());
    command
}

/// A harness home: the state fixtures, an instance harness.env with a fake token and a queue
/// count, a fake gh that wants that token and answers from the fixtures.
struct Home {
    _directory: TempDir,
    home: PathBuf,
}

impl Home {
    fn new() -> Self {
        let directory = TempDir::new();
        let home = directory.0.join("harness");
        for sub in ["bin", "instance", "state/logs", "state/locks"] {
            std::fs::create_dir_all(home.join(sub)).ok();
        }
        let fixtures = fixtures();
        for (from, to) in [
            ("host_health.tsv", "state/host_health.tsv"),
            ("stalls.log", "state/stalls.log"),
            ("allocator.log", "state/logs/allocator.log"),
            ("usage-cache.json", "usage-cache.json"),
        ] {
            std::fs::copy(fixtures.join(from), home.join(to)).ok();
        }
        std::fs::write(
            home.join("instance/harness.env"),
            format!(
                "export GH_TOKEN=\"{FAKE_TOKEN}\"\nGH_REPO=example/repo\nISSUE_LABEL=ai:plan\nLANE_LABEL=ai:lane\n\
                 QUEUE_DEPTH_CMD=$HARNESS_HOME/bin/claimable\nOTHER_SECRET=do-not-read\n"
            ),
        )
        .ok();
        write_script(
            &home.join("bin/claimable"),
            "#!/usr/bin/env bash\n[ -n \"${GH_TOKEN:-}\" ] || exit 5\necho 2\n",
        );
        let fixtures = fixtures.display();
        write_script(
            &home.join("bin/gh"),
            &format!(
                r#"#!/usr/bin/env bash
[ "${{GH_TOKEN:-}}" = "{FAKE_TOKEN}" ] || {{ echo "no token" >&2; exit 4; }}
query=""; closed=""; first=""
for argument in "$@"; do
    case "$argument" in
        query=*) query=${{argument#query=}} ;;
        *"is:issue is:closed"*) closed=1 ;;
        label=ai:first) first=1 ;;
    esac
done
if [[ "$query" == *"issues("* ]]; then
    name=issues.json; [ -n "$first" ] && name=issues_first.json
    jq -c '{{data: {{repository: {{issues: {{pageInfo: {{hasNextPage: false, endCursor: null}}, nodes: .}}}}}}}}' "{fixtures}/$name"
elif [[ "$query" == *"search("* && -n "$closed" ]]; then
    jq -c '{{data: {{search: {{nodes: .}}}}}}' "{fixtures}/closed.json"
elif [[ "$query" == *"search("* ]]; then
    jq -c '{{data: {{search: {{nodes: .}}}}}}' "{fixtures}/merged.json"
else
    echo '{{"data": {{"repository": {{}}}}}}'
fi
"#
            ),
        );
        Self {
            _directory: directory,
            home,
        }
    }

    fn path(&self, relative: &str) -> String {
        self.home.join(relative).display().to_string()
    }

    fn once(&self, extra: &[&str]) -> Output {
        let mut command = dashboard();
        command.args([
            "--harness-home",
            &self.path(""),
            "--data-dir",
            &self.path("data"),
            "--gh",
            &self.path("bin/gh"),
        ]);
        command.args([
            "--usage-cache",
            &self.path("usage-cache.json"),
            "--remote-interval",
            "0",
            "--once",
        ]);
        command.args(extra);
        command.output().expect("harness runs")
    }
}

fn json_of(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON on stdout")
}

#[test]
fn once_prints_a_snapshot_without_the_token() {
    let home = Home::new();
    let output = home.once(&[]);
    let state = json_of(&output);
    assert_eq!(state["queue"]["claimable_sh"]["value"], 2);
    assert_eq!(state["repo"], "example/repo");
    assert!(
        state["queue"]["error"].is_null(),
        "{}",
        state["queue"]["error"]
    );
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!printed.contains(FAKE_TOKEN));
    assert!(!printed.contains("do-not-read"));
}

#[test]
fn once_against_a_bare_instance_prints_a_snapshot_without_those_panels_or_errors() {
    let directory = TempDir::new();
    let root = &directory.0;
    std::fs::create_dir_all(root.join("state")).ok();
    let gh = root.join("gh");
    write_script(
        &gh,
        "#!/usr/bin/env bash\necho '{\"data\": {\"repository\": {\"issues\": {\"pageInfo\": {\"hasNextPage\": false}, \"nodes\": []}}, \"search\": {\"nodes\": []}}}'\n",
    );
    let output = dashboard()
        .args([
            "--harness-home",
            &root.display().to_string(),
            "--gh",
            &gh.display().to_string(),
        ])
        .args([
            "--usage-cache",
            &root.join("usage-cache.json").display().to_string(),
        ])
        .args([
            "--remote-interval",
            "0",
            "--claimable-interval",
            "0",
            "--once",
        ])
        .env("HARNESS_INSTANCE_DIR", fixtures().join("instance"))
        .env("HARNESS_STATE", root.join("state"))
        .output()
        .expect("harness runs");
    let state = json_of(&output);
    assert!(root.join("state/dashboard/local_history.tsv").exists());
    assert_eq!(
        (&state["repo"], &state["title"]),
        (&json!("example/app"), &json!("Harness status"))
    );
    assert!(state["leases"].is_null());
    assert!(state["health"]["stalls"].is_null());
    assert_eq!(state["flow"]["lanes"], json!([]));
    for absent in ["lease", "activity", "host_health_tsv"] {
        assert!(state["sources"].get(absent).is_none(), "{absent}");
    }
    let failing: Vec<(&String, &Value)> = state["sources"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, source)| {
            !source["error"].is_null() && (cfg!(target_os = "linux") || name.as_str() != "procs")
        })
        .collect();
    assert!(failing.is_empty(), "{failing:?}");
}

#[test]
fn print_data_dir_names_the_state_directory_the_config_names() {
    let home = Home::new();
    let env_file = home.home.join("instance/harness.env");
    let text = std::fs::read_to_string(&env_file).unwrap_or_default();
    std::fs::write(
        &env_file,
        format!("{text}HARNESS_STATE=$HARNESS_HOME/other-state\n"),
    )
    .ok();
    let output = dashboard()
        .args(["--harness-home", &home.path(""), "--print-data-dir"])
        .output()
        .expect("harness runs");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        home.path("other-state/dashboard")
    );
    let output = dashboard()
        .args(["--harness-home", &home.path(""), "--print-data-dir"])
        .env("HARNESS_STATE", home.path("state"))
        .output()
        .expect("harness runs");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        home.path("state/dashboard")
    );
}

#[test]
fn fleet_runs_without_the_dashboards_tokens() {
    let home = Home::new();
    let seen = home.home.join("environment");
    let fleet = home.home.join("bin/fleet");
    write_script(
        &fleet,
        &format!(
            "#!/usr/bin/env bash\ncompgen -e | sort > \"{}\"\ncat \"{}\"\n",
            seen.display(),
            repo_root()
                .join("tests/fixtures/fleet/status.json")
                .display()
        ),
    );
    let env_file = home.home.join("instance/harness.env");
    let text = std::fs::read_to_string(&env_file).unwrap_or_default();
    std::fs::write(
        &env_file,
        format!("{text}FLEET_STATUS_CMD=$HARNESS_HOME/bin/fleet\n"),
    )
    .ok();
    let secrets = [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "my_api_token",
        "AWS_SECRET_ACCESS_KEY",
        "DB_PASSWORD",
        "MYSQL_PASSWD",
        "OPENAI_API_KEY",
        "Deploy_Private_Key",
        "GOOGLE_APPLICATION_CREDENTIALS",
    ];
    let mut command = dashboard();
    command.args([
        "--harness-home",
        &home.path(""),
        "--data-dir",
        &home.path("data"),
        "--gh",
        &home.path("bin/gh"),
    ]);
    command.args([
        "--usage-cache",
        &home.path("usage-cache.json"),
        "--remote-interval",
        "0",
        "--claimable-interval",
        "0",
        "--once",
    ]);
    for name in secrets {
        command.env(name, "x");
    }
    command
        .env("SSH_AUTH_SOCK", "/tmp/agent.sock")
        .env("HOME", "/home/x")
        .env("KEYBOARD", "kept");
    let state = json_of(&command.output().expect("harness runs"));
    assert!(
        state["fleet"]["error"].is_null(),
        "{}",
        state["fleet"]["error"]
    );
    let names: Vec<String> = std::fs::read_to_string(&seen)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    for secret in secrets {
        assert!(
            !names.iter().any(|name| name == secret),
            "{secret} reached fleet"
        );
    }
    for kept in [
        "SSH_AUTH_SOCK",
        "HOME",
        "PATH",
        "KEYBOARD",
        "HARNESS_HOME",
        "HARNESS_INSTANCE_DIR",
    ] {
        assert!(
            names.iter().any(|name| name == kept),
            "{kept} did not reach fleet"
        );
    }
}

fn wait_for(path: &Path, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.ends_with('\n')
        {
            return text.trim().to_owned();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{} did not appear", path.display());
}

fn get(url: &str, path_and_query: &str) -> (u16, String) {
    let address = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    let mut stream = std::net::TcpStream::connect(&address).expect("connects");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    write!(
        stream,
        "GET {path_and_query} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .ok();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).ok();
    let status = raw
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let body = raw
        .split_once("\r\n\r\n")
        .map_or("", |(_, body)| body)
        .to_owned();
    (status, body)
}

#[test]
fn the_request_log_never_holds_the_access_token() {
    let home = Home::new();
    let log = home.home.join("server.log");
    let mut server = dashboard()
        .args([
            "--harness-home",
            &home.path(""),
            "--data-dir",
            &home.path("data"),
            "--gh",
            &home.path("bin/gh"),
            "--port",
            "0",
        ])
        .args([
            "--usage-cache",
            &home.path("usage-cache.json"),
            "--remote-interval",
            "0",
            "--claimable-interval",
            "0",
        ])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).expect("log"))
        .spawn()
        .expect("harness runs");
    let url = wait_for(
        &home.home.join("data/dashboard.url"),
        Duration::from_secs(30),
    );
    let token = url
        .split_once("?t=")
        .map_or("", |(_, token)| token)
        .to_owned();
    assert!(token.len() >= 20);
    let (status, body) = get(&url, &format!("/api/state?t={token}"));
    assert_eq!(status, 200);
    assert!(!body.contains(&token));
    assert!(!body.contains(FAKE_TOKEN));
    assert_eq!(get(&url, "/api/state").0, 403);
    rustix::process::kill_process(
        rustix::process::Pid::from_child(&server),
        rustix::process::Signal::TERM,
    )
    .ok();
    let deadline = Instant::now() + Duration::from_secs(10);
    while server.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    server.kill().ok();
    server.wait().ok();
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(logged.contains("/api/state?t=[token]"), "{logged}");
    assert_eq!(
        logged.matches(&token).count(),
        1,
        "only the start line names the URL: {logged}"
    );
    assert!(!logged.contains(&format!("?t={token} HTTP")));
    assert!(!logged.contains(FAKE_TOKEN));
    assert!(!home.home.join("data/dashboard.url").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn the_start_script_runs_it_detached_and_stops_it() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = Home::new();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("a free port");
    let data = home.home.join("data");
    let script = repo_root().join("bin/dashboard.sh");
    let run = |arguments: &[&str]| {
        Command::new(&script)
            .args(arguments)
            .env("HARNESS_HOME", &home.home)
            .env("DASHBOARD_DATA_DIR", &data)
            .env("HARNESS_BIN", HARNESS)
            .env_remove("HARNESS_ENV")
            .env_remove("HARNESS_INSTANCE_DIR")
            .env_remove("HARNESS_STATE")
            .output()
            .expect("dashboard.sh runs")
    };
    let port_text = port.to_string();
    let (gh, usage) = (home.path("bin/gh"), home.path("usage-cache.json"));
    let options = [
        "--port",
        port_text.as_str(),
        "--gh",
        gh.as_str(),
        "--usage-cache",
        usage.as_str(),
        "--remote-interval",
        "0",
        "--claimable-interval",
        "0",
    ];
    let started = run(&options);
    let stop = || run(&["stop"]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let url = String::from_utf8_lossy(&started.stdout).trim().to_owned();
    let prefix = format!("http://127.0.0.1:{port}/?t=");
    let token = url.strip_prefix(&prefix).unwrap_or_default().to_owned();
    assert!(
        token.len() >= 20
            && token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'),
        "{url}"
    );
    let mode = |path: &Path| {
        std::fs::metadata(path)
            .map(|metadata| metadata.permissions().mode())
            .unwrap_or(0)
    };
    assert_eq!(mode(&data) & 0o777, 0o700);
    for name in ["dashboard.url", "dashboard.log", "dashboard.pid"] {
        assert_eq!(mode(&data.join(name)) & 0o077, 0, "{name}");
    }
    assert_eq!(
        std::fs::read_to_string(data.join("dashboard.url"))
            .unwrap_or_default()
            .trim(),
        url
    );
    let again = run(&options);
    let again = String::from_utf8_lossy(&again.stdout);
    assert!(again.contains("already running"), "{again}");
    assert!(again.contains(&url));
    let (status, body) = get(&url, &format!("/api/state?t={token}"));
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&body)
            .map(|state| state["repo"].clone())
            .ok(),
        Some(json!("example/repo"))
    );
    assert_eq!(get(&url, "/api/state").0, 403);
    let status = run(&["status"]);
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains(&url));
    let stopped = stop();
    assert!(String::from_utf8_lossy(&stopped.stdout).contains("stopped pid"));
    assert!(!data.join("dashboard.url").exists());
    assert_eq!(run(&["status"]).status.code(), Some(1));
    let log = std::fs::read_to_string(data.join("dashboard.log")).unwrap_or_default();
    assert!(!log.contains(FAKE_TOKEN));
    assert!(log.contains(&url));
    assert!(!log.contains(&format!("?t={token} HTTP")));
}

/// A fake gh for both readers: `gh issue list --label L` and `gh issue view N --json state`, as
/// `harness claimable` asks, and the GraphQL issue and state queries, as the dashboard asks,
/// all from `$FX/issues.json` and `$FX/closed.<N>`.
const PARITY_GH: &str = r#"#!/usr/bin/env bash
fx="$FX"
if [ "$1 $2" = "issue list" ]; then
    label=""; prev=""
    for a in "$@"; do [ "$prev" = "--label" ] && label=$a; prev=$a; done
    jq -c --arg label "$label" 'map(select(.labels | index($label))) | map({number, body, labels: [.labels[] | {name: .}]})' "$fx/issues.json"
    exit 0
fi
if [ "$1 $2" = "issue view" ]; then
    case "$*" in
        *state*) if [ -e "$fx/closed.$3" ]; then echo CLOSED; else echo OPEN; fi ;;
        *) echo "" ;;
    esac
    exit 0
fi
query=""; label=""
for a in "$@"; do
    case "$a" in
        query=*) query=${a#query=} ;;
        label=*) label=${a#label=} ;;
    esac
done
if [[ "$query" == *"issues("* ]]; then
    jq -c --arg label "$label" '{data: {repository: {issues: {pageInfo: {hasNextPage: false}, nodes: (map(select(.labels | index($label))) | map({number, title: ("t" + (.number | tostring)), body, labels: {nodes: [.labels[] | {name: .}]}}))}}}}' "$fx/issues.json"
elif [[ "$query" == *"issueOrPullRequest"* ]]; then
    numbers=$(printf '%s' "$query" | grep -o 'n[0-9]*: issueOrPullRequest' | tr -dc '0-9 \n')
    repository="{}"
    for n in $numbers; do
        state=OPEN; [ -e "$fx/closed.$n" ] && state=CLOSED
        repository=$(jq -c --arg key "n$n" --arg state "$state" '. + {($key): {state: $state}}' <<< "$repository")
    done
    echo "{\"data\": {\"repository\": $repository}}"
else
    echo '{"data": {"search": {"nodes": []}}}'
fi
"#;

#[test]
fn the_dashboards_queue_count_matches_harness_claimable() {
    type Scenario = (
        &'static str,
        &'static [(u64, &'static [&'static str], &'static str)],
        &'static [u64],
    );
    const SCENARIOS: [Scenario; 11] = [
        (
            "plain then low",
            &[
                (1, &["ai", "ai:plan"], ""),
                (2, &["ai", "ai:plan", "ai:low"], ""),
            ],
            &[],
        ),
        (
            "only low",
            &[
                (2, &["ai", "ai:plan", "ai:low"], ""),
                (3, &["ai", "ai:plan", "ai:low"], ""),
            ],
            &[],
        ),
        (
            "new round and low",
            &[
                (4, &["ai", "ai:plan", "ai:lane"], ""),
                (2, &["ai", "ai:plan", "ai:low"], ""),
            ],
            &[],
        ),
        (
            "new round behind first",
            &[
                (4, &["ai", "ai:plan", "ai:lane"], ""),
                (5, &["ai", "ai:plan", "ai:lane", "ai:first"], ""),
            ],
            &[],
        ),
        (
            "first outside every ai issue",
            &[
                (4, &["ai", "ai:plan", "ai:lane"], ""),
                (2, &["ai", "ai:plan", "ai:low"], ""),
                (9, &["ai:first"], ""),
            ],
            &[],
        ),
        (
            "parked first outside",
            &[
                (4, &["ai", "ai:plan", "ai:lane"], ""),
                (9, &["ai:first", "ai:operator"], ""),
            ],
            &[],
        ),
        (
            "listed first outside the queue",
            &[
                (4, &["ai", "ai:plan", "ai:lane"], ""),
                (8, &["ai", "ai:first"], ""),
            ],
            &[],
        ),
        (
            "blocked plain leaves low",
            &[
                (2, &["ai", "ai:plan", "ai:low"], ""),
                (1, &["ai", "ai:plan"], "Depends on: #77"),
            ],
            &[],
        ),
        (
            "met dependency holds low",
            &[
                (2, &["ai", "ai:plan", "ai:low"], ""),
                (1, &["ai", "ai:plan"], "Depends on: #78"),
            ],
            &[78],
        ),
        (
            "first round and low",
            &[
                (5, &["ai", "ai:plan", "ai:lane", "ai:first"], ""),
                (2, &["ai", "ai:plan", "ai:low"], ""),
            ],
            &[],
        ),
        (
            "blocked first outside",
            &[
                (4, &["ai", "ai:plan", "ai:lane"], ""),
                (9, &["ai:first"], "Depends on: #77"),
            ],
            &[],
        ),
    ];
    for (name, issues, closed) in SCENARIOS {
        let directory = TempDir::new();
        let root = &directory.0;
        for sub in ["fake", "fx", "state/locks", "instance"] {
            std::fs::create_dir_all(root.join(sub)).ok();
        }
        write_script(&root.join("fake/gh"), PARITY_GH);
        let listed: Vec<Value> = issues
            .iter()
            .map(|(number, labels, body)| json!({"number": number, "labels": labels, "body": body}))
            .collect();
        std::fs::write(
            root.join("fx/issues.json"),
            Value::Array(listed).to_string(),
        )
        .ok();
        for number in closed {
            std::fs::write(root.join(format!("fx/closed.{number}")), "").ok();
        }
        std::fs::write(
            root.join("instance/harness.env"),
            "GH_REPO=o/r\nISSUE_LABEL=ai:plan\nLANE_LABEL=ai:lane\n",
        )
        .ok();
        let path = format!(
            "{}:{}",
            root.join("fake").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let counted = Command::new(HARNESS)
            .arg("claimable")
            .env("PATH", &path)
            .env("HARNESS_HOME", root)
            .env("FX", root.join("fx"))
            .env("GH_REPO", "o/r")
            .env("ISSUE_LABEL", "ai:plan")
            .env("LANE_LABEL", "ai:lane")
            .output()
            .expect("harness claimable runs");
        assert!(
            counted.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&counted.stderr)
        );
        let count: usize = String::from_utf8_lossy(&counted.stdout)
            .trim()
            .parse()
            .unwrap_or(usize::MAX);
        let state = json_of(
            &dashboard()
                .args([
                    "--harness-home",
                    &root.display().to_string(),
                    "--gh",
                    &root.join("fake/gh").display().to_string(),
                ])
                .args([
                    "--usage-cache",
                    &root.join("usage.json").display().to_string(),
                    "--remote-interval",
                    "0",
                    "--claimable-interval",
                    "0",
                    "--once",
                ])
                .env("FX", root.join("fx"))
                .output()
                .expect("harness dashboard runs"),
        );
        assert!(
            state["queue"]["error"].is_null(),
            "{name}: {}",
            state["queue"]["error"]
        );
        assert_eq!(
            state["queue"]["claimable_count"].as_u64(),
            u64::try_from(count).ok(),
            "{name}"
        );
    }
}
