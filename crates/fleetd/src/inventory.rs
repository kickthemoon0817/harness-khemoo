//! Device identity and metrics: CPU, memory, load, disks and the GPU probe.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use fleet_proto::{DeviceInfo, DeviceName, Disk, Gpu, Memory, Metrics};
use sysinfo::{Disks, System};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::watch;

use crate::config::MetricsConfig;
use crate::{report_path, report_str, stopped, unix_now};

/// More than any probe prints for a real machine's GPUs; the rest is ignored.
const MAX_PROBE_OUTPUT: u64 = 64 * 1024;
const WARN_EVERY: Duration = Duration::from_secs(60);

pub struct Inventory {
    device: DeviceInfo,
    cfg: MetricsConfig,
    sys: Mutex<System>,
    latest: RwLock<Metrics>,
    warned_at: Mutex<Option<Instant>>,
}

impl Inventory {
    /// Takes a first sample (without GPUs), so a `status` always has one to report.
    pub fn new(name: DeviceName, cfg: MetricsConfig) -> Self {
        let mut sys = System::new();
        let latest = host_sample(&mut sys, &cfg.disks);
        let opt = |s: Option<String>| report_str(&s.unwrap_or_default());
        let device = DeviceInfo {
            name,
            hostname: opt(System::host_name()),
            os: opt(System::long_os_version().or_else(System::name)),
            kernel: opt(System::kernel_version()),
            arch: std::env::consts::ARCH.to_owned(),
            cpus: u32::try_from(sys.cpus().len()).unwrap_or(u32::MAX),
            fleetd_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        Self {
            device,
            cfg,
            sys: Mutex::new(sys),
            latest: RwLock::new(latest),
            warned_at: Mutex::new(None),
        }
    }

    pub fn device(&self) -> &DeviceInfo {
        &self.device
    }

    pub fn metrics(&self) -> Metrics {
        self.latest
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Samples every `sample_interval_s` until `shutdown` turns true.
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let mut tick = tokio::time::interval(self.cfg.sample_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = stopped(&mut shutdown) => return,
                _ = tick.tick() => self.sample().await,
            }
        }
    }

    pub async fn sample(self: &Arc<Self>) {
        let this = Arc::clone(self);
        let host = tokio::task::spawn_blocking(move || {
            let mut sys = this.sys.lock().unwrap_or_else(PoisonError::into_inner);
            host_sample(&mut sys, &this.cfg.disks)
        })
        .await;
        let gpus = match &self.cfg.gpu_probe {
            Some(argv) => self.probe(argv).await,
            None => Vec::new(),
        };
        match host {
            Ok(mut m) => {
                m.gpus = gpus;
                *self.latest.write().unwrap_or_else(PoisonError::into_inner) = m;
            }
            Err(e) => tracing::warn!(error = %e, "metrics sample failed"),
        }
    }

    async fn probe(&self, argv: &[String]) -> Vec<Gpu> {
        match run_probe(argv, self.cfg.gpu_probe_timeout).await {
            Ok(out) => {
                let mut bad = false;
                let gpus = out
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .filter_map(|l| {
                        let gpu = parse_gpu_line(l);
                        bad |= gpu.is_none();
                        gpu
                    })
                    .collect();
                if bad {
                    self.warn("the GPU probe printed a line that does not parse; skipped it");
                }
                gpus
            }
            Err(why) => {
                self.warn(&why);
                Vec::new()
            }
        }
    }

    fn warn(&self, message: &str) {
        let mut at = self
            .warned_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if at.is_none_or(|t| t.elapsed() >= WARN_EVERY) {
            *at = Some(Instant::now());
            tracing::warn!("{message}");
        }
    }
}

fn host_sample(sys: &mut System, disks: &[PathBuf]) -> Metrics {
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    let load = System::load_average();
    let mounted = Disks::new_with_refreshed_list();
    let disks = disks
        .iter()
        .filter_map(|want| mounted.list().iter().find(|d| d.mount_point() == want))
        .map(|d| Disk {
            mount: report_path(d.mount_point()),
            total_bytes: d.total_space(),
            available_bytes: d.available_space(),
        })
        .collect();
    Metrics {
        sampled_at: unix_now(),
        uptime_s: System::uptime(),
        load: [load.one, load.five, load.fifteen],
        cpu_percent: sys.global_cpu_usage().clamp(0.0, 100.0),
        memory: Memory {
            total_bytes: sys.total_memory(),
            used_bytes: sys.used_memory(),
        },
        swap: Memory {
            total_bytes: sys.total_swap(),
            used_bytes: sys.used_swap(),
        },
        disks,
        gpus: Vec::new(),
    }
}

/// Runs the probe without a shell, with only `PATH` in its environment, and returns its
/// output, or why there is none.
async fn run_probe(argv: &[String], limit: Duration) -> Result<String, String> {
    let (exe, args) = argv.split_first().ok_or("the GPU probe is empty")?;
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("the GPU probe did not start: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("the GPU probe has no stdout")?;
    let run = async {
        let mut out = Vec::new();
        (&mut stdout)
            .take(MAX_PROBE_OUTPUT)
            .read_to_end(&mut out)
            .await
            .map_err(|e| format!("reading the GPU probe: {e}"))?;
        drop(stdout);
        let status = child
            .wait()
            .await
            .map_err(|e| format!("waiting for the GPU probe: {e}"))?;
        if status.success() {
            Ok(String::from_utf8_lossy(&out).into_owned())
        } else {
            Err(format!("the GPU probe failed: {status}"))
        }
    };
    tokio::time::timeout(limit, run)
        .await
        .map_err(|_| "the GPU probe timed out and was killed".to_owned())?
}

/// One probe line: `index, name, utilization %, memory total MiB, memory used MiB,
/// temperature °C`. Empty or `[N/A]` fields are unknown; the index and name are required.
pub fn parse_gpu_line(line: &str) -> Option<Gpu> {
    const MIB: f64 = 1024.0 * 1024.0;
    let fields: Vec<&str> = line.split(',').map(str::trim).collect();
    let [index, name, util, total, used, temp] = fields.as_slice() else {
        return None;
    };
    let index = index.parse().ok()?;
    if name.is_empty() {
        return None;
    }
    // A known field that does not parse fails the line rather than reading as unknown.
    let num = |s: &str| -> Option<Option<f64>> {
        if s.is_empty() || s.eq_ignore_ascii_case("[n/a]") {
            Some(None)
        } else {
            s.parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .map(Some)
        }
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bytes = |mib: Option<f64>| mib.map(|m| (m * MIB) as u64);
    #[allow(clippy::cast_possible_truncation)]
    let small = |v: Option<f64>| v.map(|v| v as f32);
    Some(Gpu {
        index,
        name: report_str(name),
        util_percent: small(num(util)?),
        memory_total_bytes: bytes(num(total)?),
        memory_used_bytes: bytes(num(used)?),
        temperature_c: small(num(temp)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_lines_parse() {
        let gpu = parse_gpu_line("0, Example GPU, 50, 24576, 1024, 61").unwrap();
        assert_eq!(gpu.index, 0);
        assert_eq!(gpu.name, "Example GPU");
        assert_eq!(gpu.util_percent, Some(50.0));
        assert_eq!(gpu.memory_total_bytes, Some(24576 << 20));
        assert_eq!(gpu.memory_used_bytes, Some(1024 << 20));
        assert_eq!(gpu.temperature_c, Some(61.0));

        let gpu = parse_gpu_line("1, Other, [N/A], , [N/A], 40").unwrap();
        assert_eq!(gpu.util_percent, None);
        assert_eq!(gpu.memory_total_bytes, None);
        assert_eq!(gpu.memory_used_bytes, None);
        assert_eq!(gpu.temperature_c, Some(40.0));
    }

    #[test]
    fn bad_gpu_lines_are_skipped() {
        for line in [
            "",
            "garbage",
            "x, name, 1, 2, 3, 4",
            "0, , 1, 2, 3, 4",
            "0, name, lots, 2, 3, 4",
            "0, name, 1, 2, 3",
            "0, name, 1, 2, 3, 4, 5",
            "0, name, -1, 2, 3, 4",
        ] {
            assert!(parse_gpu_line(line).is_none(), "{line:?}");
        }
    }
}
