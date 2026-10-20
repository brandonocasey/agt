//! `agt branch check`: read-only checks that replace hand-built git calls. `pushed` compares a branch
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
        /// Base branch used by the landable check; default: the newest default base.
        #[arg(long)]
        base: Option<String>,
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
        base: Option<&str>,
    ) -> Result<(Value, u8)> {
        match self {
            Self::Pushed => pushed::run(repo, branch, remote),
            Self::Landable => landable::run(repo, branch, base),
        }
    }
}

pub fn run(cli: Cli) -> Result<Output> {
    let current =
        std::env::current_dir().map_err(crate::core::error::ctx("read the current directory"))?;
    let repo = Repo::at(&current);
    let Command::Check {
        branch,
        checks,
        remote,
        base,
    } = cli.command;
    let branch = branch_of(&repo, branch)?;
    let mut reports = serde_json::Map::new();
    let mut status = 0;
    for check in [Check::Pushed, Check::Landable] {
        if !checks.contains(&check) {
            continue;
        }
        let result = check.run(&repo, &branch, &remote, base.as_deref());
        let (report, code) = match result {
            Ok((value, code)) => (json!({"status": code, "report": value}), code),
            Err(error) => (json!({"status": 1, "error": error.to_string()}), 1),
        };
        reports.insert(check.name().to_owned(), report);
        if status != 1 {
            status = if code == 1 { 1 } else { status.max(code) };
        }
    }
    Ok(Output::Json(json!({"checks": reports, "status": status})).exiting(status))
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
    match repo.branch_tip(branch)? {
        Some(commit) => Ok(commit),
        None => fail(format!("no local branch {branch}")),
    }
}
