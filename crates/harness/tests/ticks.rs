//! `harness::ticks::scan` against real processes recorded in slot files, as the runner records them.
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use harness::ticks::{Kind, Tick, scan};

/// Children killed, with their process groups, when the test ends.
struct Procs(Vec<Child>);

impl Procs {
    fn spawn(&mut self, command: &mut Command) -> u32 {
        use std::os::unix::process::CommandExt as _;
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        self.0.push(child);
        pid
    }
}

impl Drop for Procs {
    fn drop(&mut self) {
        for child in &mut self.0 {
            if let Some(group) = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap())
            {
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            }
            let _ = child.wait();
        }
    }
}

/// A scratch directory, with a state directory and its locks, that no other test names.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ticks-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("state/locks")).unwrap();
    dir
}

fn state_of(dir: &Path) -> String {
    dir.join("state").to_string_lossy().into_owned()
}

/// Field 22 of `/proc/<pid>/stat`, read as the runner reads it.
fn start_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let (_, rest) = stat.rsplit_once(')').unwrap();
    rest.split_whitespace().nth(19).unwrap().parse().unwrap()
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap()
        .trim()
        .to_owned()
}

/// Records `pid` as slot `slot`'s tick, as the runner does once it has started the agent.
fn record(dir: &Path, slot: u32, pid: u32, start: u64, agent: &str, kind: &str) {
    std::fs::write(
        dir.join(format!("state/locks/slot{slot}.tick")),
        format!("{pid} {start} {} {agent} model-{slot} {kind}\n", boot_id()),
    )
    .unwrap();
}

/// Polls the scan until `done` holds for its result, for at most ten seconds.
fn until(state: &str, done: impl Fn(&[Tick]) -> bool) -> Vec<Tick> {
    let start = Instant::now();
    loop {
        let ticks = scan(state).unwrap();
        if done(&ticks) || start.elapsed() > Duration::from_secs(10) {
            return ticks;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn pids(ticks: &[Tick]) -> Vec<u32> {
    ticks.iter().map(|tick| tick.pid).collect()
}

fn sleeper() -> Command {
    let mut command = Command::new("sleep");
    command.arg("30");
    command
}

#[test]
fn a_recorded_live_process_is_a_tick_with_its_fields() {
    let dir = scratch("fields");
    let mut procs = Procs(Vec::new());
    let pid = procs.spawn(&mut sleeper());
    record(&dir, 2, pid, start_ticks(pid), "codex", "landing");
    let ticks = scan(&state_of(&dir)).unwrap();
    assert_eq!(
        ticks,
        [Tick {
            pid,
            age_s: ticks.first().map_or(0, |tick| tick.age_s),
            slot: 2,
            agent: "codex".to_owned(),
            model: "model-2".to_owned(),
            kind: Kind::Landing,
        }]
    );
    assert!(ticks[0].age_s < 10);
}

#[test]
fn another_state_directory_is_never_counted() {
    let ours = scratch("ours");
    let theirs = scratch("theirs");
    let mut procs = Procs(Vec::new());
    let mine = procs.spawn(&mut sleeper());
    let other = procs.spawn(&mut sleeper());
    record(&ours, 1, mine, start_ticks(mine), "claude", "work");
    record(&theirs, 1, other, start_ticks(other), "claude", "work");
    assert_eq!(pids(&scan(&state_of(&ours)).unwrap()), [mine]);
    assert_eq!(pids(&scan(&state_of(&theirs)).unwrap()), [other]);
}

#[test]
fn a_stale_or_reused_pid_is_not_a_tick() {
    let dir = scratch("stale");
    let mut procs = Procs(Vec::new());
    let live = procs.spawn(&mut sleeper());
    // A reused pid: the process lives, but started after the tick the file recorded.
    record(&dir, 1, live, start_ticks(live) - 1, "claude", "work");
    // A tick that ended: its process is gone.
    let mut gone = Command::new("true").spawn().unwrap();
    let gone_pid = gone.id();
    gone.wait().unwrap();
    record(&dir, 2, gone_pid, 1, "claude", "work");
    // Malformed and misnamed files are not ticks either.
    std::fs::write(dir.join("state/locks/slot3.tick"), "not a record\n").unwrap();
    std::fs::write(
        dir.join("state/locks/slotx.tick"),
        format!("{live} {} {} claude m work\n", start_ticks(live), boot_id()),
    )
    .unwrap();
    // A file a crash left behind, from another boot: its pid and start time may match by chance.
    std::fs::write(
        dir.join("state/locks/slot4.tick"),
        format!(
            "{live} {} 00000000-0000-0000-0000-000000000000 claude m work\n",
            start_ticks(live)
        ),
    )
    .unwrap();
    assert_eq!(scan(&state_of(&dir)).unwrap(), []);
}

/// Builds tests/fixtures/nodump.c: a stand-in agent that marks itself non-dumpable, as the release
/// Codex CLI does, then runs a tool shell and sleeps.
fn nodump(dir: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/nodump.c");
    let exe = dir.join("nodump");
    let built = Command::new("cc")
        .arg("-o")
        .arg(&exe)
        .arg(&source)
        .status()
        .unwrap();
    assert!(built.success(), "cc builds the stand-in agent");
    exe
}

#[test]
fn a_non_dumpable_agent_is_a_tick() {
    let dir = scratch("nodump");
    let exe = nodump(&dir);
    let mut procs = Procs(Vec::new());
    let pid = procs.spawn(Command::new(&exe).args(["sleep 30", "30"]));
    // Give it time to mark itself, then confirm what a reader may no longer see.
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read(format!("/proc/{pid}/environ")).is_ok() {
        assert!(
            Instant::now() < deadline,
            "the stand-in never became non-dumpable"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    record(&dir, 1, pid, start_ticks(pid), "codex", "work");
    assert_eq!(pids(&scan(&state_of(&dir)).unwrap()), [pid]);
}

#[test]
fn a_tool_child_that_outlives_its_parent_is_never_a_second_tick() {
    let dir = scratch("orphans");
    let mut procs = Procs(Vec::new());
    let marker = dir.join("orphan-started");
    // The agent starts a nohup'd child and a subshell's background child, both of which outlive it.
    let agent = procs.spawn(Command::new("sh").arg("-c").arg(format!(
        "nohup sleep 30 >/dev/null 2>&1 & (sleep 30 &); touch '{}'; exec sleep 30",
        marker.display()
    )));
    record(&dir, 1, agent, start_ticks(agent), "claude", "work");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "the agent never started its children"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(pids(&until(&state_of(&dir), |t| !t.is_empty())), [agent]);
    // The agent ends; its children live on, and the slot holds no tick.
    let _ = rustix::process::kill_process(
        rustix::process::Pid::from_raw(i32::try_from(agent).unwrap()).unwrap(),
        rustix::process::Signal::KILL,
    );
    // Reaped, as the runner's wait reaps it.
    procs.0[0].wait().unwrap();
    assert_eq!(until(&state_of(&dir), <[Tick]>::is_empty), []);
}
