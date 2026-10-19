//! `agt worktree`: create a worktree on the newest default base, install its dependencies, give
//! it a stable dev-server port, list linked worktrees, remove one after a snapshot backup, and
//! restore a backup.

mod backup;
mod cleanup;
mod create;
mod disposable;
mod fast_forward;
mod gate;
mod idle;
mod info;
mod landed;
mod list;
mod nested;
mod port;
mod prepare;
pub mod prune;
mod remove;
mod restore;
pub(crate) mod roots;
mod snapshot;

use clap::{Args, Subcommand};
use serde_json::Value;

use crate::core::error::Result;
use crate::core::output::{self, Output};

#[derive(Args)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a worktree for a branch under `.worktrees/`, on the newest default base.
    Create(create::Cli),
    /// Install a worktree's dependencies from its lockfiles.
    Prepare(prepare::Cli),
    /// A stable dev-server TCP port for a worktree.
    Port(port::Cli),
    /// Linked worktrees with their dirty counts, ahead/behind and merged state.
    List(list::Cli),
    /// Back up a worktree, then remove it; the branch stays. Without `--yes` only a plan.
    Remove(remove::Cli),
    /// Remove worktrees and delete branches whose work landed in the default branch. Without
    /// `--yes` only a plan.
    Cleanup(cleanup::Cli),
    /// Recreate a removed worktree and its uncommitted files from a backup.
    Restore(restore::Cli),
    /// List, show or delete worktree backups.
    Backups(backup::Cli),
}

pub fn run(cli: Cli) -> Result<Output> {
    match cli.command {
        Command::Create(args) => create::run(args),
        Command::Prepare(args) => prepare::run(args),
        Command::Port(args) => port::run(args),
        Command::List(args) => list::run(args),
        Command::Remove(args) => remove::run(args),
        Command::Cleanup(args) => cleanup::run(args),
        Command::Restore(args) => restore::run(args),
        Command::Backups(args) => backup::run(args),
    }
}

/// Prints `value`; a failed step still prints its report but exits 1.
fn emit(value: Value, ok: bool) -> Result<Output> {
    if ok {
        return Ok(Output::Json(value));
    }
    output::print(Output::Json(value))?;
    Ok(Output::Exit(1))
}
