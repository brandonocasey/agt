mod cli;
mod core;
mod forge;
mod git;
mod review;
mod store;
mod task;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    match cli::run(cli::Cli::parse()).and_then(core::output::print) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agt: error: {error}");
            ExitCode::FAILURE
        }
    }
}
