//! Assembles one JSON-ready snapshot from the collector's last results and the state files.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::collector::{Collector, Meta, Source, USAGE_THRESHOLDS, describe_io, lock};
use super::files::{read_tail, read_text, read_text_or_empty};
use super::flow::FlowInputs;
use super::models::{
    AllocatorLine, HostHealthRow, LeaseSlot, LeaseStatus, MachineProbe, TickProcess,
};
use super::parsers::{self, issue_from_path};
use super::procs::ProcessScanner;
use super::redact::redact;
use super::text::{age, clean, iso, parse_iso};

/// A whole-device hold: its host, holder and pid.
type HoldKey = (String, Option<String>, Option<i64>);

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn iso_or_null(epoch: Option<f64>) -> Value {
    epoch
        .filter(|epoch| *epoch != 0.0)
        .map_or(Value::Null, |epoch| Value::String(iso(epoch)))
}

/// Every string of a value, keys included, with control, bidi and invisible characters replaced.
pub fn clean_value(value: &mut Value) {
    match value {
        Value::String(text) => {
            if let std::borrow::Cow::Owned(cleaned) = clean(text) {
                *text = cleaned;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(clean_value),
        Value::Object(map) => {
            let entries: Vec<(String, Value)> = std::mem::take(map).into_iter().collect();
            for (key, mut item) in entries {
                clean_value(&mut item);
                map.insert(clean(&key).into_owned(), item);
            }
        }
        _ => {}
    }
}

impl Collector {
    /// Refreshes the fast sources when due and assembles the current snapshot: the JSON-ready
    /// state every panel renders from.
    pub fn snapshot(&self) -> Value {
        self.refresh_fast_sources_when_due();
        let mut snapshot = self.assemble_snapshot(self.now());
        clean_value(&mut snapshot);
        snapshot
    }

    #[allow(clippy::too_many_lines)]
    fn assemble_snapshot(&self, now: f64) -> Value {
        let state_dir = self.config.state_dir();
        let (
            lease_status,
            lease_meta,
            candidates,
            procs_read,
            github_payload,
            github_meta,
            claimable_meta,
            claimable_value,
            paused,
        ) = {
            let results = lock(&self.results);
            (
                results.lease.as_ref().and_then(|slot| slot.data.clone()),
                results.meta(Source::Lease),
                results
                    .procs
                    .as_ref()
                    .and_then(|slot| slot.data.clone())
                    .unwrap_or_default(),
                results.procs.is_some(),
                results
                    .github
                    .as_ref()
                    .and_then(|slot| slot.data.clone())
                    .unwrap_or_default(),
                results.meta(Source::Github),
                results.meta(Source::ClaimableSh),
                results.claimable_sh.as_ref().and_then(|slot| slot.data),
                results
                    .paused
                    .as_ref()
                    .and_then(|slot| slot.data.clone())
                    .unwrap_or_default(),
            )
        };
        let lease_slots: Vec<LeaseSlot> = lease_status
            .as_ref()
            .map(LeaseStatus::slots_and_device)
            .unwrap_or_default();
        let processes = self.process_table();
        let claims = self.read_claims(now);
        let github_issue_by_pid = lock(&self.github_issue_by_pid).clone();
        let ticks: Vec<TickProcess> = if procs_read {
            ProcessScanner::describe_ticks(
                &candidates,
                &processes,
                &claims,
                &lease_slots,
                &github_issue_by_pid,
                now,
            )
        } else {
            Vec::new()
        };
        let tick_pids: Vec<i64> = ticks.iter().map(|tick| tick.pid).collect();
        let tick_pid_by_issue: HashMap<i64, i64> = ticks
            .iter()
            .filter_map(|tick| Some((tick.issue?, tick.pid)))
            .collect();
        let live_claim_pid_by_issue: HashMap<i64, i64> = claims
            .iter()
            .filter(|claim| {
                processes.contains_key(&claim.pid) && !tick_pid_by_issue.contains_key(&claim.issue)
            })
            .map(|claim| (claim.issue, claim.pid))
            .collect();

        let stalls_path = state_dir.join("stalls.log");
        let allocator_lines = parsers::parse_allocator(
            &read_tail(
                &state_dir.join("logs").join("allocator.log"),
                super::collector::TAIL_BYTES,
            ),
            400,
        );
        let usage = self.usage_view(now);

        let groups = self.queue.classify(
            &github_payload.issues,
            &github_payload.dependency_states,
            &live_claim_pid_by_issue,
            &tick_pid_by_issue,
            &github_payload.first_only_issues,
        );
        let issue_titles: HashMap<i64, (String, String)> = github_payload
            .issues
            .iter()
            .map(|issue| {
                (
                    issue.number,
                    (
                        issue.title.clone().unwrap_or_default(),
                        issue.url.clone().unwrap_or_default(),
                    ),
                )
            })
            .collect();
        let claimable_count = groups.get("claimable").map_or(0, Vec::len);
        let waiting_count = claimable_count;
        let mut claimable_view = source_value_view(claimable_meta.as_ref(), claimable_value, now);
        claimable_view["unverified"] = Value::Bool(
            claimable_meta
                .as_ref()
                .and_then(|meta| meta.fetched_at)
                .is_some_and(|at| self.during_github_outage(at, now)),
        );
        let last_allocator = allocator_lines
            .iter()
            .rev()
            .find(|line| line.fields.contains_key("allowed"));
        let last_claimable_line = allocator_lines
            .iter()
            .rev()
            .find(|line| line.fields.contains_key("claimable"));
        let line_unverified = |line: &AllocatorLine| {
            parse_iso(&line.time).is_some_and(|at| self.during_github_outage(at, now))
        };
        let allocator_views: Vec<Value> = allocator_lines[allocator_lines
            .len()
            .saturating_sub(self.config.allocator_lines)..]
            .iter()
            .map(|line| {
                let mut view = to_value(line);
                view["text"] = Value::String(redact(&line.text));
                view["claimable_unverified"] =
                    Value::Bool(line.fields.contains_key("claimable") && line_unverified(line));
                view
            })
            .collect();
        let github_failing = github_meta.as_ref().is_none_or(|meta| meta.error.is_some());

        let lease_view = Self::lease_view(
            lease_status.as_ref(),
            lease_meta.as_ref(),
            &processes,
            &tick_pids,
            now,
        );
        let machines = self.machine_views(lease_status.as_ref(), waiting_count, now);
        let issue_by_tick_pid: HashMap<i64, i64> = ticks
            .iter()
            .filter_map(|tick| Some((tick.pid, tick.issue?)))
            .collect();
        let (run_queues, build_lanes) = self.activity(&issue_by_tick_pid, now);
        let status_slots: Vec<LeaseSlot> = lease_status
            .as_ref()
            .map(|status| status.slots.clone())
            .unwrap_or_default();
        let mut flow = self.flow_builder.build(&FlowInputs {
            groups: &groups,
            ticks: &ticks,
            queues: &run_queues,
            lanes: &build_lanes,
            lease_slots: &status_slots,
            closed_issues: &github_payload.closed_issues,
            machines: &machines,
            titles: &issue_titles,
            now,
        });
        self.flow_tracker.update_in_place(&mut flow, now);
        let live_standard_ticks = ticks.iter().filter(|tick| tick.lane != "landing").count();
        let last_allowed = last_allocator.and_then(|line| {
            super::text::optional_int(line.fields.get("allowed").map_or("", String::as_str))
        });
        let work_waiting = waiting_count > 0
            && last_allowed.is_some_and(|allowed| {
                i64::try_from(live_standard_ticks).unwrap_or(i64::MAX) < allowed
            })
            && !paused.paused
            && usage["allowed_band"] != "none";

        let tick_views: Vec<Value> = ticks
            .iter()
            .map(|tick| {
                let mut view = to_value(tick);
                let (title, url) = tick
                    .issue
                    .and_then(|issue| issue_titles.get(&issue).cloned())
                    .unwrap_or_default();
                view["title"] = Value::String(title);
                view["url"] = Value::String(url);
                view["lease_slots"] = Value::from(
                    lease_slots
                        .iter()
                        .filter(|lease| tick.issue.is_some() && lease.issue == tick.issue)
                        .filter_map(|lease| lease.slot)
                        .collect::<Vec<_>>(),
                );
                #[allow(clippy::cast_precision_loss)]
                let choosing =
                    tick.issue.is_none() && (tick.age_s as f64) < self.config.claim_grace_s;
                view["choosing"] = Value::Bool(choosing);
                view
            })
            .collect();
        let group_views: Map<String, Value> = groups
            .iter()
            .map(|(name, issues)| (name.clone(), to_value(issues)))
            .collect();
        let counts: Map<String, Value> = groups
            .iter()
            .map(|(name, issues)| (name.clone(), Value::from(issues.len())))
            .collect();
        let stalls = stalls_path.exists().then(|| {
            parsers::parse_stalls(
                &read_tail(&stalls_path, super::collector::TAIL_BYTES),
                self.config.stall_lines,
            )
            .iter()
            .map(|stall| {
                let mut view = to_value(stall);
                view["key"] = Value::String(redact(&stall.key));
                view["detail"] = Value::String(redact(&stall.detail));
                view
            })
            .collect::<Vec<_>>()
        });
        json!({
            "generated_at": iso(now),
            "generated_epoch": now,
            "title": self.config.title,
            "harness_home": self.config.harness_home.display().to_string(),
            "repo": self.config.repo,
            "history_hours": self.config.history_hours,
            "harness": {
                "paused": paused.paused,
                "paused_reason": paused.reason,
                "tick_models": self.read_tick_models(),
                "cooling_models": self.cooling_models(now),
                "live_ticks": ticks.len(),
                "last_allowed": last_allowed,
                "last_allowed_at": last_allocator.map(|line| line.time.clone()),
                "work_waiting": work_waiting,
                "waiting_count": waiting_count,
                "waiting_count_from_failing_read": github_failing,
                "queue_label": self.config.labels.queue,
                "lane_model": self.config.lane_model,
                "labels": to_value(&self.config.labels),
            },
            "usage": usage,
            "machines": machines,
            "leases": if self.config.resource_status_command.is_empty() { Value::Null } else { lease_view },
            "ticks": tick_views,
            "queue": {
                "groups": group_views,
                "counts": counts,
                "claimable_count": claimable_count,
                "claimable_sh": claimable_view,
                "allocator_claimable": last_claimable_line.map(|line| json!({
                    "value": line.fields.get("claimable"),
                    "at": line.time,
                    "unverified": line_unverified(line),
                })),
                "github_outages": self.github_outages().iter().map(|(start, end)| json!({"from": iso(*start), "to": end.map(iso)})).collect::<Vec<_>>(),
                "fetched_at": iso_or_null(github_meta.as_ref().and_then(|meta| meta.fetched_at)),
                "age_s": age(github_meta.as_ref().and_then(|meta| meta.fetched_at), now),
                "error": github_meta.as_ref().map_or_else(|| Some("not fetched yet".to_owned()), |meta| meta.error.clone()),
            },
            "landings": github_payload.landings,
            "health": {"stalls": stalls, "allocator": allocator_views},
            "fleet": if self.config.fleet_status_command.is_empty() { Value::Null } else { self.fleet_view(&issue_titles, now) },
            "flow": flow,
            "sources": self.sources_view(now),
        })
    }

    fn fleet_view(&self, issue_titles: &HashMap<i64, (String, String)>, now: f64) -> Value {
        let (report, meta) = {
            let results = lock(&self.results);
            (
                results.fleet.as_ref().and_then(|slot| slot.data.clone()),
                results.meta(Source::Fleet),
            )
        };
        let redact_option = |text: Option<&str>| text.map(redact);
        let devices = report
            .as_ref()
            .and_then(|report| report.devices.as_ref())
            .map(|devices| {
                devices
                    .iter()
                    .map(|device| {
                        let mut view = to_value(device);
                        view["name"] = Value::String(redact(&device.name));
                        view["error"] = to_value(&redact_option(device.error.as_deref()));
                        view["hostname"] = Value::String(redact(&device.hostname));
                        view["os"] = Value::String(redact(&device.os));
                        view["gpus"] = Value::Array(
                            device
                                .gpus
                                .iter()
                                .map(|gpu| {
                                    let mut gpu_view = to_value(gpu);
                                    gpu_view["name"] = Value::String(redact(&gpu.name));
                                    gpu_view
                                })
                                .collect(),
                        );
                        view["adapters"] = Value::Array(
                            device
                                .adapters
                                .iter()
                                .map(|adapter| {
                                    let mut adapter_view = to_value(adapter);
                                    adapter_view["name"] = Value::String(redact(&adapter.name));
                                    adapter_view["models"] = to_value(
                                        &adapter
                                            .models
                                            .iter()
                                            .map(|model| redact(model))
                                            .collect::<Vec<_>>(),
                                    );
                                    adapter_view
                                })
                                .collect(),
                        );
                        view
                    })
                    .collect::<Vec<_>>()
            });
        let sessions: Vec<Value> = report
            .iter()
            .flat_map(|report| report.sessions.iter())
            .map(|session| {
                let mut issue = session.labels.iter().find(|(key, _)| key == "issue").and_then(|(_, value)| value.parse::<i64>().ok());
                let mut issue_source = if issue.is_some() { "label" } else { "" };
                if issue.is_none()
                    && let Some(cwd) = session.cwd.as_deref().filter(|cwd| !cwd.is_empty())
                {
                    issue = issue_from_path(cwd, &self.config.worktree_root);
                    issue_source = if issue.is_some() { "worktree" } else { "" };
                }
                #[allow(clippy::cast_precision_loss)]
                let ended = session.ended_at.map_or(now, |ended| ended as f64);
                #[allow(clippy::cast_precision_loss)]
                let started = session.started_at as f64;
                let (title, url) = issue.and_then(|issue| issue_titles.get(&issue).cloned()).unwrap_or_default();
                let labels: Map<String, Value> =
                    session.labels.iter().map(|(key, value)| (redact(key), Value::String(redact(value)))).collect();
                json!({
                    "device": redact(&session.device), "adapter": redact(&session.adapter), "id": redact(&session.id),
                    "origin": session.origin, "state": redact(&session.state), "running": session.running,
                    "pid": session.pid, "model": redact_option(session.model.as_deref()), "summary": redact(&session.summary),
                    "cwd": redact_option(session.cwd.as_deref()), "age_s": age(Some(started), ended),
                    "issue": issue, "issue_source": issue_source, "title": title, "url": url, "labels": labels,
                })
            })
            .collect();
        json!({
            "available": report.is_some(),
            "error": meta.as_ref().map_or_else(|| Some("not read yet".to_owned()), |meta| meta.error.clone()),
            "fetched_at": iso_or_null(meta.as_ref().and_then(|meta| meta.fetched_at)),
            "age_s": age(meta.as_ref().and_then(|meta| meta.fetched_at), now),
            "devices": devices,
            "sessions": sessions,
            "errors": report.as_ref().map(|report| report.errors.iter().map(|error| redact(error)).collect::<Vec<_>>()).unwrap_or_default(),
            "truncated": report.as_ref().map(|report| report.truncated.iter().map(|name| redact(name)).collect::<Vec<_>>()).unwrap_or_default(),
        })
    }

    fn probe_fields(probe: Option<&MachineProbe>) -> Map<String, Value> {
        let view = match probe {
            None => {
                json!({"load1": null, "nproc": null, "mem_used_mb": null, "mem_total_mb": null, "gpu": null,
                           "gpu_processes": [], "containers": []})
            }
            Some(probe) => json!({
                "load1": probe.load1, "nproc": probe.nproc, "mem_used_mb": probe.mem_used_mb, "mem_total_mb": probe.mem_total_mb,
                "gpu": probe.gpu.as_ref().map(to_value), "gpu_processes": to_value(&probe.gpu_processes),
                "containers": to_value(&probe.containers),
            }),
        };
        match view {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }

    /// One view a machine: this host first, then each remote host.
    #[allow(clippy::too_many_lines)]
    fn machine_views(
        &self,
        lease_status: Option<&LeaseStatus>,
        waiting_count: usize,
        now: f64,
    ) -> Vec<Value> {
        let window_start = now - self.config.history_hours * 3600.0;
        let all_rows = self.read_host_health();
        let streaks = parsers::parse_streaks(&read_text_or_empty(
            &self.config.state_dir().join("host_health.streaks"),
        ));
        let lease_waits = lease_status.map_or(0, |status| status.waits.len());
        let slots: &[LeaseSlot] = lease_status.map_or(&[], |status| status.slots.as_slice());
        let mut machines = Vec::new();

        let (local_probe, local_meta, remote_probes) = {
            let results = lock(&self.results);
            (
                results.local.as_ref().and_then(|slot| slot.data.clone()),
                results.meta(Source::Local),
                results
                    .remote
                    .as_ref()
                    .and_then(|slot| slot.data.clone())
                    .unwrap_or_default(),
            )
        };
        let local_rows: Vec<[Option<f64>; 8]> = lock(&self.history)
            .iter()
            .filter(|row| row[0].is_some_and(|epoch| epoch >= window_start))
            .copied()
            .collect();
        let mut local = Self::probe_fields(local_probe.as_ref());
        let card_note = if lease_status
            .is_some_and(|status| status.local_card_off || !status.harness_only_hosts.is_empty())
        {
            "harness off (left to its owner)"
        } else {
            ""
        };
        let slot_views = |host: &str| -> Vec<Value> {
            slots
                .iter()
                .filter(|slot| slot.host == host && slot.state != "free")
                .map(to_value)
                .collect()
        };
        let slot_count = |host: &str| slots.iter().filter(|slot| slot.host == host).count();
        let ok =
            local_meta.as_ref().is_some_and(|meta| meta.error.is_none()) && local_probe.is_some();
        let local_fields = json!({
            "key": "local",
            "name": hostname(),
            "kind": "local",
            "status": if ok { "ok" } else { "unknown" },
            "upload_mb_s": null,
            "connect_s": null,
            "streak": null,
            "card_note": card_note,
            "slots": slot_views("local"),
            "slot_count": slot_count("local"),
            "history": {
                "gpu_util": local_rows.iter().filter_map(|row| Some(json!([row[0], row[5]?]))).collect::<Vec<_>>(),
                "load_per_core": local_rows.iter().filter_map(|row| {
                    let (load, cores) = (row[1]?, row[2].filter(|cores| *cores != 0.0)?);
                    Some(json!([row[0], load / cores]))
                }).collect::<Vec<_>>(),
            },
            "sampled_at": iso_or_null(local_meta.as_ref().and_then(|meta| meta.fetched_at)),
            "age_s": age(local_meta.as_ref().and_then(|meta| meta.fetched_at), now),
            "probe_error": local_meta.as_ref().and_then(|meta| meta.error.clone()),
        });
        if let Value::Object(fields) = local_fields {
            local.extend(fields);
        }
        let mut local = Value::Object(local);
        local["signals"] =
            machine_signals(&local, waiting_count, lease_waits, card_note.is_empty());
        machines.push(local);

        for host in self.remote_hosts() {
            let host_rows: Vec<&HostHealthRow> =
                all_rows.iter().filter(|row| row.host == host).collect();
            let latest = host_rows.last().copied();
            let probe_slot = remote_probes.get(&host);
            let probe = probe_slot.and_then(|slot| slot.data.as_ref());
            let mut view = Self::probe_fields(probe);
            if let Some(latest) = latest
                && probe_slot
                    .and_then(|slot| slot.fetched_at)
                    .is_none_or(|fetched| latest.epoch >= fetched)
            {
                view.insert("load1".to_owned(), json!(latest.load1));
                view.insert("nproc".to_owned(), json!(latest.nproc));
                match view.get_mut("gpu") {
                    Some(gpu @ Value::Object(_)) => {
                        gpu["util_pct"] = json!(latest.gpu_util_pct);
                        gpu["mem_used_mb"] = json!(latest.gpu_mem_used_mb);
                    }
                    _ => {
                        view.insert(
                            "gpu".to_owned(),
                            json!({"name": "", "util_pct": latest.gpu_util_pct, "mem_used_mb": latest.gpu_mem_used_mb, "mem_total_mb": null}),
                        );
                    }
                }
            }
            if let (Some(latest), Some(probe)) = (latest, probe)
                && probe.nproc.is_none()
            {
                view.insert("load1".to_owned(), json!(latest.load1));
                view.insert("nproc".to_owned(), json!(latest.nproc));
            }
            let window_rows: Vec<&HostHealthRow> = host_rows
                .iter()
                .copied()
                .filter(|row| row.epoch >= window_start)
                .collect();
            let preferred_rank = lease_status
                .and_then(|status| {
                    status
                        .harness_only_hosts
                        .iter()
                        .position(|preferred| *preferred == host)
                })
                .map(|rank| rank + 1);
            #[allow(clippy::cast_precision_loss)]
            let fields = json!({
                "key": host,
                "name": host,
                "kind": "remote",
                "status": latest.map_or("unknown", |latest| latest.status.as_str()),
                "upload_mb_s": latest.and_then(|latest| latest.upload_mb_s),
                "upload_timed_out": latest.is_some_and(|latest| latest.upload_timed_out),
                "connect_s": latest.and_then(|latest| latest.connect_s),
                "probed_at": latest.map(|latest| latest.time.clone()),
                "probe_age_s": age(latest.map(|latest| latest.epoch), now),
                "probe_stale": latest.is_none_or(|latest| now - latest.epoch > self.config.host_health_stale_s),
                "streak": streaks.get(&host).map(to_value),
                "preferred_rank": preferred_rank,
                "card_note": "",
                "slots": slot_views(&host),
                "slot_count": slot_count(&host),
                "history": {
                    "gpu_util": window_rows.iter().filter_map(|row| Some(json!([row.epoch, row.gpu_util_pct?]))).collect::<Vec<_>>(),
                    "load_per_core": window_rows.iter().filter_map(|row| {
                        let (load, cores) = (row.load1?, row.nproc.filter(|cores| *cores != 0)?);
                        Some(json!([row.epoch, load / cores as f64]))
                    }).collect::<Vec<_>>(),
                    "upload": window_rows.iter().filter_map(|row| Some(json!([row.epoch, row.upload_mb_s?]))).collect::<Vec<_>>(),
                    "down": window_rows.iter().filter(|row| row.status == "down").map(|row| row.epoch).collect::<Vec<_>>(),
                },
                "sampled_at": iso_or_null(probe_slot.and_then(|slot| slot.fetched_at)),
                "age_s": age(probe_slot.and_then(|slot| slot.fetched_at), now),
                "probe_error": probe_slot.and_then(|slot| slot.error.clone()),
            });
            if let Value::Object(fields) = fields {
                view.extend(fields);
            }
            let mut view = Value::Object(view);
            view["signals"] = machine_signals(&view, waiting_count, lease_waits, true);
            machines.push(view);
        }
        machines
    }

    /// The slots-and-leases panel's data: remote hosts, slots with holder kinds and ages,
    /// whole-device holds, device lease, priority and waits.
    fn lease_view(
        lease_status: Option<&LeaseStatus>,
        meta: Option<&Meta>,
        processes: &super::procs::ProcessTable,
        tick_pids: &[i64],
        now: f64,
    ) -> Value {
        let Some(status) = lease_status else {
            return json!({
                "available": false, "error": meta.map_or_else(|| Some("not read yet".to_owned()), |meta| meta.error.clone()),
                "slots": [], "whole_card_holds": [], "waits": [], "remote_cards": [], "operator_priority": [],
                "harness_only_hosts": [], "local_card_off": false, "device": null, "unparsed": [],
            });
        };
        let slot_view = |slot: &LeaseSlot| {
            let mut view = to_value(slot);
            view["holder"] = to_value(&slot.holder.as_deref().map(|holder| {
                if holder.is_empty() {
                    String::new()
                } else {
                    redact(holder)
                }
            }));
            view["holder_kind"] = Value::String(ProcessScanner::describe_holder(
                slot.pid, processes, tick_pids,
            ));
            view["held_s"] = to_value(&age(
                slot.since
                    .as_deref()
                    .filter(|since| !since.is_empty())
                    .and_then(parse_iso),
                now,
            ));
            view
        };
        let mut holds: Vec<(HoldKey, Value)> = Vec::new();
        for slot in &status.slots {
            if slot.state == "free"
                || !slot.tags.iter().any(|tag| {
                    [
                        "remote-all",
                        "held-for-all",
                        "exclusive",
                        "reserved-for-all",
                    ]
                    .contains(&tag.as_str())
                })
            {
                continue;
            }
            let key = (slot.host.clone(), slot.holder.clone(), slot.pid);
            let index = holds
                .iter()
                .position(|(existing, _)| *existing == key)
                .unwrap_or_else(|| {
                    let mut hold = slot_view(slot);
                    hold["slots"] = json!([]);
                    holds.push((key, hold));
                    holds.len() - 1
                });
            let hold = &mut holds[index].1;
            if let Some(list) = hold["slots"].as_array_mut() {
                list.push(json!(slot.slot));
            }
            hold["reserving"] = Value::Bool(slot.tags.iter().any(|tag| tag == "reserved-for-all"));
        }
        json!({
            "available": true,
            "error": meta.and_then(|meta| meta.error.clone()),
            "age_s": age(meta.and_then(|meta| meta.fetched_at), now),
            "remote_cards": to_value(&status.remote_cards),
            "harness_only_hosts": status.harness_only_hosts,
            "local_card_off": status.local_card_off,
            "slots": status.slots.iter().map(slot_view).collect::<Vec<_>>(),
            "whole_card_holds": holds.into_iter().map(|(_, hold)| hold).collect::<Vec<_>>(),
            "device": status.device.as_ref().map(slot_view),
            "operator_priority": status.operator_priority,
            "waits": to_value(&status.waits),
            "unparsed": status.unparsed_lines.iter().map(|line| redact(line)).collect::<Vec<_>>(),
        })
    }

    /// The usage cache placed against the allocator's cut points: percentages, reset times, age,
    /// staleness and each window's band (full, half, one, none).
    fn usage_view(&self, now: f64) -> Value {
        let thresholds = USAGE_THRESHOLDS;
        let path = &self.config.usage_cache;
        let reading = read_text(path)
            .map_err(|error| describe_io(&error, path))
            .and_then(|text| parsers::parse_usage_cache(&text));
        let reading = match reading {
            Ok(reading) => reading,
            Err(error) => {
                return json!({"available": false, "error": error, "thresholds": to_value(&thresholds), "stale": true});
            }
        };
        let age_s = age(reading.taken_at_epoch, now);
        #[allow(clippy::cast_precision_loss)]
        let stale = age_s.is_none_or(|age| age as f64 > self.config.usage_max_age_s);
        #[allow(clippy::cast_precision_loss)]
        let band = |percent: Option<f64>, half: i64, one: i64, none: i64| {
            percent.map(|percent| {
                if percent >= thresholds.ceiling_pct as f64 || percent >= none as f64 {
                    "none"
                } else if percent >= one as f64 {
                    "one"
                } else if percent >= half as f64 {
                    "half"
                } else {
                    "full"
                }
            })
        };
        let five = band(
            reading.five_hour_pct,
            thresholds.five_half_at,
            thresholds.five_one_at,
            thresholds.five_none_at,
        );
        let week = band(
            reading.weekly_pct,
            thresholds.week_half_at,
            thresholds.week_one_at,
            thresholds.week_none_at,
        );
        let order = ["full", "half", "one", "none"];
        let rank = |band: &str| order.iter().position(|known| *known == band).unwrap_or(0);
        let allowed = if stale {
            None
        } else {
            [five, week]
                .into_iter()
                .flatten()
                .max_by_key(|band| rank(band))
        };
        let mut view = to_value(&reading);
        for (key, value) in [
            ("available", json!(true)),
            ("error", Value::Null),
            ("age_s", json!(age_s)),
            ("stale", json!(stale)),
            ("five_hour_band", json!(five)),
            ("weekly_band", json!(week)),
            ("allowed_band", json!(allowed.unwrap_or("none"))),
            ("thresholds", to_value(&thresholds)),
        ] {
            view[key] = value;
        }
        view
    }

    /// Every source's age, last error and cadence, in the page's order.
    pub fn sources_view(&self, now: f64) -> Value {
        let config = &self.config;
        let mut intervals = vec![
            (Source::Procs, config.fast_ttl_s),
            (Source::Local, config.local_interval_s),
            (Source::Paused, config.local_interval_s),
            (Source::Remote, config.remote_interval_s),
            (Source::Github, config.github_interval_s),
            (Source::ClaimableSh, config.claimable_interval_s),
        ];
        if !config.resource_status_command.is_empty() {
            intervals.push((Source::Lease, config.fast_ttl_s));
        }
        if !config.run_queue_glob.is_empty() || config.build_locks_enabled() {
            intervals.push((Source::Activity, config.activity_ttl_s));
        }
        if !config.fleet_status_command.is_empty() {
            intervals.push((Source::Fleet, config.fleet_interval_s));
        }
        let results = lock(&self.results);
        let mut view = Map::new();
        for (source, interval) in intervals {
            let meta = results.meta(source);
            let meta = meta.as_ref();
            view.insert(
                source.name().to_owned(),
                json!({
                    "interval_s": interval,
                    "fetched_at": iso_or_null(meta.and_then(|meta| meta.fetched_at)),
                    "age_s": age(meta.and_then(|meta| meta.fetched_at), now),
                    "error": meta.and_then(|meta| meta.error.clone()),
                    "duration_s": meta.and_then(|meta| meta.duration_s).map(|duration| (duration * 100.0).round() / 100.0),
                }),
            );
        }
        drop(results);
        if let Ok(modified) = std::fs::metadata(config.state_dir().join("host_health.tsv"))
            .and_then(|metadata| metadata.modified())
        {
            let modified = modified
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |since| since.as_secs_f64());
            view.insert(
                "host_health_tsv".to_owned(),
                json!({"interval_s": 180, "fetched_at": iso(modified), "age_s": age(Some(modified), now), "error": null, "duration_s": null}),
            );
        }
        Value::Object(view)
    }
}

/// The order of the sources on the page; the snapshot's map is sorted by name.
pub const SOURCE_ORDER: [&str; 10] = [
    "procs",
    "local",
    "paused",
    "remote",
    "github",
    "claimable_sh",
    "lease",
    "activity",
    "fleet",
    "host_health_tsv",
];

fn source_value_view(meta: Option<&Meta>, value: Option<i64>, now: f64) -> Value {
    json!({
        "value": value,
        "fetched_at": iso_or_null(meta.and_then(|meta| meta.fetched_at)),
        "age_s": age(meta.and_then(|meta| meta.fetched_at), now),
        "error": meta.and_then(|meta| meta.error.clone()),
    })
}

/// A machine's notable conditions, each a level (critical, serious, warning, info) and text.
fn machine_signals(
    view: &Value,
    waiting_count: usize,
    lease_waits: usize,
    harness_uses_card: bool,
) -> Value {
    let mut signals = Vec::new();
    let streak = &view["streak"];
    if streak["state"] == "demoted" {
        signals.push(json!({"level": "serious", "text": format!("moved last ({} bad probes)", streak["bad_probes"])}));
    }
    if view["probe_stale"] == true && view["kind"] == "remote" {
        signals.push(json!({"level": "warning", "text": "host probe stale"}));
    }
    if view["probe_error"]
        .as_str()
        .is_some_and(|error| !error.is_empty())
    {
        signals.push(json!({"level": "info", "text": "live read failed"}));
    }
    let gpu_util = view["gpu"]["util_pct"].as_f64();
    let last_three = |series: &str| -> Vec<f64> {
        let points = view["history"][series]
            .as_array()
            .cloned()
            .unwrap_or_default();
        points[points.len().saturating_sub(3)..]
            .iter()
            .filter_map(|point| point[1].as_f64())
            .collect()
    };
    let recent_util = last_three("gpu_util");
    let recent_load = last_three("load_per_core");
    let card_idle = gpu_util == Some(0.0) && recent_util.iter().all(|value| *value == 0.0);
    let cpu_idle = !recent_load.is_empty() && recent_load.iter().all(|value| *value < 0.25);
    let holds = view["slots"]
        .as_array()
        .is_some_and(|slots| !slots.is_empty());
    if harness_uses_card && card_idle && !holds && (waiting_count > 0 || lease_waits > 0) {
        signals.push(json!({"level": "warning", "text": "idle, work waiting"}));
    } else if harness_uses_card && card_idle && cpu_idle && holds && recent_util.len() >= 3 {
        signals.push(json!({"level": "warning", "text": "held, device and CPU idle"}));
    }
    Value::Array(signals)
}

fn hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}
