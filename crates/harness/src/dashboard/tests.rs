//! The collector, the page and the server against a scratch harness home with a fake resource
//! status command, queue count and gh, and the dashboard with none of the instance's optional
//! sources configured.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser as _;
use serde_json::{Value, json};

use super::collector::{Collector, Config, FLEET_OUTPUT_MAX_BYTES, Source, lock};
use super::github::GitHubClient;
use super::queue::QueueLabels;
use super::render;
use super::server::Server;
#[cfg(target_os = "linux")]
use super::testkit::fixture_text;
use super::testkit::{FIXTURES, FLEET_FIXTURES, TempDir, fleet_fixture_text, write_script};
use super::{App, Args, ENV_KEYS, env_path};

pub const FAKE_TOKEN: &str = "ghp_FAKEtokenFAKEtokenFAKEtoken0123456789";
pub const PINNED_EPOCH: f64 = 1_795_657_200.0;
const ACCESS_TOKEN: &str = "test-access-token-0123456789";

fn pinned() -> Box<dyn Fn() -> f64 + Send + Sync> {
    Box::new(|| PINNED_EPOCH)
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

fn kill_group(child: &mut Child) {
    if let Some(pid) = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        rustix::process::kill_process_group(pid, rustix::process::Signal::KILL).ok();
    }
    child.kill().ok();
    child.wait().ok();
}

/// A scratch harness home: state files from the fixtures, a fake resource status command, queue
/// count and gh, an instance harness.env holding a fake token and every optional source, and
/// fake ticks, run queues and build locks on demand.
pub struct FixtureHarness {
    _directory: TempDir,
    pub home: PathBuf,
    pub state: PathBuf,
    pub calls_log: PathBuf,
    pub ticks: Vec<Child>,
    helpers: Vec<Child>,
}

impl Drop for FixtureHarness {
    fn drop(&mut self) {
        for child in self.ticks.iter_mut().chain(self.helpers.iter_mut()) {
            kill_group(child);
        }
    }
}

impl FixtureHarness {
    pub fn new() -> Self {
        let directory = TempDir::new();
        let home = directory.path().join("harness");
        let state = home.join("state");
        for sub in ["bin", "libexec", "instance", "evidence", "buildlocks"] {
            std::fs::create_dir_all(home.join(sub)).ok();
        }
        for sub in ["locks", "logs", "model-cooldown"] {
            std::fs::create_dir_all(state.join(sub)).ok();
        }
        for (from, to) in [
            ("host_health.tsv", state.join("host_health.tsv")),
            ("host_health.streaks", state.join("host_health.streaks")),
            ("stalls.log", state.join("stalls.log")),
            ("allocator.log", state.join("logs/allocator.log")),
        ] {
            std::fs::copy(FIXTURES.join(from), to).ok();
        }
        write_usage_cache(&home.join("usage-cache.json"), 64, 52);
        std::fs::write(
            state.join("tick-model"),
            "claude-opus-5-5 claude-sonnet-5-2\n",
        )
        .ok();
        std::fs::write(state.join("model-cooldown/claude:claude-sonnet-5-2"), "").ok();
        std::fs::write(
            state.join("locks/claim.2501"),
            format!("2501 {} 2026-11-26T01:10:00Z\n", std::process::id()),
        )
        .ok();
        let calls_log = home.join("calls.log");
        let fixtures = FIXTURES.display();
        let (home_text, calls) = (home.display().to_string(), calls_log.display().to_string());
        std::fs::write(
            home.join("instance/harness.env"),
            format!(
                "# fixture\nTARGET_REPO=/nowhere\nWORKTREE_ROOT={home_text}/trees\nexport GH_TOKEN=\"{FAKE_TOKEN}\"\n\
                 GH_REPO=example/repo   # the target\nISSUE_LABEL=ai:plan\nFIRST_LABEL=ai:first\nLOW_LABEL=\"ai:low\"\n\
                 LANE_LABEL=ai:lane\nLANE_MODEL=claude-sonnet-5-2\nDASHBOARD_TITLE=\"Fixture <b>harness</b>\"\n\
                 RESOURCE_STATUS_CMD=\"'{home_text}/bin/resource-status' status\"\nDEVICE_PROBE_CMD=\"cat '{fixtures}/device_probe.txt'\"\n\
                 QUEUE_DEPTH_CMD=$HARNESS_HOME/bin/claimable\n\
                 RUN_QUEUE_GLOB={home_text}/evidence/*/runs*\nRUN_QUEUE_HOLDER=run_queue.sh\nRUN_QUEUE_STOP_WORDS=\"STOP YIELD NO-LEASE\"\n\
                 BUILD_LOCK_DIR={home_text}/buildlocks\nBUILD_LOCK_LOCAL=dev-build.lock\nBUILD_LOCK_REMOTE=dev-build-remote-{{name}}.lock\n\
                 BUILD_LOCK_NAMES=\"ci@worker-a.local=wa ci@192.0.2.6=laptop\"\nOTHER_SECRET=do-not-read\n"
            ),
        )
        .ok();
        write_script(
            &home.join("bin/resource-status"),
            &format!(
                "#!/usr/bin/env bash\necho \"resource-status $*\" >> \"{calls}\"\n\
                 if [ -s \"{home_text}/lease_override.txt\" ]; then cat \"{home_text}/lease_override.txt\"\n\
                 else cat \"{fixtures}/lease_status.txt\"; fi\n"
            ),
        );
        write_script(
            &home.join("bin/claimable"),
            &format!(
                "#!/usr/bin/env bash\necho \"claimable $* label=$ISSUE_LABEL token=${{GH_TOKEN:+set}} repo=$GH_REPO\" >> \"{calls}\"\n\
                 [ -e \"{home_text}/claimable-fail\" ] && {{ echo \"boom $GH_TOKEN\" >&2; exit 3; }}\necho 2\n"
            ),
        );
        write_script(
            &home.join("bin/crontab"),
            &format!(
                "#!/usr/bin/env bash\n[ -e \"{home_text}/cron-off\" ] && {{ echo \"# */2 * * * * {home_text}/bin/tick.sh\"; exit 0; }}\n\
                 echo \"*/2 * * * * {home_text}/bin/tick.sh\"\n"
            ),
        );
        write_script(&home.join("bin/gh"), &fake_gh(&calls, &home_text));
        Self {
            home,
            state,
            calls_log,
            ticks: Vec::new(),
            helpers: Vec::new(),
            _directory: directory,
        }
    }

    pub fn gh(&self) -> String {
        self.home.join("bin/gh").display().to_string()
    }

    /// A config reading this home, with its data dir inside it.
    pub fn config(&self) -> Config {
        let home = &self.home;
        let path = |relative: &str| home.join(relative).display().to_string();
        let mut config = Config::new(home.clone(), home.join("dashboard-data"));
        config.repo = Some("example/repo".to_owned());
        config.labels = QueueLabels {
            queue: "ai:plan".to_owned(),
            lane: "ai:lane".to_owned(),
            ..QueueLabels::default()
        };
        config.usage_cache = home.join("usage-cache.json");
        config.remote_interval_s = 0.0;
        config.crontab_command = vec![path("bin/crontab")];
        config.resource_status_command = vec![path("bin/resource-status"), "status".to_owned()];
        config.device_probe_command = vec![
            "cat".to_owned(),
            FIXTURES.join("device_probe.txt").display().to_string(),
        ];
        config.queue_depth_command = vec![path("bin/claimable")];
        config.worktree_root = path("trees");
        config.run_queue_glob = format!("{}/evidence/*/runs*", home.display());
        config.run_queue_holder = "run_queue.sh".to_owned();
        config.run_queue_stop_words = strings(&["STOP", "YIELD", "NO-LEASE"]);
        config.build_lock_dir = home.join("buildlocks");
        config.build_lock_local = "dev-build.lock".to_owned();
        config.build_lock_remote = "dev-build-remote-{name}.lock".to_owned();
        config.build_lock_names = vec![
            ("ci@worker-a.local".to_owned(), "wa".to_owned()),
            ("ci@192.0.2.6".to_owned(), "laptop".to_owned()),
        ];
        config
    }

    pub fn collector_with(
        &self,
        config: Config,
        clock: Box<dyn Fn() -> f64 + Send + Sync>,
    ) -> Collector {
        let github = GitHubClient::new("example/repo", Some(FAKE_TOKEN), &self.gh()).ok();
        Collector::new(config, github, clock)
    }

    pub fn collector(&self) -> Collector {
        self.collector_with(self.config(), pinned())
    }

    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.calls_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Starts a process that looks like a harness tick: argv[0] `claude`, argv[1] `-p`, stdout to
    /// a tick log, a slot lock open, and one tool shell child wrapping its command as Claude Code
    /// does. The tool shell sleeps before its `eval`, so the command is only carried in its argv,
    /// never run: it may be any text, quoting that does not parse included.
    #[cfg(target_os = "linux")]
    pub fn start_fake_tick(&mut self, issue: i64, slot: i64, tool_command: &str) -> i64 {
        self.start_marked_tick(
            issue,
            slot,
            tool_command,
            "claude",
            "claude-opus-5-5",
            "work",
        )
    }

    /// A fake tick as the runner starts one: recorded in its slot's `locks/slot<n>.tick`, with the
    /// slot lock open and `HARNESS_TICK` describing it to its own tool shells.
    #[cfg(target_os = "linux")]
    pub fn start_marked_tick(
        &mut self,
        issue: i64,
        slot: i64,
        tool_command: &str,
        agent: &str,
        model: &str,
        kind: &str,
    ) -> i64 {
        use std::os::unix::process::CommandExt as _;
        let escaped = tool_command.replace('\'', "'\"'\"'");
        let tool_argument = self.home.join(format!("tool_argument_{slot}.txt"));
        std::fs::write(
            &tool_argument,
            format!("true && sleep 30 && eval '{escaped}' < /dev/null && pwd -P >| /dev/null"),
        )
        .ok();
        let script = self.home.join(format!("fake_tick_{slot}.sh"));
        std::fs::write(
            &script,
            format!("bash -c \"$(cat '{}')\" &\nwait\n", tool_argument.display()),
        )
        .ok();
        let lock = self.state.join(format!("locks/slot{slot}.lock"));
        let log = self
            .state
            .join(format!("logs/tick-20261126T013000Z-slot{slot}.log"));
        let child = Command::new("bash")
            .args([
                "-c",
                "exec 9>\"$1\" >\"$2\"; exec -a \"$4\" bash \"$3\"",
                "_",
            ])
            .arg(&lock)
            .arg(&log)
            .arg(&script)
            .arg(agent)
            .env(
                "HARNESS_TICK",
                format!(
                    "state={} slot={slot} agent={agent} model={model} kind={kind}",
                    self.state.display()
                ),
            )
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("bash starts");
        let pid = i64::from(child.id());
        self.ticks.push(child);
        std::fs::write(
            self.state.join(format!("locks/slot{slot}.tick")),
            format!("{pid} {} {agent} {model} {kind}\n", start_ticks(pid)),
        )
        .ok();
        std::fs::write(
            self.state.join(format!("locks/claim.{issue}")),
            format!("{issue} {pid} 2026-11-26T01:30:00Z\n"),
        )
        .ok();
        std::fs::write(
            self.state.join("locks/claim.2001"),
            format!("2001 {pid} 2026-11-25T01:00:00Z\n"),
        )
        .ok();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let children = Command::new("ps")
                .args(["-o", "args=", "--ppid", &pid.to_string()])
                .output()
                .map(|output| output.stdout)
                .unwrap_or_default();
            if String::from_utf8_lossy(&children).contains("eval '") {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        pid
    }

    /// Writes a run folder and starts a process that looks like its `run_queue.sh` holder;
    /// `{pid}` in the queue log stands for the holder's pid.
    #[cfg(target_os = "linux")]
    pub fn start_fake_queue(
        &mut self,
        issue: i64,
        runs: &str,
        status: &str,
        queue_log: &str,
    ) -> i64 {
        use std::os::unix::process::CommandExt as _;
        let script = self.home.join("bin/run_queue.sh");
        std::fs::write(&script, "sleep 30\n").ok();
        let holder = Command::new("bash")
            .arg(&script)
            .process_group(0)
            .spawn()
            .expect("bash starts");
        let pid = i64::from(holder.id());
        self.helpers.push(holder);
        let folder = self
            .home
            .join(format!("evidence/2026-11-26-{issue}-fixture/{runs}"));
        std::fs::create_dir_all(&folder).ok();
        std::fs::write(folder.join("status"), status).ok();
        std::fs::write(
            folder.join("queue.log"),
            queue_log.replace("{pid}", &pid.to_string()),
        )
        .ok();
        pid
    }

    /// Takes a build lock as a build does (pid and tree written into it) and starts a waiter in
    /// another tree.
    #[cfg(target_os = "linux")]
    pub fn hold_build_lock(
        &mut self,
        name: &str,
        holder_issue: i64,
        waiter_issue: i64,
    ) -> (i64, i64) {
        use std::os::unix::process::CommandExt as _;
        let lock = self
            .home
            .join(format!("buildlocks/dev-build-remote-{name}.lock"));
        let holder = Command::new("bash")
            .args(["-c", "exec 8>\"$1\"; flock 8; sleep 30", "_"])
            .arg(&lock)
            .process_group(0)
            .spawn()
            .expect("bash starts");
        let holder_pid = i64::from(holder.id());
        self.helpers.push(holder);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !lock.exists() {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(
            &lock,
            format!(
                "pid {holder_pid}, {}/trees/app-{holder_issue}, app\n",
                self.home.display()
            ),
        )
        .ok();
        let tree = self.home.join(format!("trees/app-{waiter_issue}"));
        std::fs::create_dir_all(&tree).ok();
        let waiter = Command::new("flock")
            .arg(&lock)
            .args(["sleep", "30"])
            .current_dir(&tree)
            .process_group(0)
            .spawn()
            .expect("flock starts");
        let waiter_pid = i64::from(waiter.id());
        self.helpers.push(waiter);
        while Instant::now() < deadline {
            let listing = Command::new("lslocks")
                .args(["-n", "-o", "PID,MODE,PATH"])
                .output()
                .map(|output| output.stdout)
                .unwrap_or_default();
            let listing = String::from_utf8_lossy(&listing);
            if listing.lines().any(|line| {
                line.split_whitespace().next() == Some(&waiter_pid.to_string())
                    && line.contains('*')
            }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        (holder_pid, waiter_pid)
    }
}

/// A claude usage cache read now, with these window percentages.
fn write_usage_cache(path: &Path, five_hour: u32, weekly: u32) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    std::fs::write(
        path,
        format!(r#"{{"timestamp":{now_ms},"data":{{"fiveHourPercent":{five_hour},"fiveHourResetsAt":"2026-11-26T04:00:00Z","weeklyPercent":{weekly}}}}}"#),
    )
    .ok();
}

/// A fake gh answering the dashboard's read-only queries from the fixtures, logging each call.
fn fake_gh(calls: &str, home: &str) -> String {
    let fixtures = FIXTURES.display();
    format!(
        r#"#!/usr/bin/env bash
fixtures="{fixtures}"
printf 'gh %s\n' "$(printf '%s\0' "$@" | jq -Rsc 'split("\u0000")[:-1]')" >> "{calls}"
[ "${{GH_TOKEN:-}}" = "{FAKE_TOKEN}" ] || {{ echo "no token" >&2; exit 4; }}
if [ -e "{home}/gh-fail" ]; then echo "HTTP 401: Bad credentials for token $GH_TOKEN" >&2; exit 1; fi
query=""; closed=""; first=""; search_issues=""
for argument in "$@"; do
    case "$argument" in
        query=*) query=${{argument#query=}} ;;
        *"is:issue is:closed"*) closed=1 ;;
        label=ai:first) first=1 ;;
        search/issues) search_issues=1 ;;
    esac
done
if [ -n "$search_issues" ]; then
    echo '{{"items": [{{"number": 2490}}]}}'
elif [[ "$query" == *"issues("* ]]; then
    name=issues.json; [ -n "$first" ] && name=issues_first.json
    jq -c '{{data: {{repository: {{issues: {{totalCount: length, pageInfo: {{hasNextPage: false, endCursor: null}}, nodes: .}}}}}}}}' "$fixtures/$name"
elif [[ "$query" == *"search("* && -n "$closed" ]]; then
    jq -c '{{data: {{search: {{issueCount: length, nodes: .}}}}}}' "$fixtures/closed.json"
elif [[ "$query" == *"search("* ]]; then
    jq -c '{{data: {{search: {{issueCount: length, nodes: .}}}}}}' "$fixtures/merged.json"
elif [[ "$query" == *"issueOrPullRequest"* ]]; then
    numbers=$(printf '%s' "$query" | grep -o 'n[0-9]*: issueOrPullRequest' | tr -dc '0-9 \n')
    jq -nc --argjson states "$(cat "$fixtures/states.json")" '{{data: {{repository: ($ARGS.positional | map({{key: ("n" + .), value: (if $states[.] then {{state: $states[.]}} else null end)}}) | from_entries)}}}}' --args $numbers
else
    echo "unexpected call" >&2; exit 2
fi
"#
    )
}

/// A collector on a fixture home with a tick (on Linux), its first local sample, GitHub read and
/// queue count, and the snapshot after them.
struct Loaded {
    harness: FixtureHarness,
    collector: Collector,
    snapshot: Value,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    tick_pid: Option<i64>,
}

fn loaded() -> Loaded {
    #[allow(unused_mut)]
    let mut harness = FixtureHarness::new();
    #[cfg(target_os = "linux")]
    let tick_pid = Some(harness.start_fake_tick(2464, 3, "sleep 30"));
    #[cfg(not(target_os = "linux"))]
    let tick_pid = None;
    let collector = harness.collector();
    collector.refresh_local_sample_in_place();
    collector.refresh_github_in_place();
    collector.refresh_claimable_in_place();
    let snapshot = collector.snapshot();
    Loaded {
        harness,
        collector,
        snapshot,
        tick_pid,
    }
}

fn page(snapshot: &Value) -> String {
    render::render(snapshot, 30, "")
}

fn source_error(snapshot: &Value, name: &str) -> Value {
    snapshot["sources"][name]["error"].clone()
}

#[test]
fn publisher_keeps_its_own_cadence_whatever_the_source_intervals() {
    let harness = FixtureHarness::new();
    let mut config = harness.config();
    config.local_interval_s = 0.0;
    config.github_interval_s = 0.0;
    config.claimable_interval_s = 3600.0;
    let collector = Arc::new(harness.collector_with(config, pinned()));
    let published = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&published);
    collector
        .start(Some(Box::new(move || *lock(&counter) += 1)), 1.0)
        .expect("starts");
    let deadline = Instant::now() + Duration::from_secs(5);
    while *lock(&published) < 3 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    collector.stop();
    assert!(*lock(&published) >= 3);
}

#[test]
fn a_refresh_that_panics_is_stored_as_the_failure_and_the_loop_goes_on() {
    let loaded = loaded();
    let calls = Mutex::new(0);
    let collector = &loaded.collector;
    collector.run_periodically(Source::Github, 0.01, &|| {
        let mut count = lock(&calls);
        *count += 1;
        if *count >= 3 {
            collector.ask_stop();
        }
        drop(count);
        panic!("boom");
    });
    assert_eq!(*lock(&calls), 3);
    let github = &collector.snapshot()["sources"]["github"];
    assert_eq!(github["error"], "panic: boom");
    let logged = lock(&collector.logged).clone();
    assert_eq!(logged.len(), 3);
    assert!(
        logged
            .iter()
            .all(|line| line == "Dashboard refresh of github failed: boom"),
        "{logged:?}"
    );
    assert!(!github["fetched_at"].is_null());
}

#[cfg(target_os = "linux")]
#[test]
fn every_panel_has_data() {
    let loaded = loaded();
    let snapshot = &loaded.snapshot;
    let keys: Vec<&str> = snapshot["machines"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|machine| machine["key"].as_str())
        .collect();
    assert_eq!(keys, ["local", "ci@worker-a.local", "ci@192.0.2.6"]);
    let worker = &snapshot["machines"][1];
    assert_eq!(
        (&worker["status"], &worker["upload_mb_s"], &worker["load1"]),
        (&json!("ok"), &json!(64.1), &json!(5.5))
    );
    assert_eq!(worker["gpu"]["util_pct"], 63.0);
    assert_eq!(
        worker["history"]["gpu_util"].as_array().map(Vec::len),
        Some(3)
    );
    assert_eq!(
        worker["slots"].as_array().map(|slots| slots
            .iter()
            .map(|slot| slot["slot"].clone())
            .collect::<Vec<_>>()),
        Some(vec![json!(4), json!(5)])
    );
    let demoted = &snapshot["machines"][2];
    assert_eq!(demoted["status"], "down");
    assert!(
        demoted["signals"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|signal| signal["text"] == "moved last (3 bad probes)")
    );
    assert_eq!(demoted["history"]["down"], json!([1_795_656_961.0]));
    assert_eq!(
        snapshot["machines"][0]["card_note"],
        "harness off (left to its owner)"
    );
    assert_eq!(snapshot["machines"][0]["gpu"]["name"], "Example Device");
    assert_eq!(
        snapshot["machines"][0]["gpu_processes"],
        json!([{"pid": 4242, "used_mb": 512.0, "name": "python3", "container": null}])
    );
    let mut local_keys: Vec<&String> = snapshot["machines"][0]
        .as_object()
        .map(|map| map.keys().collect())
        .unwrap_or_default();
    local_keys.sort();
    assert_eq!(
        local_keys,
        [
            "age_s",
            "card_note",
            "connect_s",
            "containers",
            "gpu",
            "gpu_processes",
            "history",
            "key",
            "kind",
            "load1",
            "mem_total_mb",
            "mem_used_mb",
            "name",
            "nproc",
            "probe_error",
            "sampled_at",
            "signals",
            "slot_count",
            "slots",
            "status",
            "streak",
            "upload_mb_s",
        ]
    );
    let mut remote_keys: Vec<&String> = snapshot["machines"][1]
        .as_object()
        .map(|map| map.keys().collect())
        .unwrap_or_default();
    remote_keys.sort();
    assert_eq!(
        remote_keys,
        [
            "age_s",
            "card_note",
            "connect_s",
            "containers",
            "gpu",
            "gpu_processes",
            "history",
            "key",
            "kind",
            "load1",
            "mem_total_mb",
            "mem_used_mb",
            "name",
            "nproc",
            "preferred_rank",
            "probe_age_s",
            "probe_error",
            "probe_stale",
            "probed_at",
            "sampled_at",
            "signals",
            "slot_count",
            "slots",
            "status",
            "streak",
            "upload_mb_s",
            "upload_timed_out",
        ]
    );

    let leases = &snapshot["leases"];
    assert_eq!(leases["available"], true);
    assert_eq!(leases["slots"].as_array().map(Vec::len), Some(7));
    assert_eq!(leases["whole_card_holds"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        (
            &leases["whole_card_holds"][0]["issue"],
            &leases["whole_card_holds"][0]["slots"]
        ),
        (&json!(2490), &json!([4, 5]))
    );
    assert_eq!(leases["operator_priority"], json!(["2490", "2488"]));
    assert_eq!(leases["device"]["issue"], 2466);
    assert_eq!(leases["waits"].as_array().map(Vec::len), Some(2));

    assert_eq!(snapshot["ticks"].as_array().map(Vec::len), Some(1));
    let tick = &snapshot["ticks"][0];
    assert_eq!(tick["pid"].as_i64(), loaded.tick_pid);
    assert_eq!(
        (&tick["issue"], &tick["issue_source"], &tick["slot"]),
        (&json!(2464), &json!("claim"), &json!(3))
    );
    assert_eq!(
        tick["title"],
        "feat: give each region of material a persistent rest state"
    );

    let queue = &snapshot["queue"];
    assert_eq!(queue["counts"]["running"], 1);
    assert_eq!(queue["claimable_count"], 3);
    let mut held: Vec<i64> = queue["groups"]["held"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|issue| issue["number"].as_i64())
        .collect();
    held.sort_unstable();
    assert_eq!(held, [2503, 2504]);
    let listed: Vec<i64> = queue["groups"]
        .as_object()
        .into_iter()
        .flat_map(|groups| groups.values())
        .flat_map(|issues| issues.as_array().into_iter().flatten())
        .filter_map(|issue| issue["number"].as_i64())
        .collect();
    assert!(!listed.contains(&2600));
    assert_eq!(
        (
            &queue["claimable_sh"]["value"],
            &queue["claimable_sh"]["unverified"]
        ),
        (&json!(2), &json!(false))
    );
    assert_eq!(
        queue["allocator_claimable"],
        json!({"value": "2", "at": "2026-11-26T01:32:06Z", "unverified": false})
    );
    assert_eq!(snapshot["harness"]["waiting_count"], 3);
    assert_eq!(
        snapshot["harness"]["waiting_count_from_failing_read"],
        false
    );
    assert!(queue["error"].is_null());
    let landings: Vec<i64> = snapshot["landings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pull| pull["number"].as_i64())
        .collect();
    assert_eq!(landings, [2496, 2499]);

    assert_eq!(
        snapshot["health"]["stalls"].as_array().map(Vec::len),
        Some(4)
    );
    assert_eq!(
        snapshot["health"]["allocator"]
            .as_array()
            .and_then(|lines| lines.last())
            .map(|line| line["kind"].clone()),
        Some(json!("chain"))
    );
    let usage = &snapshot["usage"];
    assert_eq!(usage["agents"].as_array().map(Vec::len), Some(1));
    let claude = &usage["agents"][0];
    assert_eq!(
        (&claude["agent"], &claude["reading"]["state"]),
        (&json!("claude"), &json!("fresh"))
    );
    let windows: Vec<(Value, Value, Value)> = claude["windows"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|window| {
            (
                window["name"].clone(),
                window["used_pct"].clone(),
                window["cap"].clone(),
            )
        })
        .collect();
    assert_eq!(
        windows,
        [
            (json!("five_hour"), json!(64.0), json!(4)),
            (json!("weekly"), json!(52.0), json!(8))
        ]
    );
    assert_eq!(
        (&claude["cap"], &usage["pool_cap"], &usage["width"]),
        (&json!(4), &json!(4), &json!(8))
    );
    let harness = &snapshot["harness"];
    assert_eq!(harness["last_allowed"], 10);
    assert_eq!(
        harness["tick_models"],
        json!(["claude:claude-opus-5-5", "claude:claude-sonnet-5-2"])
    );
    let cooling = harness["cooling_models"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(cooling.len(), 1);
    assert_eq!(
        (
            &cooling[0]["entry"],
            &cooling[0]["agent"],
            &cooling[0]["label"]
        ),
        (
            &json!("claude:claude-sonnet-5-2"),
            &json!("claude"),
            &json!("claude Sonnet 5.2")
        )
    );
    assert!(
        cooling[0]["remaining_s"]
            .as_i64()
            .is_some_and(|remaining| remaining > 0)
    );
    assert_eq!(harness["work_waiting"], true);
}

#[test]
fn calls_are_read_only() {
    let loaded = loaded();
    let calls = loaded.harness.calls();
    assert!(!calls.is_empty());
    for call in calls {
        if call.starts_with("resource-status") {
            assert_eq!(call, "resource-status status");
        } else if let Some(arguments) = call.strip_prefix("gh ") {
            let arguments: Vec<String> = serde_json::from_str(arguments).expect("logged argv");
            assert_eq!(arguments[0], "api");
            let query = arguments
                .iter()
                .find(|argument| argument.starts_with("query="))
                .cloned()
                .unwrap_or_default();
            assert!(!query.contains("mutation"));
            if arguments[1] != "graphql" {
                assert_eq!(arguments[1..3], ["-X", "GET"]);
            }
        } else {
            assert_eq!(call, "claimable  label=ai:plan token=set repo=example/repo");
        }
    }
    let locks = std::fs::read_dir(loaded.harness.state.join("locks"))
        .into_iter()
        .flatten()
        .flatten();
    assert!(
        !locks
            .into_iter()
            .any(|entry| entry.file_name().to_string_lossy().starts_with("dashboard"))
    );
    assert!(
        loaded
            .harness
            .home
            .join("dashboard-data/local_history.tsv")
            .exists()
    );
}

#[test]
fn the_default_queue_count_is_the_harness_binarys_claimable() {
    let harness = FixtureHarness::new();
    let bin = harness.home.join("bin/fake-harness");
    write_script(
        &bin,
        &format!(
            "#!/usr/bin/env bash\necho \"harness $* label=$ISSUE_LABEL\" >> \"{}\"\necho 4\n",
            harness.calls_log.display()
        ),
    );
    let mut config = harness.config();
    config.queue_depth_command = Vec::new();
    config.harness_bin = bin.display().to_string();
    let collector = harness.collector_with(config, pinned());
    collector.refresh_claimable_in_place();
    assert_eq!(
        lock(&collector.results)
            .claimable_sh
            .as_ref()
            .and_then(|slot| slot.data),
        Some(4)
    );
    assert!(
        harness
            .calls()
            .contains(&"harness claimable label=ai:plan".to_owned())
    );
}

#[test]
fn page_renders_every_panel_and_escapes() {
    let loaded = loaded();
    let page = page(&loaded.snapshot);
    let mut expected = vec![
        "Harness status",
        "<meta http-equiv=\"refresh\" content=\"30;url=/",
        "ci@worker-a.local",
        "moved last (3 bad probes)",
        "Resource slots and leases",
        "whole device",
        "operator priority",
        "Running ticks",
        "Claimable",
        "from this read (ai:plan)",
        "queue count 2",
        "held back by the first-before-low gate: 2",
        "#2503",
        "Waiting on the operator",
        "Waiting for sign-off",
        "#2200",
        "Landings",
        "#2499",
        "Stalls",
        "lock_wait",
        "Allocator",
        "spawn",
        "usage",
        "<svg",
        "polyline",
        "prefers-color-scheme:dark",
        "work waiting",
    ];
    if cfg!(target_os = "linux") {
        expected.extend(["#2464", "Opus 5.5"]);
    }
    for text in expected {
        assert!(page.contains(text), "{text}");
    }
    assert!(!page.contains("<script>alert(1)</script>"));
    assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert_eq!(page.matches("<script").count(), 1);
    assert!(page.contains("<script defer src=\"/dashboard.js"));
}

#[test]
fn no_token_in_page_or_json() {
    let loaded = loaded();
    assert!(!loaded.snapshot.to_string().contains(FAKE_TOKEN));
    assert!(!page(&loaded.snapshot).contains(FAKE_TOKEN));
    assert!(!format!("{:?}", loaded.collector.github).contains(FAKE_TOKEN));
    assert!(!format!("{:?}", loaded.collector.config).contains(FAKE_TOKEN));
}

#[test]
fn gh_failure_is_redacted_and_keeps_the_last_read() {
    let loaded = loaded();
    std::fs::write(loaded.harness.home.join("gh-fail"), "").ok();
    std::fs::write(loaded.harness.home.join("claimable-fail"), "").ok();
    loaded.collector.refresh_github_in_place();
    loaded.collector.refresh_claimable_in_place();
    let snapshot = loaded.collector.snapshot();
    let error = snapshot["queue"]["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("Bad credentials for token [redacted]"),
        "{error}"
    );
    let claimable_error = snapshot["queue"]["claimable_sh"]["error"]
        .as_str()
        .unwrap_or_default();
    assert!(
        claimable_error.contains("boom [redacted]"),
        "{claimable_error}"
    );
    assert_eq!(
        snapshot["queue"]["counts"],
        loaded.snapshot["queue"]["counts"]
    );
    assert_eq!(snapshot["queue"]["claimable_sh"]["value"], 2);
    assert!(!snapshot.to_string().contains(FAKE_TOKEN));
    assert!(!page(&snapshot).contains(FAKE_TOKEN));
}

#[test]
fn paused_by_file_or_commented_cron_line_stops_the_work_waiting_signal() {
    let loaded = loaded();
    std::fs::write(loaded.harness.home.join("cron-off"), "").ok();
    loaded.collector.refresh_local_sample_in_place();
    let harness = loaded.collector.snapshot()["harness"].clone();
    assert_eq!(
        (&harness["paused"], &harness["paused_reason"]),
        (&json!(true), &json!("no active bin/tick.sh cron line"))
    );
    assert_eq!(harness["work_waiting"], false);
    std::fs::remove_file(loaded.harness.home.join("cron-off")).ok();
    std::fs::write(loaded.harness.state.join("paused"), "").ok();
    loaded.collector.refresh_local_sample_in_place();
    let harness = loaded.collector.snapshot()["harness"].clone();
    assert_eq!(
        (&harness["paused"], &harness["paused_reason"]),
        (&json!(true), &json!("state/paused present"))
    );
}

#[cfg(target_os = "linux")]
#[test]
fn counts_taken_while_the_own_read_fails_are_unverified() {
    let mut harness = FixtureHarness::new();
    harness.start_fake_tick(2464, 3, "sleep 30");
    let clock = Arc::new(Mutex::new(1_795_656_600.0));
    let reading = Arc::clone(&clock);
    let collector = harness.collector_with(harness.config(), Box::new(move || *lock(&reading)));
    collector.refresh_github_in_place();
    *lock(&clock) = 1_795_656_900.0;
    std::fs::write(harness.home.join("gh-fail"), "").ok();
    collector.refresh_github_in_place();
    *lock(&clock) = 1_795_657_000.0;
    collector.refresh_claimable_in_place();
    let snapshot = collector.snapshot();
    let queue = &snapshot["queue"];
    assert_eq!(queue["allocator_claimable"]["unverified"], true);
    assert_eq!(queue["claimable_sh"]["unverified"], true);
    assert_eq!(snapshot["harness"]["waiting_count"], 3);
    assert_eq!(snapshot["harness"]["waiting_count_from_failing_read"], true);
    let unverified: HashMap<String, Value> = snapshot["health"]["allocator"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|line| {
            (
                line["time"].as_str().unwrap_or_default().to_owned(),
                line["claimable_unverified"].clone(),
            )
        })
        .collect();
    assert_eq!(unverified["2026-11-26T01:32:06Z"], true);
    assert_eq!(unverified["2026-11-26T01:00:13Z"], false);
    assert_eq!(
        queue["github_outages"],
        json!([{"from": "2026-11-26T01:30:00Z", "to": null}])
    );
    let rendered = page(&snapshot);
    assert!(rendered.contains("claimable unverified"));
    assert!(rendered.contains("at the last good read"));

    std::fs::remove_file(harness.home.join("gh-fail")).ok();
    *lock(&clock) = 1_795_657_200.0;
    collector.refresh_github_in_place();
    *lock(&clock) = 1_795_657_260.0;
    collector.refresh_claimable_in_place();
    let snapshot = collector.snapshot();
    assert_eq!(snapshot["queue"]["allocator_claimable"]["unverified"], true);
    assert_eq!(snapshot["queue"]["claimable_sh"]["unverified"], false);
    assert_eq!(
        snapshot["harness"]["waiting_count_from_failing_read"],
        false
    );
    assert_eq!(
        snapshot["queue"]["github_outages"],
        json!([{"from": "2026-11-26T01:30:00Z", "to": "2026-11-26T01:40:00Z"}])
    );
}

#[test]
fn restart_serves_the_saved_github_read() {
    let loaded = loaded();
    let restarted = loaded.harness.collector();
    restarted.load_github_cache_in_place();
    let snapshot = restarted.snapshot();
    assert_eq!(
        snapshot["queue"]["counts"]["claimable"],
        loaded.snapshot["queue"]["counts"]["claimable"]
    );
    if cfg!(target_os = "linux") {
        assert_eq!(snapshot["queue"]["counts"]["claimable"], 3);
    }
    assert_eq!(
        snapshot["queue"]["age_s"], 0,
        "{}",
        loaded.snapshot["queue"]["error"]
    );
    assert!(snapshot["queue"]["error"].is_null());
}

#[test]
fn local_history_survives_a_restart() {
    let loaded = loaded();
    let restarted = loaded.harness.collector();
    restarted.load_local_history_in_place();
    let history = lock(&restarted.history);
    assert_eq!(history.len(), 1);
    assert_eq!(history[0][0], Some(PINNED_EPOCH));
}

#[cfg(target_os = "linux")]
#[test]
fn finds_a_live_tick_with_slot_log_issue_and_tool_call() {
    use super::parsers::parse_claim;
    use super::procs::ProcessScanner;
    let mut harness = FixtureHarness::new();
    let pid = harness.start_fake_tick(2464, 3, "sleep 30");
    let scanner = ProcessScanner::new(&harness.state);
    let processes = scanner.scan_processes().expect("/proc reads");
    let candidates = scanner
        .find_ticks(&processes, super::collector::wall_clock())
        .expect("/proc reads");
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.process.pid)
            .collect::<Vec<_>>(),
        [pid]
    );
    assert_eq!(candidates[0].slot, Some(3));
    assert_eq!(
        candidates[0].log.as_deref(),
        Some("tick-20261126T013000Z-slot3.log")
    );
    let claims: Vec<_> = ["claim.2464", "claim.2001"]
        .iter()
        .filter_map(|name| {
            parse_claim(
                name,
                &std::fs::read_to_string(harness.state.join("locks").join(name))
                    .unwrap_or_default(),
            )
        })
        .collect();
    let ticks = ProcessScanner::describe_ticks(
        &candidates,
        &processes,
        &claims,
        &[],
        &HashMap::new(),
        super::collector::wall_clock(),
    );
    assert_eq!(ticks.len(), 1);
    let tick = &ticks[0];
    assert_eq!(
        (tick.issue, tick.issue_source.as_deref()),
        (Some(2464), Some("claim"))
    );
    assert_eq!(
        (
            tick.model.as_str(),
            tick.model_label.as_str(),
            tick.kind.as_str()
        ),
        ("claude-opus-5-5", "Opus 5.5", "work")
    );
    assert_eq!(tick.doing.as_deref(), Some("sleep 30"));
    assert_eq!(tick.tool_shells, 1);
    assert!(tick.age_s >= 0);
}

#[cfg(target_os = "linux")]
#[test]
fn ignores_claude_p_outside_the_harness() {
    use super::procs::ProcessScanner;
    use std::os::unix::process::CommandExt as _;
    let harness = FixtureHarness::new();
    let mut process = Command::new("bash")
        .args([
            "-c",
            "exec -a claude bash -p -c 'sleep 30; :' x other --model m",
        ])
        .stdout(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("bash starts");
    std::thread::sleep(Duration::from_millis(200));
    let scanner = ProcessScanner::new(&harness.state);
    let ticks = scanner
        .find_ticks(
            &scanner.scan_processes().expect("/proc reads"),
            super::collector::wall_clock(),
        )
        .expect("/proc reads");
    kill_group(&mut process);
    assert!(ticks.is_empty());
}

/// Field 22 of `/proc/<pid>/stat`, as the runner records it in a slot file.
#[cfg(target_os = "linux")]
fn start_ticks(pid: i64) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19)?.parse().ok())
        .unwrap_or_default()
}

/// Whether lslocks runs and sees the locks of this namespace: some containers show none.
#[cfg(target_os = "linux")]
fn has_lslocks() -> bool {
    let directory = TempDir::new();
    let lock = directory.path().join("probe.lock");
    // Locked the way a build locks: some overlay filesystems keep such a lock out of /proc/locks.
    let Ok(mut holder) = Command::new("bash")
        .args(["-c", "exec 8>\"$1\"; flock 8; sleep 5", "_"])
        .arg(&lock)
        .spawn()
    else {
        return false;
    };
    let pid = holder.id().to_string();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut seen = false;
    while !seen && Instant::now() < deadline {
        let listing = Command::new("lslocks")
            .args(["-n", "-o", "PID"])
            .output()
            .map(|output| output.stdout)
            .unwrap_or_default();
        seen = String::from_utf8_lossy(&listing)
            .split_whitespace()
            .any(|listed| listed == pid);
        std::thread::sleep(Duration::from_millis(50));
    }
    holder.kill().ok();
    holder.wait().ok();
    seen
}

#[cfg(target_os = "linux")]
#[test]
fn reads_a_live_holder_and_waiter() {
    use super::buildlocks::BuildLockReader;
    use super::procs::ProcessScanner;
    if !has_lslocks() {
        eprintln!("SKIP: lslocks cannot see this namespace's locks");
        return;
    }
    let mut harness = FixtureHarness::new();
    let (holder_pid, waiter_pid) = harness.hold_build_lock("wa", 2495, 2388);
    let scanner = ProcessScanner::new(&harness.state);
    let reader = BuildLockReader {
        lock_names: [("ci@worker-a.local".to_owned(), "wa".to_owned())].into(),
        lock_dir: harness.home.join("buildlocks"),
        local_file: "dev-build.lock".to_owned(),
        remote_file: "dev-build-remote-{name}.lock".to_owned(),
        timeout: Duration::from_secs(5),
        worktree_root: harness.home.join("trees").display().to_string(),
    };
    let lanes = reader
        .read_lanes(
            &strings(&["ci@worker-a.local", "ci@192.0.2.6"]),
            &scanner.scan_processes().expect("/proc reads"),
            &HashMap::new(),
        )
        .expect("lslocks runs");
    let by_host: HashMap<&str, &super::models::BuildLane> = lanes
        .iter()
        .map(|lane| (lane.host.as_str(), lane))
        .collect();
    let mut hosts: Vec<&str> = by_host.keys().copied().collect();
    hosts.sort_unstable();
    assert_eq!(hosts, ["ci@192.0.2.6", "ci@worker-a.local", "local"]);
    let wa = by_host["ci@worker-a.local"];
    assert_eq!(
        wa.holder.as_ref().map(|holder| (holder.pid, holder.issue)),
        Some((holder_pid, Some(2495)))
    );
    assert_eq!(
        wa.waiters
            .iter()
            .map(|waiter| (waiter.pid, waiter.issue))
            .collect::<Vec<_>>(),
        [(waiter_pid, Some(2388))]
    );
    assert!(by_host["local"].holder.is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn the_collector_builds_a_live_flow_and_the_page_draws_it() {
    let mut harness = FixtureHarness::new();
    if !has_lslocks() {
        eprintln!("SKIP: lslocks cannot see this namespace's locks");
        return;
    }
    let tick_pid = harness.start_fake_tick(2464, 3, "sleep 30");
    let running = std::fs::read_to_string(FIXTURES.join("runqueues/running/status"))
        .unwrap_or_default()
        .replace("ci@192.0.2.60", "ci@192.0.2.6");
    harness.start_fake_queue(
        2502,
        "runs-render",
        &running,
        "holder {pid} start 2026-11-26T01:20:00Z queue=/q want=any deadline=1\n",
    );
    harness.start_fake_queue(
        2504,
        "runs",
        "",
        "holder {pid} start 2026-11-26T01:20:00Z queue=/q want=ci@192.0.2.6 deadline=1\n2026-11-26T01:30:00Z BUSY slot limit 2/2\n",
    );
    harness.hold_build_lock("wa", 2495, 2388);
    let clock = Arc::new(Mutex::new(PINNED_EPOCH));
    let reading = Arc::clone(&clock);
    // The run folders were written now; the window is measured from the collector's clock.
    let mut config = harness.config();
    config.run_queue_window_s = f64::MAX;
    let collector = harness.collector_with(config, Box::new(move || *lock(&reading)));
    collector.refresh_github_in_place();
    let first = collector.snapshot();
    let cards: HashMap<i64, Value> = first["flow"]["cards"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|card| Some((card["issue"].as_i64()?, card.clone())))
        .collect();
    assert_eq!(cards[&2464]["column"], "claimed");
    assert_eq!(
        (&cards[&2502]["column"], &cards[&2502]["label"]),
        (
            &json!("running"),
            &json!("running RS6-on-s1323-render on laptop")
        )
    );
    assert_eq!(cards[&2504]["column"], "waiting_device");
    assert_eq!(cards[&2490]["label"], "holding wa");
    assert_eq!(cards[&2495]["column"], "compiling");
    assert_eq!(cards[&2388]["label"], "waiting for the wa compile lane");
    assert_eq!(cards[&2492]["column"], "done");
    assert!(source_error(&first, "activity").is_null());

    std::fs::remove_file(harness.state.join("locks/claim.2464")).ok();
    kill_group(&mut harness.ticks[0]);
    *lock(&clock) += 30.0;
    let second = collector.snapshot();
    assert!(
        !second["ticks"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|tick| tick["pid"] == tick_pid)
    );
    let moved = second["flow"]["cards"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|card| card["issue"] == 2464)
        .cloned()
        .unwrap_or_default();
    assert_eq!(moved["column"], "waiting");
    assert_eq!(moved["moved"], true);
    assert_eq!(second["flow"]["transitions"][0]["issue"], 2464);

    let rendered = render::render(&second, 30, "access");
    for text in [
        "class=\"flow-canvas\"",
        "class=\"fdev",
        "laptop",
        "compile: #2495, 1 waiting",
        "class=\"fcard waiting moved\"",
        "--dx:",
        "<path class=\"fedge device holds",
        "class=\"fedge compile waits",
        "Waiting for a device",
        "#2464</b> claimed →",
        "&lt;script&gt;alert(1)&lt;/script&gt;",
    ] {
        assert!(rendered.contains(text), "{text}");
    }
    assert_eq!(rendered.matches("<script").count(), 1);
    assert!(!format!("{rendered}{second}").contains(FAKE_TOKEN));
}

#[cfg(target_os = "linux")]
#[test]
fn no_secret_reaches_the_json_or_the_page() {
    use std::os::unix::process::CommandExt as _;
    let shapes: Vec<Value> =
        serde_json::from_str(&fixture_text("secret_shapes.json")).expect("fixture");
    let mut harness = FixtureHarness::new();
    let texts: Vec<String> = shapes
        .iter()
        .map(|shape| shape["text"].as_str().unwrap_or_default().to_owned())
        .collect();
    let tool_command = format!(
        "GH_TOKEN=ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789 bash -c 'sleep 30' x {}",
        texts.join(" ")
    );
    let tick_pid = harness.start_fake_tick(2464, 3, &tool_command);
    let holder_script = harness.home.join("holder.sh");
    std::fs::write(&holder_script, "sleep 30\n").ok();
    let words: Vec<String> = texts
        .iter()
        .flat_map(|text| super::shlex::split(text).unwrap_or_default())
        .collect();
    let holder = Command::new("bash")
        .arg(&holder_script)
        .args(&words)
        .process_group(0)
        .spawn()
        .expect("bash starts");
    let flagged = Command::new("bash")
        .arg(&holder_script)
        .args(["--token", "flag-value-2", "--issue", "7"])
        .process_group(0)
        .spawn()
        .expect("bash starts");
    let lease = fixture_text("lease_status.txt")
        .replace(
            "slot 4 HELD 2490 432188",
            &format!("slot 4 HELD 2490 {}", holder.id()),
        )
        .replace(
            "slot 5 HELD 2490 432188",
            &format!("slot 5 HELD 2490 {}", flagged.id()),
        );
    harness.helpers.push(holder);
    harness.helpers.push(flagged);
    std::fs::write(harness.home.join("lease_override.txt"), lease).ok();
    std::thread::sleep(Duration::from_millis(200));

    let collector = harness.collector();
    collector.refresh_github_in_place();
    let snapshot = collector.snapshot();
    let snapshot_json = snapshot.to_string();
    let rendered = render::render(&snapshot, 30, "access");
    let tick = snapshot["ticks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|tick| tick["pid"] == tick_pid)
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        tick["doing"],
        "bash -c 'sleep 30' x GH_TOKEN=[redacted] \u{2026}"
    );
    let slots: HashMap<i64, Value> = snapshot["leases"]["slots"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|slot| Some((slot["slot"].as_i64()?, slot.clone())))
        .collect();
    assert_eq!(
        slots[&5]["holder_kind"],
        "holder.sh --token [redacted] \u{2026}"
    );
    assert!(
        slots[&4]["holder_kind"]
            .as_str()
            .unwrap_or_default()
            .starts_with("holder.sh ")
    );
    let stall = &snapshot["health"]["stalls"][0];
    let detail = stall["detail"].as_str().unwrap_or_default();
    assert!(
        detail.contains("https://[redacted]@example.com/r.git"),
        "{detail}"
    );
    assert!(detail.contains("GH_TOKEN=[redacted]"), "{detail}");
    let mut secrets: Vec<String> = shapes
        .iter()
        .map(|shape| shape["secret"].as_str().unwrap_or_default().to_owned())
        .collect();
    secrets.extend(strings(&[
        "ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
        "stall-pass-1",
        "stall-token-1",
        "flag-value-2",
    ]));
    for secret in secrets {
        assert!(!snapshot_json.contains(&secret), "{secret}");
        assert!(!rendered.contains(&render::escape(&secret)), "{secret}");
        assert!(!rendered.contains(&secret), "{secret}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn no_quoting_bypass_reaches_the_json_or_the_page() {
    let shapes: Vec<Value> =
        serde_json::from_str(&fixture_text("bypass_shapes.json")).expect("fixture");
    let mut harness = FixtureHarness::new();
    let mut pid_by_shape = HashMap::new();
    for (index, shape) in shapes.iter().enumerate() {
        let index = i64::try_from(index).unwrap_or_default();
        let pid = harness.start_fake_tick(
            3000 + index,
            index + 1,
            shape["command"].as_str().unwrap_or_default(),
        );
        pid_by_shape.insert(shape["shape"].as_str().unwrap_or_default().to_owned(), pid);
    }
    let kept: Vec<Value> =
        serde_json::from_str(&fixture_text("kept_shapes.json")).expect("fixture");
    let mut stalls = std::fs::OpenOptions::new()
        .append(true)
        .open(harness.state.join("stalls.log"))
        .expect("stalls.log");
    for item in &kept {
        writeln!(
            stalls,
            "2026-11-26T01:30:00Z STALL note local {}",
            item["text"].as_str().unwrap_or_default()
        )
        .ok();
    }
    drop(stalls);
    let collector = Arc::new(harness.collector());
    let snapshot = collector.snapshot();
    let snapshot_json = snapshot.to_string();
    let rendered = render::render(&snapshot, 30, "access");
    let server = Server::new(
        Arc::clone(&collector),
        30,
        &[FAKE_TOKEN.to_owned()],
        "access".to_owned(),
        1,
    );
    server.publish_snapshot(snapshot.clone());
    let manifest =
        String::from_utf8_lossy(&server.route("/api/sections").expect("index").plain).into_owned();
    let index: serde_json::Map<String, Value> =
        serde_json::from_str(&manifest).expect("index JSON");
    let mut surface = manifest.clone();
    for digest in index.values() {
        let fragment = server
            .route(&format!(
                "/api/sections/{}",
                digest.as_str().unwrap_or_default()
            ))
            .expect("fragment");
        surface.push_str(&String::from_utf8_lossy(&fragment.plain));
    }
    let surface = unescape(&surface);
    assert!(surface.contains(super::redact::MASK));
    for item in &kept {
        assert!(
            surface.contains(item["kept"].as_str().unwrap_or_default()),
            "{}",
            item["kept"]
        );
    }
    let doing_by_pid: HashMap<i64, String> = snapshot["ticks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tick| Some((tick["pid"].as_i64()?, tick["doing"].as_str()?.to_owned())))
        .collect();
    for shape in &shapes {
        let doing = &doing_by_pid[&pid_by_shape[shape["shape"].as_str().unwrap_or_default()]];
        assert!(!doing.is_empty());
        for secret in shape["secrets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            for (surface_name, text) in [
                ("doing", doing.as_str()),
                ("json", &snapshot_json),
                ("page", &rendered),
                ("sections", &surface),
            ] {
                assert!(
                    !text.contains(secret),
                    "{}: {secret} in {surface_name}",
                    shape["shape"]
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&amp;", "&")
}

#[test]
fn the_device_probe_runs_as_an_argv_here_and_quoted_over_ssh() {
    let directory = TempDir::new();
    std::fs::create_dir_all(directory.path().join("state")).ok();
    let mut config = Config::new(
        directory.path().to_path_buf(),
        directory.path().join("data"),
    );
    config.remote_interval_s = 0.0;
    config.crontab_command = strings(&["true"]);
    let argv = strings(&["printf", "%s %s\\n", "GPU X, 5, 10, 20;", "$(id)"]);
    config.device_probe_command.clone_from(&argv);
    let collector = Collector::new(config, None, pinned());
    assert_eq!(collector.run_device_probe(), "GPU X, 5, 10, 20; $(id)\n");
    assert!(
        collector
            .remote_probe_script
            .contains(&super::shlex::join(&argv))
    );
    assert!(!collector.remote_probe_script.contains("sh -c"));
}

#[test]
fn the_queue_count_runs_queue_depth_cmd_when_set() {
    let harness = FixtureHarness::new();
    let counter = harness.home.join("instance/count");
    write_script(
        &counter,
        &format!(
            "#!/usr/bin/env bash\necho \"count label=$ISSUE_LABEL\" >> \"{}\"\necho 5\n",
            harness.calls_log.display()
        ),
    );
    let mut config = harness.config();
    config.queue_depth_command = vec![counter.display().to_string()];
    let collector = harness.collector_with(config, pinned());
    collector.refresh_claimable_in_place();
    assert_eq!(
        lock(&collector.results)
            .claimable_sh
            .as_ref()
            .and_then(|slot| slot.data),
        Some(5)
    );
    assert!(harness.calls().contains(&"count label=ai:plan".to_owned()));
    assert!(
        !harness
            .calls()
            .iter()
            .any(|call| call.starts_with("claimable"))
    );
}

#[test]
fn the_collector_reads_no_optional_source_and_the_page_leaves_their_panels_out() {
    let directory = TempDir::new();
    let home = directory.path();
    std::fs::create_dir_all(home.join("state/logs")).ok();
    let mut config = Config::new(home.to_path_buf(), home.join("data"));
    config.remote_interval_s = 0.0;
    config.usage_cache = home.join("usage-cache.json");
    config.crontab_command = strings(&["true"]);
    let collector = Collector::new(config, None, pinned());
    collector.refresh_local_sample_in_place();
    collector.refresh_remote_probes_in_place();
    let snapshot = collector.snapshot();
    let commands: Vec<String> = lock(&collector.recorded)
        .iter()
        .map(|argv| argv[0].clone())
        .collect();
    assert!(snapshot["leases"].is_null());
    assert!(snapshot["health"]["stalls"].is_null());
    let mut sources: Vec<&String> = snapshot["sources"]
        .as_object()
        .map(|map| map.keys().collect())
        .unwrap_or_default();
    sources.sort();
    assert_eq!(
        sources,
        [
            "claimable_sh",
            "github",
            "local",
            "paused",
            "procs",
            "remote"
        ]
    );
    assert_eq!(snapshot["flow"]["lanes"], json!([]));
    let columns: Vec<&str> = snapshot["flow"]["columns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|column| column["key"].as_str())
        .collect();
    assert_eq!(columns, ["waiting", "claimed", "landing", "held", "done"]);
    assert_eq!(snapshot["machines"].as_array().map(Vec::len), Some(1));
    assert!(snapshot["machines"][0]["gpu"].is_null());
    assert!(snapshot["flow"]["devices"][0]["holders"].is_null());
    assert!(snapshot["flow"]["devices"][0]["compile"].is_null());
    assert_eq!(commands, ["docker", "true"]);
    for name in ["local", "paused", "remote"] {
        assert!(source_error(&snapshot, name).is_null(), "{name}");
    }
    let rendered = page(&snapshot);
    assert!(snapshot["fleet"].is_null());
    for absent in [
        "Resource slots",
        "Stalls",
        "Device",
        "lease:",
        "<th>Leases",
        "compile lane",
        "▾",
        "no slot held",
        "\"grid split\"",
        "Agent sessions",
    ] {
        assert!(!rendered.contains(absent), "{absent}");
    }
    assert!(rendered.contains("<title>Harness status</title>"));
}

/// The arguments of `harness dashboard`, parsed as the command line parses them.
#[derive(clap::Parser)]
struct Cli {
    #[command(flatten)]
    args: Args,
}

fn args(words: &[&str]) -> Args {
    Cli::parse_from(std::iter::once("dashboard").chain(words.iter().copied())).args
}

/// An environment of only these variables.
fn environment(variables: &[(&str, String)]) -> impl Fn(&str) -> Option<String> + use<> {
    let variables: HashMap<String, String> = variables
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect();
    move |name: &str| {
        variables
            .get(name)
            .filter(|value| !value.is_empty())
            .cloned()
    }
}

fn config_of(home: &Path, variables: &[(&str, String)]) -> Config {
    App::new(
        &args(&["--harness-home", &home.display().to_string()]),
        &environment(variables),
    )
    .expect("config reads")
    .config
}

#[test]
fn harness_env_wins_over_the_instance_dir() {
    let directory = TempDir::new();
    let other = directory.path().join("other.env");
    let instance = FIXTURES.join("instance");
    let with_instance = environment(&[("HARNESS_INSTANCE_DIR", instance.display().to_string())]);
    assert_eq!(
        env_path(directory.path(), &with_instance),
        instance.join("harness.env")
    );
    let with_env = environment(&[
        ("HARNESS_INSTANCE_DIR", instance.display().to_string()),
        ("HARNESS_ENV", other.display().to_string()),
    ]);
    assert_eq!(env_path(directory.path(), &with_env), other);
    assert_eq!(
        env_path(directory.path(), &environment(&[])),
        directory.path().join("instance/harness.env")
    );
}

#[test]
fn labels_come_from_the_env_file_and_the_flag_overrides_the_queue_label() {
    let harness = FixtureHarness::new();
    let home = harness.home.display().to_string();
    let data = harness.home.join("data").display().to_string();
    let base = ["--harness-home", home.as_str(), "--data-dir", data.as_str()];
    let config = App::new(&args(&base), &environment(&[]))
        .expect("config reads")
        .config;
    let labels = &config.labels;
    assert_eq!(
        (
            labels.queue.as_str(),
            labels.first.as_str(),
            labels.low.as_str(),
            labels.wip.as_str(),
            labels.lane.as_str()
        ),
        ("ai:plan", "ai:first", "ai:low", "ai:wip", "ai:lane")
    );
    assert_eq!(
        (config.title.as_str(), config.lane_model.as_str()),
        ("Fixture <b>harness</b>", "claude-sonnet-5-2")
    );
    assert_eq!(
        config.resource_status_command,
        [
            harness
                .home
                .join("bin/resource-status")
                .display()
                .to_string(),
            "status".to_owned()
        ]
    );
    assert_eq!(
        config.queue_depth_command,
        [harness.home.join("bin/claimable").display().to_string()]
    );
    assert_eq!(config.run_queue_stop_words, ["STOP", "YIELD", "NO-LEASE"]);
    assert_eq!(
        (
            config.build_lock_dir.clone(),
            config.build_lock_local.as_str(),
            config.build_lock_remote.as_str()
        ),
        (
            harness.home.join("buildlocks"),
            "dev-build.lock",
            "dev-build-remote-{name}.lock"
        )
    );
    assert_eq!(
        config.build_lock_names,
        [
            ("ci@worker-a.local".to_owned(), "wa".to_owned()),
            ("ci@192.0.2.6".to_owned(), "laptop".to_owned())
        ]
    );
    let mut overridden = base.to_vec();
    overridden.extend(["--queue-label", "ai"]);
    assert_eq!(
        App::new(&args(&overridden), &environment(&[]))
            .expect("config reads")
            .config
            .labels
            .queue,
        "ai"
    );
    let mut keys = ENV_KEYS.to_vec();
    keys.sort_unstable();
    let mut expected = vec![
        "GH_TOKEN",
        "GH_REPO",
        "ISSUE_LABEL",
        "WIP_LABEL",
        "SIGNOFF_LABEL",
        "OPERATOR_LABEL",
        "READY_LABEL",
        "LANE_LABEL",
        "FIRST_LABEL",
        "LOW_LABEL",
        "LANE_MODEL",
        "DASHBOARD_TITLE",
        "RESOURCE_STATUS_CMD",
        "DEVICE_PROBE_CMD",
        "QUEUE_DEPTH_CMD",
        "HARNESS_STATE",
        "HARNESS_BIN",
        "USAGE_CACHE",
        "WORKTREE_ROOT",
        "RUN_QUEUE_GLOB",
        "RUN_QUEUE_HOLDER",
        "RUN_QUEUE_STOP_WORDS",
        "RUN_QUEUE_LOG",
        "BUILD_LOCK_DIR",
        "BUILD_LOCK_LOCAL",
        "BUILD_LOCK_REMOTE",
        "BUILD_LOCK_NAMES",
        "FLEET_STATUS_CMD",
        "FLEET_CONFIG",
    ];
    expected.sort_unstable();
    assert_eq!(keys, expected);
}

#[test]
fn an_empty_gh_repo_is_derived_through_instance_lib() {
    let directory = TempDir::new();
    let root = directory.path();
    // The harness home's library serves when the test binary sits outside its checkout.
    std::fs::create_dir_all(root.join("lib")).ok();
    std::fs::copy(
        super::testkit::REPO_ROOT.join("lib/instance.sh"),
        root.join("lib/instance.sh"),
    )
    .ok();
    let target = root.join("target-repo");
    let git = |arguments: &[&str]| {
        Command::new("git")
            .args(arguments)
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    assert!(git(&["init", "-q", &target.display().to_string()]));
    assert!(git(&[
        "-C",
        &target.display().to_string(),
        "remote",
        "add",
        "origin",
        "https://github.com/derived-org/derived-repo.git"
    ]));
    std::fs::create_dir_all(root.join("instance")).ok();
    std::fs::write(
        root.join("instance/harness.env"),
        format!(
            "TARGET_REPO={}\nWORKTREE_ROOT={}/trees\n",
            target.display(),
            root.display()
        ),
    )
    .ok();
    let config = config_of(root, &[("HOME", std::env::var("HOME").unwrap_or_default())]);
    assert_eq!(config.repo.as_deref(), Some("derived-org/derived-repo"));
}

#[test]
fn commands_expand_only_the_harness_placeholders() {
    let harness = FixtureHarness::new();
    let env_file = harness.home.join("instance/harness.env");
    let mut text = std::fs::read_to_string(&env_file).unwrap_or_default();
    text.push_str(concat!(
        "RESOURCE_STATUS_CMD=\"$HARNESS_HOME/bin/resource-status status '$HOME' \\$PATH\"\n",
        "DEVICE_PROBE_CMD=\"${HARNESS_INSTANCE_DIR}/bin/probe --name=$HARNESS_HOMEX\"\n",
        "QUEUE_DEPTH_CMD=$HARNESS_INSTANCE_DIR/bin/count\n",
        "FLEET_STATUS_CMD=\"fleet --config '$HARNESS_INSTANCE_DIR/fleet.toml' status --json\"\n",
    ));
    std::fs::write(&env_file, text).ok();
    let instance = harness.home.join("instance");
    let config = config_of(
        &harness.home,
        &[("HARNESS_INSTANCE_DIR", instance.display().to_string())],
    );
    let path = |relative: &str| harness.home.join(relative).display().to_string();
    assert_eq!(
        config.resource_status_command,
        [
            path("bin/resource-status"),
            "status".to_owned(),
            "$HOME".to_owned(),
            "$PATH".to_owned()
        ]
    );
    assert_eq!(
        config.device_probe_command,
        [
            path("instance/bin/probe"),
            "--name=$HARNESS_HOMEX".to_owned()
        ]
    );
    assert_eq!(config.queue_depth_command, [path("instance/bin/count")]);
    assert_eq!(
        config.fleet_status_command,
        [
            "fleet".to_owned(),
            "--config".to_owned(),
            path("instance/fleet.toml"),
            "status".to_owned(),
            "--json".to_owned()
        ]
    );
    assert_eq!(config.worktree_root, path("trees"));
    assert_eq!(config.run_queue_log, "queue.log");
}

#[test]
fn harness_bin_expands_as_lib_instance_sh_resolves_it() {
    let harness = FixtureHarness::new();
    let env_file = harness.home.join("instance/harness.env");
    let original = std::fs::read_to_string(&env_file).unwrap_or_default();
    std::fs::write(&env_file, format!("{original}QUEUE_DEPTH_CMD=\"$HARNESS_BIN claimable\"\nFLEET_STATUS_CMD=${{HARNESS_BIN}}x\n")).ok();
    let from_environment = config_of(
        &harness.home,
        &[("HARNESS_BIN", "/opt/h/harness".to_owned())],
    );
    assert_eq!(
        from_environment.queue_depth_command,
        ["/opt/h/harness", "claimable"]
    );
    assert_eq!(from_environment.fleet_status_command, ["/opt/h/harnessx"]);
    assert_eq!(from_environment.harness_bin, "/opt/h/harness");
    let from_path = config_of(&harness.home, &[]);
    assert_eq!(from_path.queue_depth_command, ["harness", "claimable"]);
    let release = harness.home.join("target/release/harness");
    std::fs::create_dir_all(harness.home.join("target/release")).ok();
    write_script(&release, "#!/bin/sh\n");
    let from_checkout = config_of(&harness.home, &[]);
    assert_eq!(
        from_checkout.queue_depth_command,
        [release.display().to_string(), "claimable".to_owned()]
    );
    assert_eq!(from_checkout.harness_bin, release.display().to_string());
    std::fs::write(&env_file, format!("{original}HARNESS_BIN=$HARNESS_HOME/bin/own\nQUEUE_DEPTH_CMD=\"$HARNESS_BIN claimable\"\n")).ok();
    let from_file = config_of(
        &harness.home,
        &[("HARNESS_BIN", "/opt/h/harness".to_owned())],
    );
    assert_eq!(
        from_file.queue_depth_command,
        [
            harness.home.join("bin/own").display().to_string(),
            "claimable".to_owned()
        ]
    );
    assert_eq!(
        from_file.harness_bin,
        harness.home.join("bin/own").display().to_string()
    );
}

#[test]
fn fleet_runs_with_the_instance_a_tick_gives_it() {
    let harness = FixtureHarness::new();
    let instance: Vec<(String, String)> = vec![
        (
            "HARNESS_HOME".to_owned(),
            harness.home.display().to_string(),
        ),
        (
            "HARNESS_INSTANCE_DIR".to_owned(),
            harness.home.join("instance").display().to_string(),
        ),
    ];
    assert_eq!(config_of(&harness.home, &[]).fleet_environment, instance);
    let env_file = harness.home.join("instance/harness.env");
    let text = std::fs::read_to_string(&env_file).unwrap_or_default();
    std::fs::write(
        &env_file,
        format!("{text}FLEET_CONFIG=$HARNESS_INSTANCE_DIR/fleets/lab.toml\n"),
    )
    .ok();
    let mut expected = instance;
    expected.push((
        "FLEET_CONFIG".to_owned(),
        harness
            .home
            .join("instance/fleets/lab.toml")
            .display()
            .to_string(),
    ));
    assert_eq!(config_of(&harness.home, &[]).fleet_environment, expected);
}

#[test]
fn state_and_usage_cache_come_from_the_env_file_and_the_environment_wins() {
    let harness = FixtureHarness::new();
    let env_file = harness.home.join("instance/harness.env");
    let text = std::fs::read_to_string(&env_file).unwrap_or_default();
    std::fs::write(&env_file, format!("{text}HARNESS_STATE=$HARNESS_HOME/other-state\nUSAGE_CACHE=$HOME/usage.json\nRUN_QUEUE_LOG=custom.log\n")).ok();
    let home_dir = "/home/someone".to_owned();
    let config = config_of(&harness.home, &[("HOME", home_dir.clone())]);
    assert_eq!(config.state_dir(), harness.home.join("other-state"));
    assert_eq!(config.data_dir, harness.home.join("other-state/dashboard"));
    assert_eq!(
        config.usage_cache,
        PathBuf::from(format!("{home_dir}/usage.json"))
    );
    assert_eq!(config.run_queue_log, "custom.log");
    let config = config_of(
        &harness.home,
        &[
            ("HOME", home_dir),
            ("HARNESS_STATE", harness.state.display().to_string()),
            (
                "USAGE_CACHE",
                harness.home.join("usage-cache.json").display().to_string(),
            ),
        ],
    );
    assert_eq!(
        (config.state_dir(), config.usage_cache),
        (harness.state.clone(), harness.home.join("usage-cache.json"))
    );
    assert_eq!(config.data_dir, harness.state.join("dashboard"));
}

/// A fleet collector whose command prints the fixture document and the exit status named in
/// files the test rewrites.
struct FleetFixture {
    directory: TempDir,
    collector: Collector,
}

const SECRET_TOKEN: &str = "ghp_FLEETsecretFLEETsecretFLEETsecret0123";
const SECRET_KEY: &str = "sk-FLEETsecretFLEETsecretFLEET01";

impl FleetFixture {
    fn new() -> Self {
        let directory = TempDir::new();
        let home = directory.path();
        std::fs::create_dir_all(home.join("state/logs")).ok();
        std::fs::copy(FLEET_FIXTURES.join("status.json"), home.join("fleet.json")).ok();
        std::fs::write(home.join("fleet.rc"), "0").ok();
        let script = "cat \"$1\"; printf 'fleet: dev-b: unreachable: ssh exited 255' >&2; exit \"$(cat \"$2\")\"";
        let command = vec![
            "bash".to_owned(),
            "-c".to_owned(),
            script.to_owned(),
            "_".to_owned(),
            home.join("fleet.json").display().to_string(),
            home.join("fleet.rc").display().to_string(),
        ];
        let collector = Self::collector(home, command, 45.0, "/srv/work");
        Self {
            directory,
            collector,
        }
    }

    fn collector(
        home: &Path,
        command: Vec<String>,
        timeout_s: f64,
        worktree_root: &str,
    ) -> Collector {
        let mut config = Config::new(home.to_path_buf(), home.join("data"));
        config.remote_interval_s = 0.0;
        config.usage_cache = home.join("usage-cache.json");
        config.crontab_command = strings(&["true"]);
        config.worktree_root = worktree_root.to_owned();
        config.fleet_status_command = command;
        config.fleet_timeout_s = timeout_s;
        Collector::new(config, None, pinned())
    }

    fn running(&self, script: &str, timeout_s: f64) -> Collector {
        let command = vec![
            "bash".to_owned(),
            "-c".to_owned(),
            script.to_owned(),
            "_".to_owned(),
            self.document().display().to_string(),
        ];
        Self::collector(self.directory.path(), command, timeout_s, "")
    }

    fn document(&self) -> PathBuf {
        self.directory.path().join("fleet.json")
    }

    fn read(&self, document: Option<&str>, exit_code: i32) -> Value {
        if let Some(document) = document {
            std::fs::write(self.document(), document).ok();
        }
        std::fs::write(
            self.directory.path().join("fleet.rc"),
            exit_code.to_string(),
        )
        .ok();
        self.collector.refresh_fleet_in_place();
        self.collector.snapshot()
    }
}

fn status_document() -> Value {
    serde_json::from_str(&fleet_fixture_text("status.json")).expect("fixture")
}

#[test]
fn both_fleet_panels_render_from_the_command() {
    let fleet = FleetFixture::new();
    let snapshot = fleet.read(None, 0);
    let view = &snapshot["fleet"];
    assert_eq!(view["available"], true);
    assert!(view["error"].is_null());
    let sessions: Vec<(Value, Value, Value)> = view["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|session| {
            (
                session["issue"].clone(),
                session["issue_source"].clone(),
                session["age_s"].clone(),
            )
        })
        .collect();
    assert_eq!(
        sessions,
        [
            (json!(42), json!("label"), json!(1200)),
            (json!(77), json!("worktree"), json!(7200)),
            (json!(null), json!(""), json!(3600))
        ]
    );
    assert_eq!(snapshot["sources"]["fleet"]["interval_s"], 30.0);
    let rendered = page(&snapshot);
    for present in [
        "<h2>Devices <span class=\"sub\">1 of 2 reachable",
        "dev-a.example.com",
        "claude 1/2",
        "aider watch only",
        "Example GPU 50 %",
        "16.0 / 64.0 GB",
        "unreachable: ssh exited 255",
        "<h2>Agent sessions <span class=\"sub\">2 running",
        "pid 4242",
        "#42",
        "#77",
        "d-1795640000-9f8e7d6c",
        "exited 3",
        "/srv/work/app-77/sub",
    ] {
        assert!(rendered.contains(present), "{present}");
    }
}

#[test]
fn device_strings_are_redacted_and_escaped() {
    let fleet = FleetFixture::new();
    let mut document = status_document();
    document["devices"][0]["status"]["device"]["hostname"] = json!("<script>alert(1)</script>");
    document["devices"][0]["sessions"][0]["cwd"] = json!(format!("/srv/work/{SECRET_TOKEN}"));
    document["devices"][0]["sessions"][1]["summary"] =
        json!(format!("aider --api-key={SECRET_KEY}"));
    let snapshot = fleet.read(Some(&document.to_string()), 0);
    let rendered = page(&snapshot);
    for surface in [snapshot.to_string(), rendered.clone()] {
        assert!(!surface.contains(SECRET_TOKEN));
        assert!(!surface.contains(SECRET_KEY));
    }
    assert!(!rendered.contains("<script>alert"));
    assert!(rendered.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(rendered.contains("/srv/work/[redacted]"));
}

fn assert_source_error_keeps_the_last_read(snapshot: &Value, expected: &str) {
    let view = &snapshot["fleet"];
    let error = view["error"].as_str().unwrap_or_default();
    assert!(error.contains(expected), "{expected} not in {error}");
    assert_eq!(view["error"], snapshot["sources"]["fleet"]["error"]);
    assert_eq!(
        view["devices"].as_array().map(Vec::len),
        Some(2),
        "the last good read stays"
    );
}

#[test]
fn a_bad_version_or_bad_json_is_a_source_error_that_keeps_the_last_read() {
    let fleet = FleetFixture::new();
    fleet.read(None, 0);
    let mut version_two = status_document();
    version_two["version"] = json!(2);
    let mut soon = status_document();
    soon["generated_at"] = json!("soon");
    for (text, exit_code, expected) in [
        (version_two.to_string(), 0, "version 2, expected 1"),
        ("not json".to_owned(), 0, "fleet exit 0"),
        (String::new(), 2, "fleet exit 2"),
        (soon.to_string(), 0, "generated_at"),
    ] {
        let snapshot = fleet.read(Some(&text), exit_code);
        assert_source_error_keeps_the_last_read(&snapshot, expected);
        assert!(page(&snapshot).contains("last read failed"));
    }
    let mut broken = status_document();
    broken["devices"][0]["sessions"][0]["state"] =
        json!({"kind": format!("paused {SECRET_TOKEN}")});
    let error = fleet.read(Some(&broken.to_string()), 0)["fleet"]["error"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(error.contains("state.kind"), "{error}");
    assert!(!error.contains(SECRET_TOKEN));
    broken["devices"][0]["sessions"][0]["state"] = json!({"kind": "running"});
    let text = broken
        .to_string()
        .replace("\"cpu_percent\":25.5", "\"cpu_percent\":NaN");
    assert!(
        fleet.read(Some(&text), 0)["fleet"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("fleet exit 0")
    );
}

#[test]
fn numbers_outside_fleets_range_are_a_source_error() {
    let fleet = FleetFixture::new();
    fleet.read(None, 0);
    let huge = "1".to_owned() + &"0".repeat(400);
    let cases: [(&[&str], String, &str); 7] = [
        (
            &["status", "metrics", "load"],
            format!("[{huge}, 0, 0]"),
            "out of range",
        ),
        (
            &["status", "metrics", "load"],
            "[-1, 0, 0]".to_owned(),
            "load is not three numbers",
        ),
        (
            &["status", "metrics", "cpu_percent"],
            "1e301".to_owned(),
            "cpu_percent",
        ),
        (
            &["status", "metrics", "cpu_percent"],
            "9223372036854775808".to_owned(),
            "cpu_percent",
        ),
        (&["status", "device", "cpus"], huge.clone(), "out of range"),
        (
            &["status", "metrics", "cpu_percent"],
            "100000000000000000000".to_owned(),
            "cpu_percent",
        ),
        (
            &["status", "metrics", "cpu_percent"],
            "1e20".to_owned(),
            "cpu_percent",
        ),
    ];
    for (keys, value, expected) in cases {
        let mut document = status_document();
        let mut target = &mut document["devices"][0];
        for key in &keys[..keys.len() - 1] {
            target = &mut target[*key];
        }
        target[keys[keys.len() - 1]] = json!("PLACEHOLDER");
        let text = document.to_string().replace("\"PLACEHOLDER\"", &value);
        assert!(super::fleet::parse(&text).is_err(), "{keys:?}");
        let snapshot = fleet.read(Some(&text), 0);
        assert_source_error_keeps_the_last_read(&snapshot, expected);
    }
}

#[test]
fn output_past_the_cap_kills_fleet_and_is_a_source_error() {
    let fleet = FleetFixture::new();
    let collector = fleet.running(
        "yes xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        30.0,
    );
    let started = Instant::now();
    collector.refresh_fleet_in_place();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "killed at the cap, not at the timeout"
    );
    let error = collector.snapshot()["fleet"]["error"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        error.contains(&format!("output passed {FLEET_OUTPUT_MAX_BYTES} bytes")),
        "{error}"
    );
}

#[test]
fn a_fleet_that_hangs_is_stopped_at_the_timeout() {
    let fleet = FleetFixture::new();
    let collector = fleet.running("sleep 30", 0.5);
    let started = Instant::now();
    collector.refresh_fleet_in_place();
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        collector.snapshot()["fleet"]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("TimeoutExpired")
    );
}

#[test]
fn an_issue_label_that_is_not_a_short_ascii_number_is_a_source_error() {
    let fleet = FleetFixture::new();
    fleet.read(None, 0);
    let mut document = status_document();
    for label in [
        "\u{b2}".to_owned(),
        "9".repeat(5000),
        "1234567890".to_owned(),
    ] {
        document["devices"][0]["sessions"][0]["labels"] = json!({"issue": label});
        let snapshot = fleet.read(Some(&document.to_string()), 0);
        assert!(
            snapshot["fleet"]["error"]
                .as_str()
                .unwrap_or_default()
                .contains("labels.issue is not an issue number")
        );
        assert_eq!(
            snapshot["fleet"]["sessions"][0]["issue"], 42,
            "the last good read stays"
        );
        page(&snapshot);
    }
}

#[test]
fn a_partial_result_shows_each_device_error() {
    let fleet = FleetFixture::new();
    let snapshot = fleet.read(None, 1);
    assert!(snapshot["fleet"]["error"].is_null());
    let errors: Vec<Value> = snapshot["fleet"]["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|device| device["error"].clone())
        .collect();
    assert_eq!(errors, [json!(null), json!("unreachable: ssh exited 255")]);
    let snapshot = fleet.read(Some(&fleet_fixture_text("sessions.json")), 1);
    let view = &snapshot["fleet"];
    assert!(view["devices"].is_null());
    assert_eq!(view["errors"], json!(["dev-b: timeout: no answer in 30s"]));
    let sessions: Vec<(Value, Value)> = view["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|session| (session["adapter"].clone(), session["issue"].clone()))
        .collect();
    assert_eq!(sessions, [(json!("codex"), json!(7))]);
    let rendered = page(&snapshot);
    assert!(!rendered.contains("<h2>Devices"));
    assert!(rendered.contains("dev-b: timeout: no answer in 30s"));
    assert!(rendered.contains("dev-a: more sessions than one answer holds"));
}

/// The dashboard's server on a free port for one test, published once, with a known access
/// token.
struct Served {
    harness: FixtureHarness,
    server: Arc<Server>,
    port: u16,
}

fn serve(harness: FixtureHarness) -> Served {
    let home = harness.home.display().to_string();
    let data = harness.home.join("data").display().to_string();
    let gh = harness.gh();
    let usage = harness.home.join("usage-cache.json").display().to_string();
    let arguments = args(&[
        "--harness-home",
        &home,
        "--data-dir",
        &data,
        "--port",
        "0",
        "--gh",
        &gh,
        "--usage-cache",
        &usage,
        "--remote-interval",
        "0",
    ]);
    let app = App::new(&arguments, &environment(&[])).expect("config reads");
    assert_eq!(app.config.repo.as_deref(), Some("example/repo"));
    let collector = Arc::new(app.collector(&gh).expect("collector"));
    collector.refresh_github_in_place();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
    let port = listener
        .local_addr()
        .map(|address| address.port())
        .unwrap_or_default();
    let server = Arc::new(Server::new(
        collector,
        30,
        std::slice::from_ref(&app.github_token),
        ACCESS_TOKEN.to_owned(),
        port,
    ));
    server.publish_in_place();
    listener.set_nonblocking(true).ok();
    let serving = Arc::clone(&server);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
            serving.serve(listener, std::future::pending()).await;
        });
    });
    Served {
        harness,
        server,
        port,
    }
}

struct Reply {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
}

/// One GET with a chosen Host header, or none.
fn request(port: u16, path: &str, host: Option<&str>, headers: &[(&str, &str)]) -> Reply {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connects");
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut text = format!("GET {path} HTTP/1.1\r\n");
    if let Some(host) = host {
        write!(text, "Host: {host}\r\n").ok();
    }
    for (name, value) in headers {
        write!(text, "{name}: {value}\r\n").ok();
    }
    text.push_str("Connection: close\r\n\r\n");
    stream.write_all(text.as_bytes()).expect("sends");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok();
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut body = raw.get(split + 4..).unwrap_or_default().to_vec();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
        .collect();
    if !body.is_empty()
        && headers
            .get("content-encoding")
            .is_some_and(|coding| coding == "gzip")
    {
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(body.as_slice())
            .read_to_end(&mut decoded)
            .ok();
        body = decoded;
    }
    Reply {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

#[test]
fn serves_page_json_and_health_on_loopback_without_the_github_token() {
    let served = serve(FixtureHarness::new());
    let port = served.port;
    assert_eq!(
        served.server.url(),
        format!("http://127.0.0.1:{port}/?t={ACCESS_TOKEN}")
    );
    let host = format!("127.0.0.1:{port}");
    let health = request(
        port,
        &format!("/healthz?t={ACCESS_TOKEN}"),
        Some(&host),
        &[],
    );
    assert_eq!((health.status, health.body.as_str()), (200, "ok\n"));
    let page = request(port, &format!("/?t={ACCESS_TOKEN}"), Some(&host), &[]);
    assert_eq!(
        (page.status, page.headers["content-type"].as_str()),
        (200, "text/html; charset=utf-8")
    );
    let state = request(
        port,
        &format!("/api/state?t={ACCESS_TOKEN}"),
        Some(&format!("localhost:{port}")),
        &[],
    );
    assert_eq!(state.status, 200);
    let snapshot: Value = serde_json::from_str(&state.body).expect("JSON");
    assert_eq!(
        request(
            port,
            &format!("/instance/harness.env?t={ACCESS_TOKEN}"),
            Some(&host),
            &[]
        )
        .status,
        404
    );
    let page = page.body;
    assert!(page.contains("<title>Fixture &lt;b&gt;harness&lt;/b&gt;</title>"));
    assert!(page.contains("<h1>Fixture &lt;b&gt;harness&lt;/b&gt;</h1>"));
    assert!(page.contains("lane: Sonnet 5.2 for ai:lane"));
    assert!(page.contains(&format!("content=\"30;url=/?t={ACCESS_TOKEN}\"")));
    assert!(page.contains(&format!("href=\"/api/state?t={ACCESS_TOKEN}\"")));
    assert!(!state.body.contains(ACCESS_TOKEN));
    assert_eq!(snapshot["repo"], "example/repo");
    // The JSON's object keys come sorted: the map serde_json keeps is ordered by key.
    let top: Vec<&String> = snapshot
        .as_object()
        .map(|map| map.keys().collect())
        .unwrap_or_default();
    assert!(state.body.starts_with(&format!("{{\"{}\":", top[0])));
    let mut sorted = top.clone();
    sorted.sort();
    assert_eq!(top, sorted);
    let counts = &snapshot["queue"]["counts"];
    if cfg!(target_os = "linux") {
        assert_eq!(
            counts["running"].as_i64().unwrap_or_default()
                + counts["claimable"].as_i64().unwrap_or_default(),
            4
        );
    }
    assert!(!page.contains(FAKE_TOKEN));
    assert!(!state.body.contains(FAKE_TOKEN));
    assert_eq!(
        served.server.scrub(&format!("x {FAKE_TOKEN} y")),
        "x [redacted] y"
    );
}

#[test]
fn refuses_a_foreign_host_and_a_missing_or_wrong_access_token() {
    let served = serve(FixtureHarness::new());
    let port = served.port;
    let good = format!("?t={ACCESS_TOKEN}");
    let hosts = [
        Some("attacker.example".to_owned()),
        Some(format!("attacker.example:{port}")),
        Some(format!("127.0.0.1:{}", port.wrapping_add(1))),
        Some("127.0.0.1".to_owned()),
        Some(format!("0.0.0.0:{port}")),
        Some(String::new()),
        None,
    ];
    for host in &hosts {
        for path in [
            "/healthz".to_owned(),
            format!("/{good}"),
            format!("/api/state{good}"),
        ] {
            let reply = request(port, &path, host.as_deref(), &[]);
            assert_eq!(reply.status, 421, "{host:?} {path}");
            assert!(!reply.body.contains("Harness status"));
        }
    }
    let host = format!("127.0.0.1:{port}");
    for path in [
        "/".to_owned(),
        "/api/state".to_owned(),
        "/?t=".to_owned(),
        "/api/state?t=wrong".to_owned(),
        format!("/?x={ACCESS_TOKEN}"),
        format!("/api/state?t={ACCESS_TOKEN}x"),
    ] {
        let reply = request(port, &path, Some(&host), &[]);
        assert_eq!(reply.status, 403, "{path}");
        assert!(!reply.body.contains("repo"));
    }
    assert_eq!(
        request(
            port,
            &format!("/{good}"),
            Some(&format!("LOCALHOST:{port}")),
            &[]
        )
        .status,
        200
    );
}

#[test]
fn every_response_carries_the_security_headers() {
    let served = serve(FixtureHarness::new());
    let port = served.port;
    let host = format!("127.0.0.1:{port}");
    for (path, host, expected) in [
        (format!("/?t={ACCESS_TOKEN}"), host.clone(), 200),
        (format!("/api/state?t={ACCESS_TOKEN}"), host.clone(), 200),
        ("/".to_owned(), host.clone(), 403),
        ("/".to_owned(), "evil.example".to_owned(), 421),
        (format!("/missing?t={ACCESS_TOKEN}"), host, 404),
    ] {
        let reply = request(port, &path, Some(&host), &[]);
        assert_eq!(reply.status, expected, "{path}");
        assert_eq!(reply.headers["cross-origin-resource-policy"], "same-origin");
        assert_eq!(reply.headers["x-content-type-options"], "nosniff");
        assert_eq!(reply.headers["referrer-policy"], "no-referrer");
        let policy = &reply.headers["content-security-policy"];
        assert!(policy.contains("default-src 'none'"));
        assert!(policy.contains("frame-ancestors 'none'"));
        assert!(!policy.contains("http"));
        assert!(!policy.contains('*'));
    }
}

#[test]
fn conditional_get_and_gzip_variants() {
    let served = serve(FixtureHarness::new());
    let port = served.port;
    let host = format!("127.0.0.1:{port}");
    let query = format!("?t={ACCESS_TOKEN}");
    let published = request(port, &format!("/api/state{query}"), Some(&host), &[]).body;
    // Every source fails from here on: a GET that collected would show it.
    std::fs::write(served.harness.home.join("gh-fail"), "").ok();
    std::fs::remove_file(served.harness.home.join("bin/resource-status")).ok();
    std::fs::remove_file(served.harness.home.join("usage-cache.json")).ok();
    for path in ["/", "/api/state", "/api/sections", "/dashboard.js"] {
        let plain = request(port, &format!("{path}{query}"), Some(&host), &[]);
        assert_eq!(plain.status, 200);
        assert_eq!(
            plain.headers["etag"],
            format!("\"{}\"", super::text::sha256_hex(plain.body.as_bytes()))
        );
        let zipped = request(
            port,
            &format!("{path}{query}"),
            Some(&host),
            &[
                ("Accept-Encoding", "br, gzip;q=0.8"),
                ("If-None-Match", &plain.headers["etag"]),
            ],
        );
        assert_eq!(
            (
                zipped.status,
                zipped.body.as_str(),
                zipped.headers["content-encoding"].as_str()
            ),
            (200, plain.body.as_str(), "gzip")
        );
        assert_ne!(plain.headers["etag"], zipped.headers["etag"]);
        assert!(
            zipped.headers["content-length"]
                .parse::<usize>()
                .unwrap_or(usize::MAX)
                < plain.body.len()
        );
        let unchanged = request(
            port,
            &format!("{path}{query}"),
            Some(&host),
            &[
                ("Accept-Encoding", "gzip"),
                (
                    "If-None-Match",
                    &format!("\"old\", W/{}", zipped.headers["etag"]),
                ),
            ],
        );
        assert_eq!(
            (
                unchanged.status,
                unchanged.body.as_str(),
                &unchanged.headers["etag"]
            ),
            (304, "", &zipped.headers["etag"])
        );
        assert_eq!(unchanged.headers["cache-control"], "private, no-cache");
        assert_eq!(unchanged.headers["vary"], "Accept-Encoding");
    }
    for accepted in ["br", "gzip;q=0, *;q=1", "gzip;q=0.4, identity;q=1"] {
        let reply = request(
            port,
            &format!("/api/state{query}"),
            Some(&host),
            &[("Accept-Encoding", accepted)],
        );
        assert_eq!(reply.status, 200);
        assert!(
            !reply.headers.contains_key("content-encoding"),
            "{accepted}"
        );
    }
    assert_eq!(
        request(port, &format!("/api/state{query}"), Some(&host), &[]).body,
        published
    );
}

#[test]
fn sections_track_content_and_require_the_token() {
    let served = serve(FixtureHarness::new());
    let port = served.port;
    let host = format!("127.0.0.1:{port}");
    let query = format!("?t={ACCESS_TOKEN}");
    let index = request(port, &format!("/api/sections{query}"), Some(&host), &[]);
    let before: serde_json::Map<String, Value> =
        serde_json::from_str(&index.body).expect("index JSON");
    let fragment = format!(
        "/api/sections/{}",
        before["ticks"].as_str().unwrap_or_default()
    );
    for path in [
        "/healthz",
        "/dashboard.js",
        "/api/sections",
        fragment.as_str(),
    ] {
        for credential in ["", "?t=wrong"] {
            assert_eq!(
                request(
                    port,
                    &format!("{path}{credential}"),
                    Some(&host),
                    &[("If-None-Match", "*")]
                )
                .status,
                403
            );
        }
        assert_eq!(
            request(
                port,
                &format!("{path}{query}"),
                Some("foreign.example"),
                &[]
            )
            .status,
            421
        );
    }
    let page = request(port, &format!("/{query}"), Some(&host), &[]).body;
    assert!(page.contains(&format!(
        "<noscript><meta http-equiv=\"refresh\" content=\"30;url=/{query}\"></noscript>"
    )));
    assert!(page.contains(&format!(
        "data-section=\"ticks\" data-version=\"{}\"",
        before["ticks"].as_str().unwrap_or_default()
    )));
    let body = request(port, &format!("{fragment}{query}"), Some(&host), &[]);
    assert_eq!(body.status, 200);
    assert!(body.body.contains("Running ticks"));
    assert!(!body.body.contains("<style"));
    assert_eq!(
        super::text::sha256_hex(body.body.as_bytes()),
        before["ticks"].as_str().unwrap_or_default()
    );
    // The index lists the machines in the page's order, which a sorted map would lose.
    let state: Value =
        serde_json::from_str(&request(port, &format!("/api/state{query}"), Some(&host), &[]).body)
            .expect("state JSON");
    let expected: Vec<String> = state["machines"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|machine| {
            format!(
                "machine-{}",
                super::text::sha256_hex(machine["key"].as_str().unwrap_or_default().as_bytes())
            )
        })
        .collect();
    let mut listed: Vec<(usize, String)> = expected
        .iter()
        .filter_map(|name| Some((index.body.find(&format!("\"{name}\""))?, name.clone())))
        .collect();
    listed.sort();
    assert_eq!(
        listed.into_iter().map(|(_, name)| name).collect::<Vec<_>>(),
        expected
    );
    assert!(expected.len() >= 2);

    let mut snapshot: Value =
        serde_json::from_str(&request(port, &format!("/api/state{query}"), Some(&host), &[]).body)
            .expect("state JSON");
    snapshot["repo"] = json!(format!("different/repo {FAKE_TOKEN}"));
    served.server.publish_snapshot(snapshot);
    let changed = request(
        port,
        &format!("/api/sections{query}"),
        Some(&host),
        &[("If-None-Match", &index.headers["etag"])],
    );
    let after: serde_json::Map<String, Value> =
        serde_json::from_str(&changed.body).expect("index JSON");
    assert_eq!(changed.status, 200);
    let differing: Vec<&String> = before
        .keys()
        .filter(|name| before[*name] != after[*name])
        .collect();
    assert_eq!(differing, ["header"]);
    let header = request(
        port,
        &format!(
            "/api/sections/{}{query}",
            after["header"].as_str().unwrap_or_default()
        ),
        Some(&host),
        &[],
    )
    .body;
    assert!(header.contains("different/repo [redacted]"));
    assert!(!header.contains(FAKE_TOKEN));
}

#[test]
fn a_token_cut_by_an_error_message_leaves_no_part_behind() {
    let harness = FixtureHarness::new();
    let gh = harness.home.join("bin/gh-straddle");
    let padding = "x".repeat(290);
    write_script(
        &gh,
        &format!("#!/usr/bin/env bash\necho \"{padding} $GH_TOKEN\" >&2\nexit 1\n"),
    );
    let counter = harness.home.join("bin/count-straddle");
    write_script(
        &counter,
        &format!(
            "#!/usr/bin/env bash\necho \"$GH_TOKEN {}\" >&2\nexit 3\n",
            "y".repeat(190)
        ),
    );
    let mut config = harness.config();
    config.queue_depth_command = vec![counter.display().to_string()];
    let github =
        GitHubClient::new("example/repo", Some(FAKE_TOKEN), &gh.display().to_string()).ok();
    let collector = Collector::new(config, github, pinned());
    collector.refresh_github_in_place();
    collector.refresh_claimable_in_place();
    let snapshot = collector.snapshot();
    for error in [
        &snapshot["queue"]["error"],
        &snapshot["queue"]["claimable_sh"]["error"],
    ] {
        let error = error.as_str().unwrap_or_default();
        assert!(!error.is_empty());
        for fragment in ["ghp_", "FAKEtoken", "0123456789"] {
            assert!(!error.contains(fragment), "{fragment} in {error}");
        }
    }
}

#[test]
fn a_symlinked_state_directory_reads_as_its_target() {
    let directory = TempDir::new();
    let home = directory.path().join("home");
    let real = directory.path().join("real-state");
    std::fs::create_dir_all(&home).ok();
    std::fs::create_dir_all(&real).ok();
    std::os::unix::fs::symlink(&real, home.join("state")).ok();
    let config = config_of(&home, &[]);
    assert_eq!(config.state_dir(), real);
    assert_eq!(config.data_dir, real.join("dashboard"));
}

#[test]
fn harness_bin_from_the_env_file_expands_home_and_a_tilde() {
    let harness = FixtureHarness::new();
    let env_file = harness.home.join("instance/harness.env");
    let original = std::fs::read_to_string(&env_file).unwrap_or_default();
    for (line, expected) in [
        ("HARNESS_BIN=$HOME/bin/harness", "/home/someone/bin/harness"),
        ("HARNESS_BIN=~/bin/harness", "/home/someone/bin/harness"),
    ] {
        std::fs::write(
            &env_file,
            format!("{original}{line}\nQUEUE_DEPTH_CMD=\"$HARNESS_BIN claimable\"\n"),
        )
        .ok();
        let config = config_of(&harness.home, &[("HOME", "/home/someone".to_owned())]);
        assert_eq!(config.harness_bin, expected, "{line}");
        assert_eq!(
            config.queue_depth_command,
            [expected, "claimable"],
            "{line}"
        );
    }
}

#[test]
fn the_last_of_a_repeated_option_wins() {
    let parsed = args(&[
        "--port",
        "1",
        "--port",
        "2",
        "--refresh",
        "5",
        "--refresh",
        "7",
    ]);
    assert_eq!((parsed.port, parsed.refresh), (Some(2), 7));
}

#[test]
fn a_host_that_reads_as_an_ssh_option_is_refused() {
    let harness = FixtureHarness::new();
    let collector = harness.collector();
    let error = collector
        .probe_host("-oProxyCommand=touch /tmp/x", "true")
        .err()
        .unwrap_or_default();
    assert!(error.contains("refusing host"), "{error}");
}

#[test]
fn the_server_listens_on_loopback_only() {
    let listener = super::bind_loopback(0).expect("binds");
    let address = listener.local_addr().expect("an address");
    assert!(address.ip().is_loopback());
    assert_eq!(
        address.ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
}

#[test]
fn every_agents_windows_are_weighed_into_the_pool() {
    let harness = FixtureHarness::new();
    let sessions = harness.home.join("codex/sessions/2026/11/28");
    std::fs::create_dir_all(&sessions).ok();
    let resets = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
        + 86_400;
    std::fs::write(
        sessions.join("rollout-2026-11-28T00-00-00-x.jsonl"),
        format!(
            "{}\n",
            json!({"type": "event_msg", "payload": {"type": "token_count", "rate_limits": {
                "primary": {"used_percent": 82.0, "window_minutes": 10080, "resets_at": resets}, "secondary": null}}})
        ),
    )
    .ok();
    std::fs::remove_file(harness.state.join("tick-model")).ok();
    let mut config = harness.config();
    config.usage_settings = vec![
        (
            "TICK_MODELS".to_owned(),
            "claude:claude-opus-5-5 codex:gpt-5.5 gemini:gemini-2.5-pro".to_owned(),
        ),
        ("MAX_SLOTS".to_owned(), "6".to_owned()),
        ("USAGE_SOURCE_gemini".to_owned(), "none".to_owned()),
        (
            "USAGE_SOURCE_codex".to_owned(),
            format!(
                "codex-sessions:{}",
                harness.home.join("codex/sessions").display()
            ),
        ),
    ];
    let collector = harness.collector_with(config, pinned());
    let snapshot = collector.snapshot();
    let usage = &snapshot["usage"];
    let agents: Vec<(Value, Value, Value)> = usage["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|agent| {
            (
                agent["agent"].clone(),
                agent["reading"]["state"].clone(),
                agent["cap"].clone(),
            )
        })
        .collect();
    assert_eq!(
        agents,
        [
            (json!("claude"), json!("fresh"), json!(3)),
            (json!("codex"), json!("fresh"), json!(3)),
            (json!("gemini"), json!("unmetered"), json!(6)),
        ]
    );
    assert_eq!(usage["agents"][1]["windows"][0]["name"], "primary");
    assert_eq!(
        (&usage["pool_cap"], &usage["width"]),
        (&json!(6), &json!(6))
    );
    assert_eq!(
        snapshot["harness"]["tick_models"],
        json!([
            "claude:claude-opus-5-5",
            "codex:gpt-5.5",
            "gemini:gemini-2.5-pro"
        ])
    );
    let rendered = page(&snapshot);
    for text in [
        "claude five_hour",
        "claude weekly",
        "codex primary",
        "82 % &middot; cap 3",
        "unmetered",
        "pool at most 6 of 6, room for 6",
        "codex gpt-5.5",
    ] {
        assert!(rendered.contains(text), "{text}");
    }
}

#[test]
fn a_missing_reading_shows_as_unreadable_and_allows_nothing() {
    let harness = FixtureHarness::new();
    std::fs::remove_file(harness.home.join("usage-cache.json")).ok();
    let snapshot = harness.collector().snapshot();
    let claude = &snapshot["usage"]["agents"][0];
    assert_eq!(claude["reading"]["state"], "unreadable");
    assert_eq!(
        (&claude["cap"], &snapshot["usage"]["pool_cap"]),
        (&json!(0), &json!(0))
    );
    assert_eq!(snapshot["harness"]["work_waiting"], false);
    assert!(page(&snapshot).contains("unreadable: "));
}

#[cfg(target_os = "linux")]
#[test]
fn marked_ticks_of_any_agent_show_with_their_kind() {
    use std::os::unix::process::CommandExt as _;
    let mut harness = FixtureHarness::new();
    let work = harness.start_marked_tick(2464, 1, "sleep 30", "claude", "claude-opus-5-5", "work");
    let lane = harness.start_marked_tick(2490, 2, "sleep 30", "codex", "gpt-5.5", "lane");
    let landing =
        harness.start_marked_tick(2201, 3, "sleep 30", "gemini", "gemini-2.5-pro", "landing");
    // Not ticks: a process carrying this state directory's marker that no slot file records, and
    // one whose slot file records a start time it does not have, as a reused pid would.
    let unrecorded = Command::new("sleep")
        .arg("30")
        .env(
            "HARNESS_TICK",
            format!(
                "state={} slot=5 agent=claude model=m kind=work",
                harness.state.display()
            ),
        )
        .process_group(0)
        .spawn()
        .expect("sleep starts");
    let reused = Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .expect("sleep starts");
    std::fs::write(
        harness.state.join("locks/slot4.tick"),
        format!(
            "{} {} claude m work\n",
            reused.id(),
            start_ticks(i64::from(reused.id())) + 1
        ),
    )
    .expect("the slot file writes");
    let decoys = [i64::from(unrecorded.id()), i64::from(reused.id())];
    harness.helpers.push(unrecorded);
    harness.helpers.push(reused);
    std::thread::sleep(Duration::from_millis(200));
    let collector = harness.collector();
    collector.refresh_github_in_place();
    let snapshot = collector.snapshot();
    let ticks: HashMap<i64, (Value, Value, Value, Value)> = snapshot["ticks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tick| {
            Some((
                tick["pid"].as_i64()?,
                (
                    tick["agent"].clone(),
                    tick["kind"].clone(),
                    tick["slot"].clone(),
                    tick["issue"].clone(),
                ),
            ))
        })
        .collect();
    assert_eq!(ticks.len(), 3, "{ticks:?}");
    assert_eq!(
        ticks[&work],
        (json!("claude"), json!("work"), json!(1), json!(2464))
    );
    assert_eq!(
        ticks[&lane],
        (json!("codex"), json!("lane"), json!(2), json!(2490))
    );
    assert_eq!(
        ticks[&landing],
        (json!("gemini"), json!("landing"), json!(3), json!(2201))
    );
    for decoy in decoys {
        assert!(!ticks.contains_key(&decoy));
    }
    let cards: HashMap<i64, Value> = snapshot["flow"]["cards"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|card| Some((card["issue"].as_i64()?, card.clone())))
        .collect();
    assert_eq!(cards[&2201]["column"], "landing");
    assert!(
        cards[&2464]["chips"]
            .as_array()
            .is_some_and(|chips| chips.contains(&json!("claude Opus 5.5")))
    );
    let rendered = page(&snapshot);
    for text in [
        "codex gpt-5.5 lane",
        "gemini gemini-2.5-pro landing",
        "claude Opus 5.5",
    ] {
        assert!(rendered.contains(text), "{text}");
    }
}

#[test]
fn the_model_list_and_usage_keys_come_from_the_env_file_expanded() {
    let harness = FixtureHarness::new();
    let env_file = harness.home.join("instance/harness.env");
    let text = std::fs::read_to_string(&env_file).unwrap_or_default();
    std::fs::write(
        &env_file,
        format!(
            "{text}TICK_MODELS=\"claude:claude-opus-5-5 codex:gpt-5.5\"\nTICK_AGENT=claude\nMAX_SLOTS=4\nTICK_WIDTH=3\n\
             MODEL_COOLDOWN_S=600\nUSAGE_SOURCE_codex=codex-sessions:$HOME/codex\nUSAGE_STEPS_SHORT=\"50 60 70\"\n\
             AGENT_FLAGS_codex=--full-auto\nUNRELATED=1\n"
        ),
    )
    .ok();
    let config = config_of(&harness.home, &[("HOME", "/home/someone".to_owned())]);
    let settings: HashMap<&str, &str> = config
        .usage_settings
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    assert_eq!(
        settings["TICK_MODELS"],
        "claude:claude-opus-5-5 codex:gpt-5.5"
    );
    assert_eq!(
        settings["USAGE_SOURCE_codex"],
        "codex-sessions:/home/someone/codex"
    );
    assert_eq!(settings["USAGE_STEPS_SHORT"], "50 60 70");
    assert_eq!(settings["AGENT_FLAGS_codex"], "--full-auto");
    assert_eq!(
        (
            settings["MAX_SLOTS"],
            settings["TICK_WIDTH"],
            settings["LANE_MODEL"]
        ),
        ("4", "3", "claude-sonnet-5-2")
    );
    assert!(!settings.contains_key("UNRELATED"));
    assert!((config.model_cooldown_s - 600.0).abs() < f64::EPSILON);
    for key in [
        "USAGE_FETCH_codex",
        "AGENT_BIN_gemini",
        "TICK_MODELS",
        "MODEL_COOLDOWN_S",
        "GH_TOKEN",
    ] {
        assert!(super::wanted_key(key), "{key}");
    }
    assert!(!super::wanted_key("OTHER_SECRET"));
}
