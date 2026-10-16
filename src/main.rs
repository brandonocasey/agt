use clap::Parser;

/// Local PR/MR-style review threads for coding agents.
#[derive(Parser)]
#[command(version, about)]
struct Cli {}

fn main() {
    Cli::parse();
}
