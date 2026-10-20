use clap::{Parser, Subcommand};

use crate::branch;
use crate::ci;
use crate::cleanup;
use crate::core::error::Result;
use crate::core::output::Output;
use crate::doctor;
use crate::issue;
use crate::pr;
use crate::review;
use crate::secrets;
use crate::sessions;
use crate::task;
use crate::wait;
use crate::worktree;

/// Tools for coding agents.
#[derive(Parser)]
#[command(name = "agt", version, about)]
pub struct Cli {
    #[command(flatten)]
    offline: crate::forge::auto::Offline,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Local PR/MR-style review threads.
    Review(Box<review::cli::Cli>),
    /// Durable task records.
    Task(task::cli::Cli),
    /// Pushed and landable checks for a branch (`check`).
    Branch(branch::Cli),
    /// Wait for PR/MR checks and pipelines; GitLab failing specs versus the default branch.
    Ci(ci::Cli),
    /// Local issues with optional GitHub and GitLab sync.
    Issue(issue::Cli),
    /// Delete stale reviews, issues, tasks, locks and backups across every repository.
    Cleanup(cleanup::Cli),
    /// GitHub/GitLab PRs/MRs: view, list, create, update, merge behind a head-sha guard, pause auto-merge.
    Pr(pr::Cli),
    /// Cost, context, tool, cache, error and friction reports from agent transcripts, and search of what you typed.
    Sessions(sessions::Cli),
    /// Claude Code sessions and Codex processes running now: status, directory, branch, model, context, cost.
    Ps(sessions::live::Cli),
    /// Worktrees: create, prepare, port, list, remove after a backup, restore.
    Worktree(worktree::Cli),
    /// Scan for leaked credentials without printing them.
    ///
    /// Reads tracked and unignored text files (binary files are skipped). Prints `path:line rule`
    /// per hit (`--json`: rows of path, line, rule), never the matched text. Exit 0 when clean,
    /// 1 on any match, 2 on an error. A line containing `agt-secrets: allow` is skipped.
    Secrets(secrets::Cli),
    /// Check that this host and repository are set up for agt. Read-only; exit 0 ok, 1 a check failed, 2 warnings only.
    Doctor(doctor::Cli),
    /// Rerun a check until it succeeds or `--timeout` seconds pass (exit 3).
    Wait(wait::Cli),
}

pub fn run(cli: Cli) -> Result<Output> {
    cli.offline.install();
    match cli.command {
        Command::Review(args) => review::commands::run(*args),
        Command::Task(args) => task::cli::run(args),
        Command::Branch(args) => branch::run(args),
        Command::Ci(args) => ci::run(args),
        Command::Issue(args) => issue::run(args),
        Command::Pr(args) => pr::run(args),
        Command::Cleanup(args) => cleanup::run(args),
        Command::Sessions(args) => sessions::run(args),
        Command::Ps(args) => sessions::live::run(args),
        Command::Worktree(args) => worktree::run(args),
        Command::Secrets(args) => secrets::run(args),
        Command::Doctor(args) => doctor::run(args),
        Command::Wait(args) => wait::run(args),
    }
}
