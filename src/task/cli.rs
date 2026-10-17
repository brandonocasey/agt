use clap::Args;

use crate::core::error::Result;
use crate::core::output::Output;
use crate::task::args::Options;
use crate::task::{records, sessions};

const USAGE: &str = "usage: agt task <command> [options]\n\ncommands:\n  create   create a durable task record\n  list     list task records\n  show     show one task record\n  update   update one task record\n  check    add a completed check\n  index    update the private session index and show coverage\n  resolve  resolve an exact session ID to its source\n  search   search indexed human turns with bounded context\n";

/// Durable task records and session search.
///
/// Arguments are parsed by hand so they stay identical to the `agent-task` tool this replaces:
/// run `agt task --help` for the commands.
#[derive(Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct Cli {
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "ARGS"
    )]
    args: Vec<String>,
}

pub fn run(cli: Cli) -> Result<Output> {
    let (command, options) = Options::parse(&cli.args)?;
    match command.as_deref() {
        Some("create") => records::create(&options),
        Some("list") => records::list(),
        Some("show") => records::show(&options),
        Some("update") => records::update(&options),
        Some("check") => records::check(&options),
        Some("index") => sessions::index_command(&options),
        Some("resolve") => sessions::resolve(&options),
        Some("search") => sessions::search(&options),
        None | Some("help" | "--help") => Ok(Output::Text(USAGE.to_owned())),
        Some(other) => crate::core::error::fail(format!("unknown command: {other}")),
    }
}
