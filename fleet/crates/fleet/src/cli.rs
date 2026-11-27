//! The `fleet` command line.
//!
//! Exit codes, except `dispatch --wait`: 0 success, 1 a device or operation failed, 2 a
//! usage or config error. `dispatch --wait` exits with the agent's status instead (see
//! [`wait_exit_code`]), 125 when fleet itself failed after the config was read.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};
use fleet_proto::{
    AdapterName, DeviceName, DispatchSpec, Labels, LogLines, ModelName, Prompt, RequestBody, Role,
    SessionId, SessionState, TimeoutSecs, WireError, Workdir,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::signal::unix::{SignalKind, signal};

use crate::client::{Chunk, Client};
use crate::pki;
use crate::query::{self, Dispatched};
use crate::registry::{Device, Registry, Timeouts, config_path};
use crate::render::{self, clean};

pub const EXIT_OK: u8 = 0;
pub const EXIT_FAILED: u8 = 1;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_TIMED_OUT: u8 = 124;
pub const EXIT_FLEET_FAILED: u8 = 125;
pub const EXIT_CANCELLED: u8 = 130;

#[derive(Parser)]
#[command(
    name = "fleet",
    version,
    about = "Monitor devices and their agent sessions, and dispatch to them"
)]
struct Cli {
    /// The registry; else `$FLEET_CONFIG`, else `$HARNESS_INSTANCE_DIR/fleet.toml`.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Every device's metrics, adapters and sessions.
    Status {
        #[arg(long)]
        json: bool,
        /// Ask only this device; repeatable.
        #[arg(long = "device", value_name = "NAME")]
        devices: Vec<DeviceName>,
    },
    /// Every device's agent sessions.
    Sessions {
        #[arg(long)]
        json: bool,
        #[arg(long = "device", value_name = "NAME")]
        devices: Vec<DeviceName>,
        /// Only sessions still running.
        #[arg(long)]
        running: bool,
    },
    /// Start an agent session; the prompt comes on stdin.
    Dispatch(DispatchArgs),
    /// A session's log.
    Logs {
        device: DeviceName,
        session: SessionId,
        #[arg(long, default_value = "200")]
        lines: LogLines,
        /// Keep printing output until the session ends.
        #[arg(long)]
        follow: bool,
        #[arg(long)]
        json: bool,
    },
    /// Cancel a dispatched session.
    Cancel {
        device: DeviceName,
        session: SessionId,
        #[arg(long)]
        json: bool,
    },
    /// The private CA and the certificates of the tls transport.
    Pki {
        #[command(subcommand)]
        command: PkiCommand,
    },
}

#[derive(Args)]
struct DispatchArgs {
    /// A device name, or `auto` to pick one.
    #[arg(long, value_name = "NAME|auto")]
    device: String,
    #[arg(long)]
    adapter: AdapterName,
    /// The absolute working directory on the device.
    #[arg(long)]
    workdir: Workdir,
    #[arg(long)]
    model: Option<ModelName>,
    /// Seconds; the device applies the lesser of this and the adapter's own limit.
    #[arg(long, value_name = "S")]
    timeout: Option<TimeoutSecs>,
    /// A label for the session; repeatable.
    #[arg(long = "label", value_name = "KEY=VALUE", value_parser = parse_label)]
    labels: Vec<(String, String)>,
    /// Stream the session's output and exit with the agent's status.
    #[arg(long)]
    wait: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum PkiCommand {
    /// Make the CA: ca.pem and ca.key.
    Init {
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long, default_value_t = pki::CA_DAYS, value_parser = days())]
        days: u32,
    },
    /// Issue a device's server certificate.
    IssueServer {
        device: DeviceName,
        #[arg(long)]
        dir: Option<PathBuf>,
        /// An extra IP SAN; repeatable.
        #[arg(long = "ip", value_name = "IP")]
        ips: Vec<IpAddr>,
        /// An extra DNS SAN; repeatable.
        #[arg(long = "dns", value_name = "NAME")]
        dns: Vec<String>,
        #[arg(long, default_value_t = pki::LEAF_DAYS, value_parser = days())]
        days: u32,
    },
    /// Issue a client certificate carrying a role.
    IssueClient {
        name: DeviceName,
        #[arg(long)]
        role: Role,
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long, default_value_t = pki::LEAF_DAYS, value_parser = days())]
        days: u32,
    },
}

fn days() -> clap::builder::RangedI64ValueParser<u32> {
    clap::value_parser!(u32).range(1..=i64::from(pki::MAX_DAYS))
}

fn parse_label(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .ok_or_else(|| "expected KEY=VALUE".to_owned())
}

/// Why a command failed, which decides its exit code and its line on stderr.
enum Failure {
    /// A usage or config error: exit 2.
    Usage(String),
    /// A device or the operation failed: exit 1, or 125 under `dispatch --wait`.
    Op(Option<DeviceName>, WireError),
    /// A local operation failed: exit 1.
    Local(String),
}

impl Failure {
    fn report(&self) {
        match self {
            Self::Usage(m) | Self::Local(m) => eprintln!("fleet: {m}"),
            Self::Op(Some(d), e) => eprintln!("fleet: {d}: {}: {}", e.code, clean(&e.message)),
            Self::Op(None, e) => eprintln!("fleet: {}: {}", e.code, clean(&e.message)),
        }
    }
}

fn usage(m: impl Into<String>) -> Failure {
    Failure::Usage(m.into())
}

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fleet: runtime: {e}");
            return ExitCode::from(EXIT_FAILED);
        }
    };
    ExitCode::from(runtime.block_on(run(cli)))
}

async fn run(cli: Cli) -> u8 {
    let env = |k: &str| std::env::var_os(k);
    let config = cli.config.as_deref();
    let result = match cli.command {
        Command::Status { json, devices } => status(config, &env, json, &devices).await,
        Command::Sessions {
            json,
            devices,
            running,
        } => sessions(config, &env, json, &devices, running).await,
        Command::Dispatch(args) => return dispatch(config, &env, args).await,
        Command::Logs {
            device,
            session,
            lines,
            follow,
            json,
        } => logs(config, &env, &device, session, lines, follow, json).await,
        Command::Cancel {
            device,
            session,
            json,
        } => cancel(config, &env, &device, session, json).await,
        Command::Pki { command } => pki_command(config, &env, command),
    };
    match result {
        Ok(code) => code,
        Err(f) => {
            f.report();
            match f {
                Failure::Usage(_) => EXIT_USAGE,
                Failure::Op(..) | Failure::Local(_) => EXIT_FAILED,
            }
        }
    }
}

type Env<'a> = &'a dyn Fn(&str) -> Option<OsString>;

fn load(config: Option<&Path>, env: Env<'_>) -> Result<Registry, Failure> {
    let path = config_path(config, env).map_err(|e| usage(e.0))?;
    Registry::load(&path, env).map_err(|e| usage(e.0))
}

/// The devices named, in registry order, or all of them when none is named.
fn select<'a>(registry: &'a Registry, names: &[DeviceName]) -> Result<Vec<&'a Device>, Failure> {
    if let Some(missing) = names.iter().find(|n| registry.device(n.as_str()).is_none()) {
        return Err(usage(format!(
            "no device named {missing} in {}",
            registry.path.display()
        )));
    }
    Ok(registry
        .devices
        .iter()
        .filter(|d| names.is_empty() || names.contains(&d.name))
        .collect())
}

fn device<'a>(registry: &'a Registry, name: &str) -> Result<&'a Device, Failure> {
    registry.device(name).ok_or_else(|| {
        usage(format!(
            "no device named {name} in {}",
            registry.path.display()
        ))
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Writes to stdout, ignoring a closed pipe (`fleet status | head`) rather than panicking
/// as `print!` does.
fn out(text: &str) {
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
}

fn to_json<T: serde::Serialize>(v: &T) -> String {
    // Only fleet-proto's own types reach here, and they always serialize.
    serde_json::to_string(v).unwrap_or_else(|e| json!({ "error": e.to_string() }).to_string())
}

async fn status(
    config: Option<&Path>,
    env: Env<'_>,
    json: bool,
    names: &[DeviceName],
) -> Result<u8, Failure> {
    let registry = load(config, env)?;
    let devices = select(&registry, names)?;
    let doc = query::status(&devices, registry.timeouts, now()).await;
    if json {
        out(&format!("{}\n", to_json(&doc)));
    } else {
        out(&render::status_table(&doc));
    }
    let mut code = EXIT_OK;
    for d in &doc.devices {
        if let Some(e) = &d.error {
            Failure::Op(Some(d.name.clone()), e.clone()).report();
            code = EXIT_FAILED;
        }
    }
    Ok(code)
}

async fn sessions(
    config: Option<&Path>,
    env: Env<'_>,
    json: bool,
    names: &[DeviceName],
    running: bool,
) -> Result<u8, Failure> {
    let registry = load(config, env)?;
    let devices = select(&registry, names)?;
    let doc = query::sessions(&devices, registry.timeouts, now(), running).await;
    if json {
        out(&format!("{}\n", to_json(&doc)));
    } else {
        out(&render::sessions_table(&doc));
    }
    for e in &doc.errors {
        Failure::Op(Some(e.device.clone()), e.error.clone()).report();
    }
    Ok(if doc.errors.is_empty() {
        EXIT_OK
    } else {
        EXIT_FAILED
    })
}

/// Reads the prompt from stdin, reading at most one byte past the cap so an oversize
/// prompt is refused without being buffered whole.
async fn read_prompt() -> Result<Prompt, Failure> {
    let mut buf = Vec::new();
    tokio::io::stdin()
        .take(Prompt::MAX_LEN as u64 + 1)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| usage(format!("reading the prompt from stdin: {e}")))?;
    if buf.len() > Prompt::MAX_LEN {
        return Err(usage(format!(
            "the prompt on stdin exceeds {} bytes",
            Prompt::MAX_LEN
        )));
    }
    let text = String::from_utf8(buf).map_err(|_| usage("the prompt on stdin is not UTF-8"))?;
    if text.is_empty() {
        return Err(usage("no prompt on stdin"));
    }
    Prompt::try_from(text).map_err(|e| usage(format!("the prompt on stdin: {e}")))
}

async fn dispatch(config: Option<&Path>, env: Env<'_>, args: DispatchArgs) -> u8 {
    let failed = if args.wait {
        EXIT_FLEET_FAILED
    } else {
        EXIT_FAILED
    };
    let registry = match load(config, env) {
        Ok(r) => r,
        Err(f) => {
            f.report();
            return EXIT_USAGE;
        }
    };
    let prepared = async {
        let labels: BTreeMap<String, String> = args.labels.iter().cloned().collect();
        let labels = Labels::try_from(labels).map_err(|e| usage(format!("--label: {e}")))?;
        let target = if args.device == "auto" {
            None
        } else {
            Some(device(&registry, &args.device)?)
        };
        let spec = DispatchSpec {
            adapter: args.adapter.clone(),
            workdir: args.workdir.clone(),
            model: args.model.clone(),
            prompt: read_prompt().await?,
            timeout_s: args.timeout,
            labels,
        };
        Ok::<_, Failure>((target, spec))
    }
    .await;
    let (target, spec) = match prepared {
        Ok(p) => p,
        Err(f) => {
            f.report();
            return EXIT_USAGE;
        }
    };
    let timeouts = registry.timeouts;
    let result = if let Some(d) = target {
        query::dispatch_to(d, timeouts, spec)
            .await
            .map_err(|e| Failure::Op(Some(d.name.clone()), e))
    } else {
        let all: Vec<&Device> = registry.devices.iter().collect();
        query::dispatch_auto(&all, timeouts, &spec)
            .await
            .map_err(|(d, e)| Failure::Op(d.map(|d| d.name.clone()), e))
    };
    let dispatched = match result {
        Ok(d) => d,
        Err(f) => {
            f.report();
            return failed;
        }
    };
    let device = dispatched.device.name.clone();
    if !args.wait {
        if args.json {
            let line = json!({"device": device, "session": dispatched.session});
            out(&format!("{line}\n"));
        } else {
            out(&format!("{device} {}\n", dispatched.session));
        }
        return EXIT_OK;
    }
    eprintln!("fleet: {device}: session {}", dispatched.session);
    wait(dispatched, timeouts, args.json).await
}

/// Streams a dispatched session's output and returns the agent's exit status. SIGINT or
/// SIGTERM cancels the session, best effort, and exits 128 plus the signal.
async fn wait(dispatched: Dispatched<'_>, timeouts: Timeouts, json: bool) -> u8 {
    let Dispatched {
        device,
        session,
        mut client,
    } = dispatched;
    let fail = |e: WireError| {
        Failure::Op(Some(device.name.clone()), e).report();
        EXIT_FLEET_FAILED
    };
    let (mut int, mut term) = match (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) {
        (Ok(i), Ok(t)) => (i, t),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("fleet: installing signal handlers: {e}");
            return EXIT_FLEET_FAILED;
        }
    };
    if let Err(e) = client
        .stream(RequestBody::Wait {
            session: session.clone(),
        })
        .await
    {
        return fail(e);
    }
    let signo = tokio::select! {
        ended = pump(&mut client, &device.name, json) => {
            return match ended {
                Ok(state) => wait_exit_code(&device.name, &state),
                Err(f) => {
                    f.report();
                    EXIT_FLEET_FAILED
                }
            };
        }
        _ = int.recv() => 2,
        _ = term.recv() => 15,
    };
    let cancel = async {
        let mut c = Client::connect(device, timeouts, Role::Dispatch).await?;
        c.cancel(session.clone()).await
    };
    // A second signal abandons a cancel that hangs, as the default action would have.
    tokio::select! {
        cancelled = cancel => {
            if let Err(e) = cancelled {
                Failure::Op(Some(device.name.clone()), e).report();
            }
        }
        _ = int.recv() => {}
        _ = term.recv() => {}
    }
    128 + signo
}

/// The exit code of `dispatch --wait` for how the session ended.
pub fn wait_exit_code(device: &DeviceName, state: &SessionState) -> u8 {
    match state {
        SessionState::Exited { code } => u8::try_from(*code).unwrap_or_else(|_| {
            eprintln!("fleet: {device}: the agent exited {code}");
            EXIT_FLEET_FAILED
        }),
        SessionState::Signalled { signal } => {
            if let Ok(s @ 1..=127) = u8::try_from(*signal) {
                128 + s
            } else {
                eprintln!("fleet: {device}: the agent died of signal {signal}");
                EXIT_FLEET_FAILED
            }
        }
        SessionState::TimedOut {} => EXIT_TIMED_OUT,
        SessionState::Cancelled {} => EXIT_CANCELLED,
        SessionState::Failed { message } => {
            eprintln!("fleet: {device}: the session failed: {}", clean(message));
            EXIT_FLEET_FAILED
        }
        SessionState::Running {} => {
            eprintln!("fleet: {device}: protocol: the wait ended with the session running");
            EXIT_FLEET_FAILED
        }
    }
}

/// Copies a stream to stdout until its `end`: raw output, or with `json` one object per
/// line, `{"data": …}` for each chunk and `{"state": …}` last.
async fn pump(
    client: &mut Client,
    device: &DeviceName,
    json: bool,
) -> Result<SessionState, Failure> {
    let mut out = tokio::io::stdout();
    loop {
        let chunk = client
            .next_chunk()
            .await
            .map_err(|e| Failure::Op(Some(device.clone()), e))?;
        match chunk {
            Chunk::Data(data) => {
                let line;
                let bytes = if json {
                    line = format!("{}\n", json!({ "data": data }));
                    line.as_bytes()
                } else {
                    data.as_bytes()
                };
                write_out(&mut out, bytes).await?;
            }
            Chunk::End(state) => {
                if json {
                    write_out(
                        &mut out,
                        format!("{}\n", json!({ "state": state })).as_bytes(),
                    )
                    .await?;
                }
                return Ok(state);
            }
        }
    }
}

async fn write_out(out: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> Result<(), Failure> {
    let written = async {
        out.write_all(bytes).await?;
        out.flush().await
    };
    written
        .await
        .map_err(|e| Failure::Local(format!("writing to stdout: {e}")))
}

async fn logs(
    config: Option<&Path>,
    env: Env<'_>,
    name: &DeviceName,
    session: SessionId,
    lines: LogLines,
    follow: bool,
    json: bool,
) -> Result<u8, Failure> {
    let registry = load(config, env)?;
    let d = device(&registry, name.as_str())?;
    let op = |e| Failure::Op(Some(name.clone()), e);
    let mut client = Client::connect(d, registry.timeouts, Role::Monitor)
        .await
        .map_err(op)?;
    client
        .stream(RequestBody::Logs {
            session,
            lines,
            follow,
        })
        .await
        .map_err(op)?;
    pump(&mut client, name, json).await?;
    Ok(EXIT_OK)
}

async fn cancel(
    config: Option<&Path>,
    env: Env<'_>,
    name: &DeviceName,
    session: SessionId,
    json: bool,
) -> Result<u8, Failure> {
    let registry = load(config, env)?;
    let d = device(&registry, name.as_str())?;
    let op = |e| Failure::Op(Some(name.clone()), e);
    let mut client = Client::connect(d, registry.timeouts, Role::Dispatch)
        .await
        .map_err(op)?;
    let session = client.cancel(session).await.map_err(op)?;
    if json {
        out(&format!(
            "{}\n",
            json!({"device": name, "session": session})
        ));
    } else {
        out(&format!("{name} {session} cancelled\n"));
    }
    Ok(EXIT_OK)
}

fn pki_command(config: Option<&Path>, env: Env<'_>, command: PkiCommand) -> Result<u8, Failure> {
    let dir_or_default = |dir: Option<PathBuf>| {
        if let Some(d) = dir {
            return Ok(d);
        }
        let path = config_path(config, env).map_err(|e| usage(format!("{e}; or pass --dir")))?;
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        Ok(parent.unwrap_or(Path::new(".")).join("pki"))
    };
    let issued = match command {
        PkiCommand::Init { dir, days } => pki::init(&dir_or_default(dir)?, days),
        PkiCommand::IssueServer {
            device,
            dir,
            ips,
            dns,
            days,
        } => pki::issue_server(&dir_or_default(dir)?, &device, &dns, &ips, days),
        PkiCommand::IssueClient {
            name,
            role,
            dir,
            days,
        } => pki::issue_client(&dir_or_default(dir)?, &name, role, days),
    };
    let issued = issued.map_err(|e| Failure::Local(e.to_string()))?;
    out(&format!(
        "wrote {}\nwrote {}\nserial {}\n",
        issued.cert.display(),
        issued.key.display(),
        issued.serial
    ));
    Ok(EXIT_OK)
}
