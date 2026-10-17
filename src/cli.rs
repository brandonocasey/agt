use clap::{Parser, Subcommand};

use crate::ci;
use crate::cleanup;
use crate::core::error::Result;
use crate::core::output::Output;
use crate::issue;
use crate::review;
use crate::sessions;
use crate::task;
use crate::wait;
use crate::watch;

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
    Review(Box<review::cli::Cli>),
    /// Durable task records and session search.
    Task(task::cli::Cli),
    /// Poll PR/MR checks until they finish, fail or change.
    Watch(watch::Cli),
    /// GitLab CI triage: failing specs versus the default branch, and waiting for a pipeline.
    Ci(ci::Cli),
    /// GitHub and GitLab issues, synced locally; writes wait for `push`.
    Issue(issue::Cli),
    /// Delete stale reviews, issues, tasks, locks and backups across every repository.
    Cleanup(cleanup::Cli),
    /// Cost, context, tool, cache, error and friction reports from agent transcripts.
    Sessions(sessions::Cli),
    /// Rerun a check until it succeeds or `--max` seconds pass (exit 3).
    Wait(wait::Cli),
}

pub fn run(cli: Cli) -> Result<Output> {
    match cli.command {
        Command::Review(args) => review::commands::run(*args),
        Command::Task(args) => task::cli::run(args),
        Command::Watch(args) => watch::run(args),
        Command::Ci(args) => ci::run(args),
        Command::Issue(args) => issue::run(args),
        Command::Cleanup(args) => cleanup::run(args),
        Command::Sessions(args) => sessions::run(args),
        Command::Wait(args) => wait::run(args),
    }
}
