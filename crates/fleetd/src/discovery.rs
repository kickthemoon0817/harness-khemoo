//! Agent sessions found among the device's processes, by adapter `match_exe`.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::sync::{Mutex, PoisonError};

use fleet_proto::{AdapterName, Labels, ModelName, Origin, Session, SessionState};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::config::Config;
use crate::{own_uid, report_path, report_str};

const SUMMARY_MAX_FLAGS: usize = 8;
const SUMMARY_MAX_LEN: usize = 120;

pub struct Discovery {
    sys: Mutex<System>,
    adapters: Vec<(AdapterName, Vec<String>)>,
    all_users: bool,
}

impl Discovery {
    pub fn new(cfg: &Config) -> Self {
        Self {
            sys: Mutex::new(System::new()),
            adapters: cfg
                .adapters
                .iter()
                .map(|a| (a.name.clone(), a.match_exe.clone()))
                .collect(),
            all_users: cfg.all_users,
        }
    }

    /// The agent processes running now, except those in `skip_groups`: the process groups
    /// of fleetd's own dispatched sessions, which are reported as those sessions instead.
    /// Blocking; it reads the process table.
    pub fn scan(&self, skip_groups: &HashSet<i32>) -> Vec<Session> {
        let mut sys = self.sys.lock().unwrap_or_else(PoisonError::into_inner);
        // A cheap pass over every process for its name, exe and owner, then argv and cwd for
        // the few that match an adapter.
        sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_exe(UpdateKind::OnlyIfNotSet)
                .with_user(UpdateKind::OnlyIfNotSet),
        );
        let me = std::process::id();
        let uid = own_uid();
        let matched: Vec<(Pid, usize)> = sys
            .processes()
            .iter()
            .filter(|(pid, p)| {
                pid.as_u32() != me
                    && p.thread_kind().is_none()
                    && (self.all_users || p.user_id().is_some_and(|u| **u == uid))
            })
            .filter_map(|(pid, p)| {
                let exe_name = p.exe().and_then(|e| e.file_name());
                let adapter = self.adapters.iter().position(|(_, names)| {
                    names
                        .iter()
                        .any(|n| p.name() == n.as_str() || exe_name == Some(OsStr::new(n)))
                })?;
                Some((*pid, adapter))
            })
            .filter(|(pid, _)| !in_groups(*pid, skip_groups))
            .collect();
        let pids: Vec<Pid> = matched.iter().map(|(pid, _)| *pid).collect();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            false,
            ProcessRefreshKind::nothing()
                .with_cmd(UpdateKind::Always)
                .with_cwd(UpdateKind::Always),
        );
        matched
            .into_iter()
            .filter_map(|(pid, adapter)| {
                let p = sys.process(pid)?;
                let base = p
                    .exe()
                    .and_then(|e| e.file_name())
                    .unwrap_or_else(|| p.name());
                let args = p.cmd().iter().skip(1).map(std::ffi::OsString::as_os_str);
                let id = format!("p-{}-{}", pid.as_u32(), p.start_time())
                    .parse()
                    .ok()?;
                Some(Session {
                    id,
                    adapter: self.adapters[adapter].0.clone(),
                    origin: Origin::Discovered,
                    state: SessionState::Running {},
                    pid: Some(pid.as_u32()),
                    started_at: p.start_time(),
                    ended_at: None,
                    cwd: p.cwd().map(report_path),
                    exe: p.exe().map(report_path),
                    model: model_flag(args.clone()),
                    summary: summarize(base, args),
                    labels: Labels::default(),
                })
            })
            .collect()
    }
}

fn in_groups(pid: Pid, groups: &HashSet<i32>) -> bool {
    if groups.is_empty() {
        return false;
    }
    i32::try_from(pid.as_u32())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .and_then(|p| rustix::process::getpgid(Some(p)).ok())
        .is_some_and(|g| groups.contains(&g.as_raw_nonzero().get()))
}

/// The executable's base name followed by the flag names in `args` (elements starting with
/// `-`, cut at any `=`), at most 8 flags and 120 bytes. Values and positional arguments are
/// left out: they can hold prompts or secrets.
pub fn summarize<'a>(base: &OsStr, args: impl Iterator<Item = &'a OsStr>) -> String {
    let mut out = report_str(&base.to_string_lossy());
    while out.len() > SUMMARY_MAX_LEN {
        out.pop();
    }
    let flags = args
        .map(OsStr::to_string_lossy)
        .filter(|a| a.starts_with('-'))
        .take(SUMMARY_MAX_FLAGS);
    for flag in flags {
        let name = report_str(flag.split('=').next().unwrap_or_default());
        if out.len() + 1 + name.len() > SUMMARY_MAX_LEN {
            break;
        }
        out.push(' ');
        out.push_str(&name);
    }
    out
}

/// The value of a `--model` flag (`--model M` or `--model=M`), if it is a valid model name.
fn model_flag<'a>(mut args: impl Iterator<Item = &'a OsStr>) -> Option<ModelName> {
    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else { continue };
        let value = if arg == "--model" {
            args.next()?.to_str()?
        } else if let Some(v) = arg.strip_prefix("--model=") {
            v
        } else {
            continue;
        };
        return value.parse().ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&'static str]) -> Vec<&'static OsStr> {
        v.iter().map(|s| OsStr::new(*s)).collect()
    }

    #[test]
    fn summary_keeps_flag_names_only() {
        let args = os(&[
            "-p",
            "fix the bug with key sk-secret",
            "--model",
            "opus",
            "--api-key=sk-secret",
        ]);
        assert_eq!(
            summarize(OsStr::new("claude"), args.into_iter()),
            "claude -p --model --api-key"
        );
    }

    #[test]
    fn summary_is_bounded() {
        let args = os(&["-a", "-b", "-c", "-d", "-e", "-f", "-g", "-h", "-i", "-j"]);
        assert_eq!(
            summarize(OsStr::new("x"), args.into_iter()),
            "x -a -b -c -d -e -f -g -h"
        );
        let long = "-".to_owned() + &"y".repeat(200);
        let s = summarize(OsStr::new("x"), std::iter::once(OsStr::new(long.as_str())));
        assert_eq!(s, "x");
        let s = summarize(OsStr::new(&"é".repeat(100)), std::iter::empty());
        assert!(s.len() <= SUMMARY_MAX_LEN);
    }

    #[test]
    fn model_flag_forms() {
        let m = |v: &[&'static str]| model_flag(os(v).into_iter()).map(ModelName::into_string);
        assert_eq!(m(&["-p", "--model", "opus"]).as_deref(), Some("opus"));
        assert_eq!(m(&["--model=sonnet"]).as_deref(), Some("sonnet"));
        assert_eq!(m(&["--model", "-rf"]), None);
        assert_eq!(m(&["--model"]), None);
        assert_eq!(m(&["opus"]), None);
    }
}
