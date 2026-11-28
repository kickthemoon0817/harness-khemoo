use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "harness", version, about = "The harness's own tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Lint what a tick is about to post to GitHub against prompts/public-record.md.
    Lint(harness::lint::Args),
    /// Print how many queued issues a tick could claim now.
    Claimable(harness::claimable::Args),
    /// Serve the status dashboard on 127.0.0.1.
    Dashboard(harness::dashboard::Args),
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Lint(args) => harness::lint::run(&args),
        Command::Claimable(args) => harness::claimable::run(&args),
        Command::Dashboard(args) => harness::dashboard::run(&args),
    }
}
