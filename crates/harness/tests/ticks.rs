//! `harness::ticks::scan` against real processes that carry the runner's marker.
#![cfg(target_os = "linux")]

use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use harness::ticks::{Kind, Tick, scan};

/// Children killed, and their process groups with them, when the test ends.
struct Procs(Vec<Child>);

impl Procs {
    /// Starts `command` holding `lock` open, as a tick holds its slot's lock, or holding none.
    fn spawn(&mut self, command: &mut Command, lock: Option<&Path>) -> u32 {
        let stdout = lock.map_or_else(Stdio::null, |lock| Stdio::from(File::create(lock).unwrap()));
        let child = command
            .stdin(Stdio::null())
            .stdout(stdout)
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

/// A state directory, with its locks directory, that no other test or installation names.
fn unique_state(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("ticks-{name}-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("state/locks")).unwrap();
    dir.join("state").to_string_lossy().into_owned()
}

fn slot_lock(state: &str, slot: u32) -> PathBuf {
    Path::new(state).join(format!("locks/slot{slot}.lock"))
}

fn marker(state: &str, slot: u32, agent: &str, kind: &str) -> String {
    format!("state={state} slot={slot} agent={agent} model=model-{slot} kind={kind}")
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

/// Waits until the process group `group` holds at least `count` processes.
fn wait_for_group(group: u32, count: usize) {
    let start = Instant::now();
    let size = || {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|entry| std::fs::read_to_string(entry.path().join("stat")).ok())
            .filter(|stat| {
                stat.rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().nth(2)?.parse::<u32>().ok())
                    == Some(group)
            })
            .count()
    };
    while size() < count {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "group {group} never reached {count}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn pids(ticks: &[Tick]) -> Vec<u32> {
    ticks.iter().map(|tick| tick.pid).collect()
}

#[test]
fn a_marked_process_is_a_tick_with_its_fields() {
    let state = unique_state("fields");
    let mut procs = Procs(Vec::new());
    let pid = procs.spawn(
        Command::new("sleep")
            .arg("30")
            .env("HARNESS_TICK", marker(&state, 2, "codex", "landing")),
        Some(&slot_lock(&state, 2)),
    );
    let ticks = until(&state, |ticks| !ticks.is_empty());
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
    let ours = unique_state("ours");
    let theirs = unique_state("theirs");
    let mut procs = Procs(Vec::new());
    let mine = procs.spawn(
        Command::new("sleep")
            .arg("30")
            .env("HARNESS_TICK", marker(&ours, 1, "claude", "work")),
        Some(&slot_lock(&ours, 1)),
    );
    let other = procs.spawn(
        Command::new("sleep")
            .arg("30")
            .env("HARNESS_TICK", marker(&theirs, 1, "claude", "work")),
        Some(&slot_lock(&theirs, 1)),
    );
    assert_eq!(pids(&until(&ours, |ticks| !ticks.is_empty())), [mine]);
    assert_eq!(pids(&until(&theirs, |ticks| !ticks.is_empty())), [other]);
    // A state that merely starts with ours is another directory.
    let prefixed = format!("{ours}-2");
    assert!(scan(&prefixed).unwrap().is_empty());
}

#[test]
fn the_topmost_marked_process_is_the_tick_and_its_children_are_not() {
    let state = unique_state("topmost");
    let mut procs = Procs(Vec::new());
    // A shell that starts a tool shell, which starts a command: three marked generations.
    let shell = procs.spawn(
        Command::new("sh")
            .args(["-c", "sh -c 'sleep 30 & wait' & wait"])
            .env("HARNESS_TICK", marker(&state, 1, "claude", "work")),
        Some(&slot_lock(&state, 1)),
    );
    let ticks = until(&state, |ticks| !ticks.is_empty());
    // Read again once the grandchildren run: they must not show.
    wait_for_group(shell, 3);
    let again = scan(&state).unwrap();
    assert_eq!(pids(&ticks), [shell]);
    assert_eq!(pids(&again), [shell]);
}

#[test]
fn a_node_shim_resolves_to_the_shim_not_its_native_agent() {
    let state = unique_state("shim");
    let dir = PathBuf::from(state.trim_end_matches("/state"));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("agent.js");
    std::fs::write(&script, "sleep 30 &\nwait\n").unwrap();
    let mut procs = Procs(Vec::new());
    // argv[0] is node and argv[1] the script, as an npm-installed agent CLI runs.
    let shim = procs.spawn(
        Command::new("/bin/sh")
            .arg0("node")
            .arg(&script)
            .env("HARNESS_TICK", marker(&state, 4, "gemini", "work")),
        Some(&slot_lock(&state, 4)),
    );
    let ticks = until(&state, |ticks| !ticks.is_empty());
    wait_for_group(shim, 2);
    let again = scan(&state).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(pids(&ticks), [shim]);
    assert_eq!(pids(&again), [shim]);
    assert_eq!(again[0].agent, "gemini");
}

#[test]
fn a_child_whose_marked_parent_is_another_installation_is_a_tick() {
    let ours = unique_state("nested-ours");
    let theirs = unique_state("nested-theirs");
    let mut procs = Procs(Vec::new());
    let inner = format!(
        "HARNESS_TICK='{}' sleep 30 >'{}' & wait",
        marker(&ours, 1, "claude", "work"),
        slot_lock(&ours, 1).display()
    );
    let outer = procs.spawn(
        Command::new("sh")
            .args(["-c", &inner])
            .env("HARNESS_TICK", marker(&theirs, 1, "claude", "work")),
        Some(&slot_lock(&theirs, 1)),
    );
    let ticks = until(&ours, |ticks| !ticks.is_empty());
    assert_eq!(ticks.len(), 1);
    assert_ne!(ticks[0].pid, outer);
    assert_eq!(pids(&scan(&theirs).unwrap()), [outer]);
}

#[test]
fn a_marked_process_without_its_slot_lock_is_not_a_tick() {
    let state = unique_state("daemon");
    let mut procs = Procs(Vec::new());
    // A daemon a tick left behind: it kept the marker and closed the descriptors it started with.
    procs.spawn(
        Command::new("sleep")
            .arg("30")
            .env("HARNESS_TICK", marker(&state, 1, "claude", "landing")),
        None,
    );
    // Another slot's lock is not its own.
    procs.spawn(
        Command::new("sleep")
            .arg("30")
            .env("HARNESS_TICK", marker(&state, 2, "claude", "work")),
        Some(&slot_lock(&state, 3)),
    );
    let tick = procs.spawn(
        Command::new("sleep")
            .arg("30")
            .env("HARNESS_TICK", marker(&state, 3, "claude", "work")),
        Some(&slot_lock(&state, 3)),
    );
    assert_eq!(pids(&until(&state, |ticks| !ticks.is_empty())), [tick]);
}
