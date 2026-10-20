//! `agt branch`: read-only checks that replace hand-built git calls. `pushed` compares a branch
//! with its pinned remote tip and its PR/MR; `landable` compares it with the newest default base.

mod landable;
mod pushed;

use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};

use crate::core::error::{Result, fail};
use crate::core::output::Output;
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
    /// Check push state and merge conflicts, retaining each report and exit status.
    ///
    /// Prints `checks` keyed by check name, each with `status` and `report` (or `error`).
    /// Exit priority is 1 for errors, then 5 for conflicts, then 4 for not pushed, then 0.
    Check {
        /// Branch to check; default: the current branch.
        branch: Option<String>,
        #[arg(
            long,
            value_enum,
            value_delimiter = ',',
            default_value = "pushed,landable"
        )]
        checks: Vec<Check>,
        /// Remote used by the pushed check.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Target used by the landable check; default: the newest default base.
        #[arg(long)]
        target: Option<String>,
    },
    /// Whether the remote branch equals the local one, and the PR/MR head matches.
    ///
    /// Fetches the remote branch, pins the fetched commit and prints JSON: `state` (`equal`,
    /// `ahead`, `behind`, `diverged` or `missing` on the remote), `ahead` and `behind` counts,
    /// `uncommitted` (changed paths in the worktree that has the branch checked out, else null)
    /// and, when a PR/MR exists, `pr` with `head_matches` and `pipeline.exists` for its head
    /// commit. When the forge lookup fails (`gh` or `glab` missing or not logged in) `pr` is null
    /// and a warning names the cause; the exit code then follows the git state alone. Exit 0 only when `state` is `equal` and any PR/MR head equals the local tip; exit
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

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Check {
    Pushed,
    Landable,
}

impl Check {
    fn name(self) -> &'static str {
        match self {
            Self::Pushed => "pushed",
            Self::Landable => "landable",
        }
    }

    fn run(
        self,
        repo: &Repo,
        branch: &str,
        remote: &str,
        target: Option<&str>,
    ) -> Result<(Value, u8)> {
        match self {
            Self::Pushed => pushed::run(repo, branch, remote),
            Self::Landable => landable::run(repo, branch, target),
        }
    }
}

pub fn run(cli: Cli) -> Result<Output> {
    let current =
        std::env::current_dir().map_err(crate::core::error::ctx("read the current directory"))?;
    let repo = Repo::at(&current);
    let (branch, checks, remote, target, combined) = match cli.command {
        Command::Check {
            branch,
            checks,
            remote,
            target,
        } => (branch, checks, remote, target, true),
        Command::Pushed { branch, remote } => (branch, vec![Check::Pushed], remote, None, false),
        Command::Landable { branch, target } => {
            (branch, vec![Check::Landable], String::new(), target, false)
        }
    };
    let branch = branch_of(&repo, branch)?;
    let mut reports = serde_json::Map::new();
    let mut status = 0;
    for check in [Check::Pushed, Check::Landable] {
        if !checks.contains(&check) {
            continue;
        }
        let result = check.run(&repo, &branch, &remote, target.as_deref());
        if !combined {
            let (value, code) = result?;
            return finish(value, code);
        }
        let (report, code) = match result {
            Ok((value, code)) => (json!({"status": code, "report": value}), code),
            Err(error) => (json!({"status": 1, "error": error.to_string()}), 1),
        };
        reports.insert(check.name().to_owned(), report);
        if status != 1 {
            status = if code == 1 { 1 } else { status.max(code) };
        }
    }
    finish(json!({"checks": reports, "status": status}), status)
}

/// Prints `value`, then exits with `code` unless it is 0.
fn finish(value: Value, code: u8) -> Result<Output> {
    Ok(Output::Json(value).exiting(code))
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
