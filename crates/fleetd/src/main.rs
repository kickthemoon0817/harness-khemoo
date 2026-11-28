#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use fleet_proto::Role;
use fleetd::config::{self, Config};
use tokio::signal::unix::{SignalKind, signal};

/// The fleet device daemon: inventory, agent-session discovery and sandboxed dispatch.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(
        long,
        value_name = "PATH",
        help = "fleetd.toml; else $FLEETD_CONFIG, else the XDG config location"
    )]
    config: Option<PathBuf>,
    #[arg(long, value_name = "LEVEL", default_value = "info",
          value_parser = ["error", "warn", "info", "debug"])]
    log_level: String,
    /// Relay stdin and stdout to the running daemon: the ssh transport.
    #[arg(long, conflicts_with = "check")]
    stdio: bool,
    #[arg(
        long,
        requires = "stdio",
        value_name = "ROLE",
        help = "With --stdio, the most this relay asks for; it can only lower [stdio] max_role"
    )]
    role: Option<Role>,
    /// Validate the config and print the resolved settings.
    #[arg(long)]
    check: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_max_level(cli.log_level.parse().unwrap_or(tracing::Level::INFO))
        .init();

    let loaded = config::resolve_path(cli.config).and_then(|path| {
        let cfg = Config::load(&path)?;
        // The relay needs only the socket; the TLS material is the daemon's.
        let tls = if cli.stdio {
            None
        } else {
            fleetd::tls::load(&cfg)?
        };
        Ok((cfg, tls))
    });
    let (cfg, tls) = match loaded {
        Ok(ok) => ok,
        Err(e) => {
            eprintln!("fleetd: {e}");
            if cli.stdio
                && let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
            {
                let stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
                rt.block_on(fleetd::relay::refuse_client(
                    stdio,
                    fleet_proto::ErrorCode::Internal,
                    "fleetd.toml on the device does not load; run fleetd --check there",
                ));
                std::process::exit(2);
            }
            return ExitCode::from(2);
        }
    };
    if cli.check {
        println!("{cfg:#?}");
        return ExitCode::SUCCESS;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fleetd: {e}");
            return ExitCode::FAILURE;
        }
    };
    if cli.stdio {
        let cap = cli.role.unwrap_or(Role::Dispatch).min(cfg.stdio_max_role);
        let stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
        let code = runtime.block_on(fleetd::relay::run(stdio, &cfg.unix.socket, cap));
        // Exit at once: the runtime would otherwise wait on the thread blocked reading stdin.
        std::process::exit(code);
    }
    match runtime.block_on(daemon(cfg, tls)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fleetd: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn daemon(
    cfg: Config,
    tls: Option<std::sync::Arc<rustls::ServerConfig>>,
) -> anyhow::Result<()> {
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let running = fleetd::server::start(cfg, tls).await?;
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    tracing::info!("shutting down");
    running.shutdown().await;
    Ok(())
}
