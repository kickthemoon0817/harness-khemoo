//! Choosing a device for `--device auto`.

use fleet_proto::{AdapterName, ErrorCode, ModelName, StatusReport};

/// Ranks the candidates for a dispatch. `probes` holds each `auto` device's status in
/// registry order, `None` for one that did not answer. The result is the indices of the
/// devices that have the adapter with free capacity and allow `model`, least loaded per
/// CPU first, ties in registry order.
///
/// With none eligible, the error says why: `unreachable` when no device answered,
/// `not_found` when none that answered offers the adapter and model, `busy` when all
/// that do are full.
pub fn rank(
    probes: &[Option<&StatusReport>],
    adapter: &AdapterName,
    model: Option<&ModelName>,
) -> Result<Vec<usize>, ErrorCode> {
    let mut offered = false;
    let mut eligible: Vec<(f64, usize)> = Vec::new();
    for (i, report) in probes.iter().enumerate() {
        let Some(report) = report else { continue };
        let Some(info) = report.adapters.iter().find(|a| &a.name == adapter) else {
            continue;
        };
        if info.max_concurrent == 0 || model.is_some_and(|m| !info.models.contains(m)) {
            continue;
        }
        offered = true;
        if info.running < info.max_concurrent {
            let cpus = f64::from(report.device.cpus.max(1));
            eligible.push((report.metrics.load[0] / cpus, i));
        }
    }
    if eligible.is_empty() {
        return Err(if probes.iter().all(Option::is_none) {
            ErrorCode::Unreachable
        } else if offered {
            ErrorCode::Busy
        } else {
            ErrorCode::NotFound
        });
    }
    // Stable, so equal loads keep registry order.
    eligible.sort_by(|a, b| a.0.total_cmp(&b.0));
    Ok(eligible.into_iter().map(|(_, i)| i).collect())
}

#[cfg(test)]
mod tests {
    use fleet_proto::{AdapterInfo, DeviceInfo, Memory, Metrics};

    use super::*;

    fn report(cpus: u32, load: f64, adapters: &[(&str, &[&str], u32, u32)]) -> StatusReport {
        StatusReport {
            device: DeviceInfo {
                name: "dev-a".parse().unwrap(),
                hostname: "dev-a.example.com".into(),
                os: "Linux".into(),
                kernel: "6.8.0".into(),
                arch: "x86_64".into(),
                cpus,
                fleetd_version: "0.1.0".into(),
            },
            metrics: Metrics {
                sampled_at: 0,
                uptime_s: 0,
                load: [load, 0.0, 0.0],
                cpu_percent: 0.0,
                memory: Memory {
                    total_bytes: 0,
                    used_bytes: 0,
                },
                swap: Memory {
                    total_bytes: 0,
                    used_bytes: 0,
                },
                disks: vec![],
                gpus: vec![],
            },
            adapters: adapters
                .iter()
                .map(|(name, models, max, running)| AdapterInfo {
                    name: name.parse().unwrap(),
                    models: models.iter().map(|m| m.parse().unwrap()).collect(),
                    max_concurrent: *max,
                    running: *running,
                })
                .collect(),
        }
    }

    fn claude() -> AdapterName {
        "claude".parse().unwrap()
    }

    #[test]
    fn lowest_load_per_cpu_first_ties_in_registry_order() {
        let busy_big = report(16, 8.0, &[("claude", &["sonnet"], 2, 0)]); // 0.5
        let idle_small = report(4, 0.4, &[("claude", &["sonnet"], 1, 0)]); // 0.1
        let tie = report(8, 0.8, &[("claude", &["sonnet"], 1, 0)]); // 0.1
        let probes = [Some(&busy_big), Some(&idle_small), Some(&tie)];
        assert_eq!(rank(&probes, &claude(), None), Ok(vec![1, 2, 0]));
    }

    #[test]
    fn skips_full_missing_discovery_only_and_wrong_model() {
        let full = report(4, 0.0, &[("claude", &["sonnet"], 1, 1)]);
        let other = report(4, 0.0, &[("codex", &["o3"], 1, 0)]);
        let discovery = report(4, 0.0, &[("claude", &[], 0, 0)]);
        let no_opus = report(4, 0.0, &[("claude", &["sonnet"], 1, 0)]);
        let opus = report(4, 3.0, &[("claude", &["sonnet", "opus"], 1, 0)]);
        let probes = [
            Some(&full),
            Some(&other),
            Some(&discovery),
            None,
            Some(&no_opus),
            Some(&opus),
        ];
        let opus_model: ModelName = "opus".parse().unwrap();
        assert_eq!(rank(&probes, &claude(), Some(&opus_model)), Ok(vec![5]));
        assert_eq!(rank(&probes, &claude(), None), Ok(vec![4, 5]));
    }

    #[test]
    fn zero_cpus_does_not_divide_by_zero() {
        let odd = report(0, 1.0, &[("claude", &[], 1, 0)]);
        let probes = [Some(&odd)];
        assert_eq!(rank(&probes, &claude(), None), Ok(vec![0]));
    }

    #[test]
    fn says_why_none_is_eligible() {
        let full = report(4, 0.0, &[("claude", &["sonnet"], 1, 1)]);
        let other = report(4, 0.0, &[("codex", &["o3"], 1, 0)]);
        assert_eq!(
            rank(&[None, None], &claude(), None),
            Err(ErrorCode::Unreachable)
        );
        assert_eq!(rank(&[], &claude(), None), Err(ErrorCode::Unreachable));
        assert_eq!(
            rank(&[Some(&other), None], &claude(), None),
            Err(ErrorCode::NotFound)
        );
        assert_eq!(
            rank(&[Some(&full), Some(&other)], &claude(), None),
            Err(ErrorCode::Busy)
        );
        let haiku: ModelName = "haiku".parse().unwrap();
        assert_eq!(
            rank(&[Some(&full)], &claude(), Some(&haiku)),
            Err(ErrorCode::NotFound)
        );
    }
}
