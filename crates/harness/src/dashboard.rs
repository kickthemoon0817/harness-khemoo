use std::process::ExitCode;

#[derive(clap::Args)]
pub struct Args {}

pub fn run(_args: &Args) -> ExitCode {
    eprintln!("harness dashboard: not ported yet");
    ExitCode::from(2)
}
