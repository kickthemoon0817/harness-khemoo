//! `harness dashboard`: serves the harness status dashboard on 127.0.0.1: machines, live ticks,
//! the issue queue, landings and health signals, plus the instance's resource slots, run queues,
//! build lanes and fleet's devices and agent sessions when it configures them, as an
//! auto-refreshing page (`/`) and as JSON (`/api/state`).
//!
//! The config file is `$HARNESS_ENV`, else `$HARNESS_INSTANCE_DIR/harness.env` (default `<harness
//! home>/instance`). From it the dashboard reads only [`ENV_KEYS`]; `GH_TOKEN` reaches `gh` through
//! its environment and is never shown. A panel whose key is unset is left off the page.
//! `HARNESS_STATE`, `USAGE_CACHE` and `WORKTREE_ROOT` in the environment win over the file's. An empty
//! `GH_REPO` is derived from `TARGET_REPO`'s origin by lib/instance.sh, as the scripts derive it.
//! Otherwise the file is never evaluated: a command key may name `$HARNESS_HOME`,
//! `$HARNESS_INSTANCE_DIR` and `$HARNESS_BIN`, a path key `$HOME` too, and no other `$` text is
//! expanded. bin/dashboard.sh starts this detached.

mod buildlocks;
mod collector;
mod command;
mod env;
mod files;
mod fleet;
mod flow;
mod flowtrack;
mod github;
mod models;
mod parsers;
mod procs;
mod queue;
mod redact;
mod render;
mod runqueue;
mod server;
mod shlex;
mod snapshot;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod tests;
mod text;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use collector::{Collector, Config, wall_clock};
use command::Environment;
use github::GitHubClient;
use queue::QueueLabels;
use server::Server;

/// harness.env's label keys and the label each sets.
const LABEL_KEYS: [(&str, &str); 8] = [
    ("ISSUE_LABEL", "queue"),
    ("WIP_LABEL", "wip"),
    ("SIGNOFF_LABEL", "signoff"),
    ("OPERATOR_LABEL", "operator"),
    ("READY_LABEL", "ready"),
    ("LANE_LABEL", "lane"),
    ("FIRST_LABEL", "first"),
    ("LOW_LABEL", "low"),
];

/// The only keys read from harness.env.
pub const ENV_KEYS: [&str; 29] = [
    "GH_TOKEN",
    "GH_REPO",
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
    "ISSUE_LABEL",
    "WIP_LABEL",
    "SIGNOFF_LABEL",
    "OPERATOR_LABEL",
    "READY_LABEL",
    "LANE_LABEL",
    "FIRST_LABEL",
    "LOW_LABEL",
];

const URL_FILE_NAME: &str = "dashboard.url";

/// A repeated option takes its last value: bin/dashboard.sh passes its own `--port` before the
/// caller's options.
#[derive(clap::Args, Debug, Clone)]
#[command(args_override_self = true)]
pub struct Args {
    /// Harness checkout to read (default: `$HARNESS_HOME`, else this binary's checkout).
    #[arg(long)]
    harness_home: Option<PathBuf>,
    /// Port on 127.0.0.1; 0 picks a free one (default: `$DASHBOARD_PORT`, else 8787).
    #[arg(long)]
    port: Option<u16>,
    /// The dashboard's own history and cache (default: `$DASHBOARD_DATA_DIR`, else <state>/dashboard).
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// owner/name; default `GH_REPO` from harness.env.
    #[arg(long)]
    repo: Option<String>,
    /// The label the queue count counts (default: `ISSUE_LABEL` from harness.env, else ai).
    #[arg(long)]
    queue_label: Option<String>,
    /// Label of every issue the queue panels list.
    #[arg(long, default_value = "ai")]
    all_label: String,
    /// Usage reading (default: `$USAGE_CACHE`, else `USAGE_CACHE` from harness.env, else ~/.claude/usage-cache.json).
    #[arg(long)]
    usage_cache: Option<PathBuf>,
    /// gh executable.
    #[arg(long, default_value = "gh")]
    gh: String,
    /// Snapshot and page refresh interval, seconds.
    #[arg(long, default_value_t = 30)]
    refresh: u64,
    /// Sparkline window, hours.
    #[arg(long, default_value_t = 6.0)]
    history_hours: f64,
    /// How often to read GitHub, seconds.
    #[arg(long, default_value_t = 60.0)]
    github_interval: f64,
    /// How often to sample this host, seconds.
    #[arg(long, default_value_t = 30.0)]
    local_interval: f64,
    /// Read-only ssh probe of each remote host, seconds; 0 turns it off.
    #[arg(long, default_value_t = 120.0)]
    remote_interval: f64,
    /// How often to run `QUEUE_DEPTH_CMD`, else `harness claimable`, seconds; 0 turns it off.
    #[arg(long, default_value_t = 300.0)]
    claimable_interval: f64,
    /// How often to run `FLEET_STATUS_CMD`, when it is set, seconds.
    #[arg(long, default_value_t = 30.0)]
    fleet_interval: f64,
    /// How long a closed issue stays as done, hours.
    #[arg(long, default_value_t = 6.0)]
    done_hours: f64,
    /// Print one JSON snapshot and exit.
    #[arg(long)]
    once: bool,
    /// Print the data directory this start would use and exit.
    #[arg(long)]
    print_data_dir: bool,
}

/// The process environment, a variable that is unset or empty reading as none, as the scripts'
/// `${NAME:-…}` reads it. The config is built from such a lookup, so it can be built from another.
pub type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

pub fn env_nonempty(name: &str) -> Option<String> {
    crate::home::env_nonempty(name).and_then(|value| value.into_string().ok())
}

/// A path made absolute, symlinks resolved as far as it exists.
fn resolve(path: &Path) -> PathBuf {
    if let Ok(resolved) = path.canonicalize() {
        return resolved;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) if parent != absolute => resolve(parent).join(name),
        _ => absolute,
    }
}

fn expand_user(path: &str, lookup: Lookup<'_>) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            PathBuf::from(format!("{}{rest}", lookup("HOME").unwrap_or_default()))
        }
        _ => PathBuf::from(path),
    }
}

/// `$HARNESS_INSTANCE_DIR`, else `<harness home>/instance`.
fn instance_dir(harness_home: &Path, lookup: Lookup<'_>) -> PathBuf {
    lookup("HARNESS_INSTANCE_DIR").map_or_else(|| harness_home.join("instance"), PathBuf::from)
}

/// `$HARNESS_ENV`, else harness.env in the instance directory.
pub fn env_path(harness_home: &Path, lookup: Lookup<'_>) -> PathBuf {
    lookup("HARNESS_ENV").map_or_else(
        || instance_dir(harness_home, lookup).join("harness.env"),
        PathBuf::from,
    )
}

/// The checkout this binary was built in: the parent of the `target` directory above it.
fn checkout_of_binary() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    exe.ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == "target"))?
        .parent()
        .map(Path::to_path_buf)
}

/// The harness binary as lib/instance.sh resolves it: harness.env's `HARNESS_BIN`, else the
/// environment's, else this checkout's release build when it is executable, else `harness` from
/// PATH.
fn harness_bin(harness_home: &Path, from_file: Option<&str>, lookup: Lookup<'_>) -> String {
    if let Some(bin) = from_file.filter(|bin| !bin.is_empty()) {
        return bin.to_owned();
    }
    if let Some(bin) = lookup("HARNESS_BIN") {
        return bin;
    }
    let release = harness_home.join("target").join("release").join("harness");
    let executable = std::fs::metadata(&release)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0);
    if executable {
        release.display().to_string()
    } else {
        "harness".to_owned()
    }
}

/// `GH_REPO` from lib/instance.sh, so an empty one is derived from `TARGET_REPO`'s origin exactly as
/// the launcher derives it; empty when none is derived or the loader fails. The library is
/// sourced by bash, the one place the dashboard runs a shell, with a fixed script.
fn derived_repo(harness_home: &Path, lookup: Lookup<'_>) -> String {
    let Some(library) = checkout_of_binary()
        .map(|checkout| checkout.join("lib").join("instance.sh"))
        .filter(|library| library.is_file())
    else {
        eprintln!(
            "harness dashboard: no checkout with lib/instance.sh holds this binary; GH_REPO is not derived"
        );
        return String::new();
    };
    let argv = [
        "bash".to_owned(),
        "-c".to_owned(),
        r#". "$1"; harness_load >/dev/null 2>&1; printf %s "${GH_REPO:-}""#.to_owned(),
        "_".to_owned(),
        library.display().to_string(),
    ];
    let decisive = [
        "HARNESS_HOME",
        "HARNESS_ENV",
        "HARNESS_INSTANCE_DIR",
        "GH_REPO",
        "HARNESS_BIN",
    ];
    let mut variables: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| !decisive.contains(&name.as_str()))
        .collect();
    variables.extend(
        decisive[1..]
            .iter()
            .filter_map(|name| Some(((*name).to_owned(), lookup(name)?))),
    );
    variables.push((
        "HARNESS_HOME".to_owned(),
        harness_home.display().to_string(),
    ));
    let environment = Environment::Exactly(variables);
    match command::run(&argv, environment, Duration::from_secs(10), 1024 * 1024) {
        Ok(output) if output.success() => output.stdout.trim().to_owned(),
        _ => String::new(),
    }
}

/// `<user@host>=<name> ...`, each remote host's name in `BUILD_LOCK_REMOTE`; a word without `=` is
/// left out.
fn parse_build_lock_names(text: &str) -> Vec<(String, String)> {
    text.split_whitespace()
        .filter_map(|word| word.split_once('='))
        .filter(|(host, name)| !host.is_empty() && !name.is_empty())
        .map(|(host, name)| (host.to_owned(), name.to_owned()))
        .collect()
}

/// The options and the harness.env keys, read into the collector's config and the GitHub token.
pub struct App {
    pub config: Config,
    pub github_token: String,
}

impl App {
    /// Every value no response may carry: harness.env's GitHub token and the environment's.
    pub fn secret_values(&self) -> Vec<String> {
        std::iter::once(self.github_token.clone())
            .chain(github::environment_secrets())
            .filter(|secret| !secret.is_empty())
            .collect()
    }

    pub fn new(args: &Args, lookup: Lookup<'_>) -> Result<Self, String> {
        let harness_home = args
            .harness_home
            .clone()
            .or_else(|| lookup("HARNESS_HOME").map(PathBuf::from))
            .or_else(crate::home::harness_home)
            .ok_or("no harness home: pass --harness-home or set HARNESS_HOME")?;
        let harness_home = resolve(&harness_home);
        let instance = instance_dir(&harness_home, lookup);
        let values: HashMap<String, String> =
            env::read_keys(&env_path(&harness_home, lookup), &ENV_KEYS);
        let value = |key: &str| values.get(key).map_or("", String::as_str);
        let mut command_placeholders = vec![
            ("HARNESS_HOME", harness_home.display().to_string()),
            ("HARNESS_INSTANCE_DIR", instance.display().to_string()),
        ];
        let mut bin_placeholders = command_placeholders.clone();
        bin_placeholders.push(("HOME", lookup("HOME").unwrap_or_default()));
        let bin_text = env::expand(value("HARNESS_BIN"), &bin_placeholders);
        let bin_from_file = if bin_text.is_empty() {
            bin_text
        } else {
            expand_user(&bin_text, lookup).display().to_string()
        };
        let bin = harness_bin(&harness_home, Some(&bin_from_file), lookup);
        let fleet_base = command_placeholders.clone();
        command_placeholders.push(("HARNESS_BIN", bin.clone()));
        let mut path_placeholders = command_placeholders.clone();
        path_placeholders.push(("HOME", lookup("HOME").unwrap_or_default()));
        let path_setting =
            |key: &str| lookup(key).unwrap_or_else(|| env::expand(value(key), &path_placeholders));
        let command_setting = |key: &str| -> Result<Vec<String>, String> {
            let words = shlex::split(value(key))
                .map_err(|error| format!("{key} does not parse: {error}"))?;
            Ok(words
                .iter()
                .map(|word| env::expand(word, &command_placeholders))
                .collect())
        };
        let state_setting = path_setting("HARNESS_STATE");
        let harness_state = if state_setting.is_empty() {
            resolve(&harness_home.join("state"))
        } else {
            resolve(&expand_user(&state_setting, lookup))
        };
        let github_token = value("GH_TOKEN").to_owned();
        let repo = args
            .repo
            .clone()
            .filter(|repo| !repo.is_empty())
            .or_else(|| Some(value("GH_REPO").to_owned()).filter(|repo| !repo.is_empty()))
            .or_else(|| lookup("GH_REPO"))
            .or_else(|| Some(derived_repo(&harness_home, lookup)).filter(|repo| !repo.is_empty()));
        let usage_cache = args.usage_cache.clone().unwrap_or_else(|| {
            let setting = path_setting("USAGE_CACHE");
            expand_user(
                if setting.is_empty() {
                    "~/.claude/usage-cache.json"
                } else {
                    &setting
                },
                lookup,
            )
        });
        // What a tick's fleet finds in its environment, so FLEET_STATUS_CMD reads the same fleet.toml.
        let mut fleet_environment: Vec<(String, String)> = fleet_base
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect();
        let fleet_config = path_setting("FLEET_CONFIG");
        if !fleet_config.is_empty() {
            fleet_environment.push(("FLEET_CONFIG".to_owned(), fleet_config));
        }
        let mut labels = QueueLabels {
            all_issues: args.all_label.clone(),
            ..QueueLabels::default()
        };
        for (key, field) in LABEL_KEYS {
            let label = value(key);
            if label.is_empty() {
                continue;
            }
            let slot = match field {
                "queue" => &mut labels.queue,
                "wip" => &mut labels.wip,
                "signoff" => &mut labels.signoff,
                "operator" => &mut labels.operator,
                "ready" => &mut labels.ready,
                "lane" => &mut labels.lane,
                "first" => &mut labels.first,
                _ => &mut labels.low,
            };
            label.clone_into(slot);
        }
        if let Some(queue_label) = args.queue_label.clone().filter(|label| !label.is_empty()) {
            labels.queue = queue_label;
        }
        let data_dir = args
            .data_dir
            .clone()
            .or_else(|| lookup("DASHBOARD_DATA_DIR").map(PathBuf::from))
            .map_or_else(|| harness_state.join("dashboard"), |dir| resolve(&dir));
        let mut config = Config::new(harness_home, data_dir);
        config.harness_state = Some(harness_state);
        config.repo = repo;
        config.harness_bin = bin;
        config.title = if value("DASHBOARD_TITLE").is_empty() {
            "Harness status".to_owned()
        } else {
            value("DASHBOARD_TITLE").to_owned()
        };
        value("LANE_MODEL").clone_into(&mut config.lane_model);
        config.labels = labels;
        config.usage_cache = usage_cache;
        config.history_hours = args.history_hours;
        config.github_interval_s = args.github_interval;
        config.local_interval_s = args.local_interval;
        config.remote_interval_s = args.remote_interval;
        config.claimable_interval_s = args.claimable_interval;
        config.fleet_interval_s = args.fleet_interval;
        config.resource_status_command = command_setting("RESOURCE_STATUS_CMD")?;
        config.device_probe_command = command_setting("DEVICE_PROBE_CMD")?;
        config.queue_depth_command = command_setting("QUEUE_DEPTH_CMD")?;
        config.fleet_status_command = command_setting("FLEET_STATUS_CMD")?;
        config.fleet_environment = fleet_environment;
        config.worktree_root = path_setting("WORKTREE_ROOT");
        value("RUN_QUEUE_GLOB").clone_into(&mut config.run_queue_glob);
        config.run_queue_log = if value("RUN_QUEUE_LOG").is_empty() {
            "queue.log".to_owned()
        } else {
            value("RUN_QUEUE_LOG").to_owned()
        };
        value("RUN_QUEUE_HOLDER").clone_into(&mut config.run_queue_holder);
        config.run_queue_stop_words = value("RUN_QUEUE_STOP_WORDS")
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        config.build_lock_dir = PathBuf::from(if value("BUILD_LOCK_DIR").is_empty() {
            "/tmp"
        } else {
            value("BUILD_LOCK_DIR")
        });
        value("BUILD_LOCK_LOCAL").clone_into(&mut config.build_lock_local);
        value("BUILD_LOCK_REMOTE").clone_into(&mut config.build_lock_remote);
        config.build_lock_names = parse_build_lock_names(value("BUILD_LOCK_NAMES"));
        config.done_window_s = args.done_hours * 3600.0;
        Ok(Self {
            config,
            github_token,
        })
    }

    pub fn collector(&self, gh: &str) -> Result<Collector, String> {
        let github = match &self.config.repo {
            Some(repo) => Some(GitHubClient::new(repo, Some(&self.github_token), gh)?),
            None => None,
        };
        Ok(Collector::new(
            self.config.clone(),
            github,
            Box::new(wall_clock),
        ))
    }
}

/// Runs the dashboard: prints the data directory, one snapshot, or serves until SIGINT or SIGTERM.
pub fn run(args: &Args) -> ExitCode {
    match run_app(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("harness dashboard: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run_app(args: &Args) -> Result<(), String> {
    let app = App::new(args, &env_nonempty)?;
    if args.print_data_dir {
        println!("{}", app.config.data_dir.display());
        return Ok(());
    }
    let collector = app.collector(&args.gh)?;
    if args.once {
        files::prepare_private_dir(&collector.config.data_dir)?;
        collector.refresh_local_sample_in_place();
        if collector.config.remote_interval_s > 0.0 {
            collector.refresh_remote_probes_in_place();
        }
        collector.refresh_github_in_place();
        if collector.config.claimable_interval_s > 0.0 {
            collector.refresh_claimable_in_place();
        }
        if !collector.config.fleet_status_command.is_empty() {
            collector.refresh_fleet_in_place();
        }
        let mut body = pretty(&collector.snapshot());
        for secret in app.secret_values() {
            body = body.replace(&secret, "[redacted]");
        }
        println!("{body}");
        return Ok(());
    }
    let port = match args.port {
        Some(port) => port,
        None => env_nonempty("DASHBOARD_PORT").map_or(Ok(8787), |port| {
            port.parse()
                .map_err(|_| format!("DASHBOARD_PORT {port:?} is not a port"))
        })?,
    };
    serve(
        Arc::new(collector),
        port,
        args.refresh,
        &app.secret_values(),
    )
}

/// A listener on 127.0.0.1 only: the dashboard is never reachable from another host.
pub fn bind_loopback(port: u16) -> std::io::Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// The snapshot as `--once` prints it: one space an indent level.
fn pretty(value: &serde_json::Value) -> String {
    use serde::Serialize as _;
    let mut output = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(
        &mut output,
        serde_json::ser::PrettyFormatter::with_indent(b" "),
    );
    if value.serialize(&mut serializer).is_err() {
        return value.to_string();
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn serve(
    collector: Arc<Collector>,
    port: u16,
    refresh_s: u64,
    secret_values: &[String],
) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async move {
        let listener = bind_loopback(port)
            .and_then(tokio::net::TcpListener::from_std)
            .map_err(|error| format!("cannot listen on 127.0.0.1:{port}: {error}"))?;
        let bound = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        let token = server::new_access_token()
            .map_err(|error| format!("no random access token: {error}"))?;
        let server = Arc::new(Server::new(
            Arc::clone(&collector),
            refresh_s,
            secret_values,
            token,
            bound,
        ));
        let publisher = Arc::clone(&server);
        #[allow(clippy::cast_precision_loss)]
        let interval = server.refresh_s as f64;
        collector.start(
            Some(Box::new(move || publisher.publish_in_place())),
            interval,
        )?;
        let url_path = collector.config.data_dir.join(URL_FILE_NAME);
        files::write_private(&url_path, &format!("{}\n", server.url()))
            .map_err(|error| format!("{}: {error}", url_path.display()))?;
        let config = &collector.config;
        eprintln!(
            "harness dashboard on {} (home {}, data {}, repo {}, queue label {})",
            server.url().replace(&server.access_token, "[token]"),
            config.harness_home.display(),
            config.data_dir.display(),
            config.repo.as_deref().unwrap_or("unset"),
            config.labels.queue
        );
        let shutdown = async {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            let terminated = async {
                match terminate.as_mut() {
                    Some(signal) => {
                        signal.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                () = terminated => {}
            }
        };
        Arc::clone(&server).serve(listener, shutdown).await;
        collector.stop();
        std::fs::remove_file(&url_path).ok();
        Ok(())
    })
}
