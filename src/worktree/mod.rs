//! `agt worktree`: list linked worktrees, remove one after a snapshot backup, restore a backup.

mod backup;
mod disposable;
mod info;
mod list;
mod remove;
mod restore;
mod roots;
mod snapshot;
mod submodules;

use clap::{Args, Subcommand};

use crate::core::error::Result;
use crate::core::output::Output;

#[derive(Args)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Linked worktrees with their dirty counts, ahead/behind and merged state.
    List(list::Cli),
    /// Back up a worktree, then remove it; the branch stays. Without `--yes` only a plan.
    Remove(remove::Cli),
    /// Recreate a removed worktree and its uncommitted files from a backup.
    Restore(restore::Cli),
    /// List, show or delete worktree backups.
    Backups(backup::Cli),
}

pub fn run(cli: Cli) -> Result<Output> {
    match cli.command {
        Command::List(args) => list::run(args),
        Command::Remove(args) => remove::run(args),
        Command::Restore(args) => restore::run(args),
        Command::Backups(args) => backup::run(args),
    }
}
