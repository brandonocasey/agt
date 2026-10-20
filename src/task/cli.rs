use clap::Args;

use crate::core::error::Result;
use crate::core::output::Output;
use crate::task::args::Command;
use crate::task::records;

/// Durable task records.
#[derive(Args)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

pub fn run(cli: Cli) -> Result<Output> {
    match cli.command {
        Command::Create(args) => records::create(&args),
        Command::List => records::list(),
        Command::Show(target) => records::show(&target),
        Command::Update(args) => records::update(&args),
    }
}
