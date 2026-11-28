use std::process::ExitCode;

#[derive(clap::Args)]
pub struct Args {}

pub fn run(_args: &Args) -> ExitCode {
    eprintln!("harness claimable: not ported yet");
    ExitCode::from(2)
}
