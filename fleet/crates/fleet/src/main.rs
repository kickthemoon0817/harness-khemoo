#![forbid(unsafe_code)]

use clap::Parser;

#[derive(Parser)]
#[command(version, about)]
struct Cli {}

fn main() {
    Cli::parse();
}
