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
    /// List the live ticks of this harness state directory.
    Ticks(harness::ticks::Args),
    /// Print the usage windows of each agent, and the ticks they allow.
    Usage(harness::usage::Args),
    /// Print each agent account's sign-in and usage, or the folder a tick on one runs with.
    Accounts(harness::accounts::Args),
    /// Print one bounded, redacted snapshot of the harness for the supervisor.
    Health(harness::health::Args),
    /// Apply, show or clear the supervisor's directive.
    Directive(harness::directive::Args),
    /// Decide when the supervisor runs, and record its runs.
    Supervisor(harness::supervisor::Args),
    /// Pick an operator's comments out of an issue's.
    Operator(harness::operator::Args),
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Lint(args) => harness::lint::run(&args),
        Command::Claimable(args) => harness::claimable::run(&args),
        Command::Dashboard(args) => harness::dashboard::run(&args),
        Command::Ticks(args) => harness::ticks::run(&args),
        Command::Usage(args) => harness::usage::run(&args),
        Command::Accounts(args) => harness::accounts::run(&args),
        Command::Health(args) => harness::health::run(&args),
        Command::Directive(args) => harness::directive::run(&args),
        Command::Supervisor(args) => harness::supervisor::run(&args),
        Command::Operator(args) => harness::operator::run(&args),
    }
}
