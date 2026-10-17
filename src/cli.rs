use clap::{Parser, Subcommand};

use crate::core::error::Result;
use crate::core::output::Output;
use crate::review;

/// Tools for coding agents.
#[derive(Parser)]
#[command(name = "agt", version, about)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Local PR/MR-style review threads.
    Review(review::cli::Cli),
}

pub fn run(cli: Cli) -> Result<Output> {
    match cli.command {
        Command::Review(args) => review::commands::run(args),
    }
}
