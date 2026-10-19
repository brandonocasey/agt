//! `agt branch`: read-only checks that replace hand-built git calls. `pushed` compares a branch
//! with its pinned remote tip and its PR/MR; `landable` compares it with the newest default base.

mod landable;
mod pushed;

use clap::{Args, Subcommand};
use serde_json::Value;

use crate::core::error::{Result, fail};
use crate::core::output::{self, Output};
use crate::git::Repo;

/// Exit status of `pushed` when the remote branch is not equal to the local one, or its PR/MR
/// head differs.
const NOT_PUSHED: u8 = 4;
/// Exit status of `landable` when merging the branch into the target conflicts.
const CONFLICTS: u8 = 5;

/// Pushed and landable checks for a branch.
#[derive(Args)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Whether the remote branch equals the local one, and the PR/MR head matches.
    ///
    /// Fetches the remote branch, pins the fetched commit and prints JSON: `state` (`equal`,
    /// `ahead`, `behind`, `diverged` or `missing` on the remote), `ahead` and `behind` counts,
    /// `uncommitted` (changed paths in the worktree that has the branch checked out, else null)
    /// and, when a PR/MR exists, `pr` with `head_matches` and `pipeline.exists` for its head
    /// commit. Exit 0 only when `state` is `equal` and any PR/MR head equals the local tip; exit
    /// 4 otherwise; exit 1 on an error.
    #[command(verbatim_doc_comment)]
    Pushed {
        /// Branch to check; default: the current branch.
        branch: Option<String>,
        #[arg(long, default_value = "origin")]
        remote: String,
    },
    /// Whether the branch merges cleanly into the target.
    ///
    /// Prints JSON: `behind` and `ahead` counts against the target, `conflicts` (paths from
    /// `git merge-tree --write-tree`) and `already_in_target` (commits of the branch whose patch
    /// is in the target, from `git cherry`). The target defaults to the newest default base
    /// among every remote's live default branch and the local one, as `agt worktree create`
    /// picks it. Exit 0 when there are no conflicts, 5 when there are, 1 on an error.
    #[command(verbatim_doc_comment)]
    Landable {
        /// Branch to check; default: the current branch.
        branch: Option<String>,
        #[arg(long)]
        target: Option<String>,
    },
}

pub fn run(cli: Cli) -> Result<Output> {
    let current =
        std::env::current_dir().map_err(crate::core::error::ctx("read the current directory"))?;
    let repo = Repo::at(&current);
    let (value, code) = match cli.command {
        Command::Pushed { branch, remote } => {
            pushed::run(&repo, &branch_of(&repo, branch)?, &remote)?
        }
        Command::Landable { branch, target } => {
            landable::run(&repo, &branch_of(&repo, branch)?, target.as_deref())?
        }
    };
    finish(value, code)
}

/// Prints `value`, then exits with `code` unless it is 0.
fn finish(value: Value, code: u8) -> Result<Output> {
    if code == 0 {
        return Ok(Output::Json(value));
    }
    output::print(Output::Json(value))?;
    Ok(Output::Exit(code))
}

/// The branch named, or the current one, and its tip.
fn branch_of(repo: &Repo, branch: Option<String>) -> Result<String> {
    match branch.or(repo.current_branch()?) {
        Some(branch) => Ok(branch),
        None => fail("HEAD is detached; name a branch"),
    }
}

/// The commit of local branch `branch`.
fn tip(repo: &Repo, branch: &str) -> Result<String> {
    let spec = format!("refs/heads/{branch}^{{commit}}");
    match repo.git_opt(&["rev-parse", "--verify", "--quiet", &spec])? {
        Some(commit) => Ok(commit),
        None => fail(format!("no local branch {branch}")),
    }
}
