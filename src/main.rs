mod anchors;
mod cli;
mod clock;
mod commands;
mod error;
mod forge;
mod git;
mod github;
mod gitlab;
mod model;
mod store;

use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use clap::Parser;

use crate::commands::Output;
use crate::error::{Result, ctx};

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    match commands::run(cli).and_then(print) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agent-review: {error}");
            ExitCode::FAILURE
        }
    }
}

fn print(output: Output) -> Result<()> {
    let text = match output {
        // Pretty for a person at a terminal; compact for agents and scripts, which pay per byte.
        Output::Json(value) => {
            let mut text = if std::io::stdout().is_terminal() {
                serde_json::to_string_pretty(&value)
            } else {
                serde_json::to_string(&value)
            }
            .map_err(ctx("serialize output"))?;
            text.push('\n');
            text
        }
        Output::Text(text) => text,
    };
    std::io::stdout()
        .lock()
        .write_all(text.as_bytes())
        .map_err(ctx("write stdout"))
}
