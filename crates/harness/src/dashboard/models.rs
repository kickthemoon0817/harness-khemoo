//! Records the dashboard reads from the harness state, the process table and the hosts.

use std::collections::BTreeMap;

use serde::Serialize;

/// One `state/host_health.tsv` row: a remote host's probe, written by the instance.
#[derive(Clone, Debug)]
pub struct HostHealthRow {
    pub time: String,
    pub epoch: f64,
    pub host: String,
    pub connect_s: Option<f64>,
    pub upload_mb_s: Option<f64>,
    pub upload_timed_out: bool,
    pub load1: Option<f64>,
    pub nproc: Option<i64>,
    pub gpu_util_pct: Option<f64>,
    pub gpu_mem_used_mb: Option<f64>,
    pub status: String,
}

/// One `state/host_health.streaks` line: whether the probe has demoted a host, and its streaks.
#[derive(Clone, Debug, Serialize)]
pub struct HostStreak {
    pub host: String,
    pub state: String,
    pub bad_probes: i64,
    pub good_probes: i64,
    pub since_epoch: i64,
}

/// A remote host named by the resource status, with the slot numbers that live on it.
#[derive(Clone, Debug, Serialize)]
pub struct RemoteCard {
    pub host: String,
    pub first_slot: i64,
    pub last_slot: i64,
}

/// One slot (or the device lease) as the resource status reports it.
#[derive(Clone, Debug, Serialize)]
pub struct LeaseSlot {
    pub slot: Option<i64>,
    pub state: String,
    pub holder: Option<String>,
    pub issue: Option<i64>,
    pub pid: Option<i64>,
    pub since: Option<String>,
    pub domain: Option<i64>,
    pub tags: Vec<String>,
    pub host: String,
}

/// A recorded wait for a slot, the whole device or the whole pool.
#[derive(Clone, Debug, Serialize)]
pub struct LeaseWait {
    pub kind: String,
    pub issue: i64,
    pub minutes: i64,
    pub tag: String,
}

/// The parsed output of the resource status command.
#[derive(Clone, Debug, Default)]
pub struct LeaseStatus {
    pub remote_cards: Vec<RemoteCard>,
    pub harness_only_hosts: Vec<String>,
    pub local_card_off: bool,
    pub slots: Vec<LeaseSlot>,
    pub device: Option<LeaseSlot>,
    pub operator_priority: Vec<String>,
    pub waits: Vec<LeaseWait>,
    pub unparsed_lines: Vec<String>,
}

impl LeaseStatus {
    /// The slots and the device lease.
    pub fn slots_and_device(&self) -> Vec<LeaseSlot> {
        self.slots
            .iter()
            .chain(self.device.iter())
            .cloned()
            .collect()
    }
}

/// One `state/stalls.log` line: `<UTC> STALL <kind> <key> <detail>`.
#[derive(Clone, Debug, Serialize)]
pub struct StallLine {
    pub time: String,
    pub kind: String,
    pub key: String,
    pub detail: String,
}

/// One `state/logs/allocator.log` line, with its `key=value` fields lifted out.
#[derive(Clone, Debug, Serialize)]
pub struct AllocatorLine {
    pub time: String,
    pub kind: String,
    pub slot: Option<String>,
    pub fields: BTreeMap<String, String>,
    pub text: String,
}

/// The usage windows from `~/.claude/usage-cache.json`.
#[derive(Clone, Debug, Serialize)]
pub struct UsageReading {
    pub five_hour_pct: Option<f64>,
    pub weekly_pct: Option<f64>,
    pub five_hour_resets_at: Option<String>,
    pub weekly_resets_at: Option<String>,
    pub taken_at_epoch: Option<f64>,
}

/// One `state/locks/claim.<issue>` file: `<issue> <pid> <since>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimRecord {
    pub issue: i64,
    pub pid: i64,
    pub since: String,
}

/// One process from /proc.
#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: i64,
    pub ppid: i64,
    pub start_epoch: f64,
    pub argv: Vec<String>,
}

/// A live `claude -p` tick and the issue it works.
#[derive(Clone, Debug, Serialize)]
pub struct TickProcess {
    pub pid: i64,
    pub model: String,
    pub model_label: String,
    pub lane: String,
    pub age_s: i64,
    pub slot: Option<i64>,
    pub log: Option<String>,
    pub issue: Option<i64>,
    pub issue_source: Option<String>,
    pub doing: Option<String>,
    pub doing_age_s: Option<i64>,
    pub tool_shells: i64,
}

/// One device's utilisation and memory.
#[derive(Clone, Debug, Serialize)]
pub struct DeviceReading {
    pub name: String,
    pub util_pct: Option<f64>,
    pub mem_used_mb: Option<f64>,
    pub mem_total_mb: Option<f64>,
}

/// A process on a device, with the container it runs in when known.
#[derive(Clone, Debug, Serialize)]
pub struct DeviceProcess {
    pub pid: i64,
    pub used_mb: Option<f64>,
    pub name: String,
    pub container: Option<String>,
}

/// One `docker ps` row.
#[derive(Clone, Debug, Serialize)]
pub struct ContainerInfo {
    pub container_id: String,
    pub name: String,
    pub status: String,
    pub image: String,
}

/// One read of a machine: load, memory, device, device processes and containers.
#[derive(Clone, Debug, Default)]
pub struct MachineProbe {
    pub load1: Option<f64>,
    pub nproc: Option<i64>,
    pub mem_used_mb: Option<f64>,
    pub mem_total_mb: Option<f64>,
    pub gpu: Option<DeviceReading>,
    pub gpu_processes: Vec<DeviceProcess>,
    pub containers: Vec<ContainerInfo>,
}

/// One run folder `RUN_QUEUE_GLOB` matches: who holds it, what it waits for and what it runs.
#[derive(Clone, Debug)]
pub struct RunQueue {
    pub issue: Option<i64>,
    pub holder_pid: Option<i64>,
    pub alive: bool,
    pub state: String,
    pub want: Option<String>,
    pub job: Option<String>,
    pub job_host: Option<String>,
    pub job_started: Option<String>,
    pub busy_since: Option<String>,
    pub busy_reason: Option<String>,
    pub granted_host: Option<String>,
    pub updated_epoch: f64,
}

/// A process holding or waiting on a build lock, and the issue its worktree belongs to.
#[derive(Clone, Debug)]
pub struct LockParty {
    pub pid: i64,
    pub issue: Option<i64>,
    pub tree: Option<String>,
}

/// One host's compile lane: its build lock, the holder and the waiters in line.
#[derive(Clone, Debug)]
pub struct BuildLane {
    pub host: String,
    pub holder: Option<LockParty>,
    pub waiters: Vec<LockParty>,
}
