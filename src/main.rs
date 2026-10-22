mod branch;
mod check;
mod ci;
mod cleanup;
mod cli;
mod core;
mod doctor;
mod forge;
mod git;
mod issue;
mod pr;
mod review;
mod secrets;
mod sessions;
mod store;
mod task;
mod wait;
mod worktree;

use std::process::ExitCode;

use clap::Parser;

use crate::core::output::Output;

fn main() -> ExitCode {
    match cli::run(cli::Cli::parse()) {
        Ok(Output::Exit(code)) => ExitCode::from(code),
        Ok(Output::Failure(error, code)) => failed(&error, code),
        Ok(output) => {
            let code = match &output {
                Output::Report(_, code) => *code,
                _ => 0,
            };
            match core::output::print(output) {
                Ok(()) => ExitCode::from(code),
                Err(error) => failed(&error, 1),
            }
        }
        Err(error) => failed(&error, 1),
    }
}

fn failed(error: &core::error::Error, code: u8) -> ExitCode {
    eprintln!("agt: error: {error}");
    ExitCode::from(code)
}
