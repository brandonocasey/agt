mod ci;
mod cli;
mod core;
mod forge;
mod git;
mod issue;
mod review;
mod store;
mod task;
mod watch;

use std::process::ExitCode;

use clap::Parser;

use crate::core::output::Output;

fn main() -> ExitCode {
    match cli::run(cli::Cli::parse()) {
        Ok(Output::Exit(code)) => ExitCode::from(code),
        Ok(output) => match core::output::print(output) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => failed(&error),
        },
        Err(error) => failed(&error),
    }
}

fn failed(error: &core::error::Error) -> ExitCode {
    eprintln!("agt: error: {error}");
    ExitCode::FAILURE
}
