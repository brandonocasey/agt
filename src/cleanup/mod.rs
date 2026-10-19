//! `agt cleanup`: one pass over what agents leave behind. Store documents follow each domain's
//! own rule for what is expired and are deleted only under that item's lock, skipping what is
//! busy. Cache files go by age, orphan processes by their gone session, and the rest is only
//! listed.

mod apply;
mod files;
mod manual;
mod plan;
mod procs;

use clap::{Args, ValueEnum};
use serde_json::{Value, json};

use crate::core::clock;
use crate::core::error::{Result, fail};
use crate::core::output::Output;
use crate::store::prune::PRUNE_AFTER_SECS;
use crate::worktree;

#[derive(Args)]
pub struct Cli {
    /// Delete what the plan lists. Without it nothing is deleted and the plan is printed.
    #[arg(long)]
    yes: bool,
    /// Limit the pass to these kinds of thing; repeat for several. Default: all but
    /// `worktrees`. `--yes` signals processes only when `processes` is named here.
    #[arg(long, value_enum, value_name = "KIND")]
    only: Vec<Only>,
    /// Print how many leftovers the last `--yes` run listed for you to remove, from its saved
    /// report, and exit. Prints nothing when there are none.
    #[arg(long, conflicts_with_all = ["yes", "only"])]
    count: bool,
    #[command(flatten)]
    worktrees: worktree::cleanup::Options,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Only {
    /// Review targets, and `local/` repositories whose targets have all expired.
    Reviews,
    Issues,
    Tasks,
    /// agt's own pre-migration backups and unfinished backup copies.
    Backups,
    /// Lock files whose document is gone.
    Locks,
    /// Long-output copies in `agents/output` untouched for 7 days.
    Output,
    /// `agents/scratch` and Claude Code session scratchpads untouched for 30 days.
    Scratch,
    /// Processes a gone Claude Code session started, a day or more old.
    Processes,
    /// Merged worktrees and branches, other backups, old Codex releases and Playwright
    /// builds: listed, never deleted. `--yes` saves the list to a report.
    Manual,
    /// Worktrees and local branches whose work landed in the default branch, in the current
    /// repository. Only when named: `--yes` then removes each worktree after a snapshot
    /// backup and deletes its branch.
    Worktrees,
}

pub fn run(cli: Cli) -> Result<Output> {
    let sweep = cli.only.contains(&Only::Worktrees);
    if !sweep && let Some(flag) = cli.worktrees.flag_given() {
        return fail(format!("{flag} needs --only worktrees"));
    }
    if cli.count {
        return Ok(Output::Text(match manual::saved_count()? {
            Some((count, path)) if count > 0 => {
                format!("agt: {count} cleanup candidates in {}\n", path.display())
            }
            _ => String::new(),
        }));
    }
    // The sweep runs first: its error (such as no repository here) then stops the pass before
    // anything is deleted, and the manual list below no longer shows what it removed.
    let mut worktrees = cli.worktrees;
    worktrees.yes = cli.yes;
    let worktrees = if sweep {
        Some(worktree::cleanup::run(&worktrees)?)
    } else {
        None
    };
    let now = clock::now_secs()?;
    // Removing worktrees and deleting branches is never part of the default pass.
    let only =
        |kind: Only| cli.only.contains(&kind) || (cli.only.is_empty() && kind != Only::Worktrees);
    let mut plan = plan::Plan::default();
    if only(Only::Reviews) {
        plan.reviews(now)?;
    }
    if only(Only::Issues) {
        plan.issues(now)?;
    }
    if only(Only::Tasks) {
        plan.tasks(now)?;
    }
    if only(Only::Locks) {
        plan.locks()?;
    }
    if only(Only::Backups) {
        plan.backups(now)?;
    }
    if only(Only::Output) {
        plan.output(now)?;
    }
    if only(Only::Scratch) {
        plan.scratch(now)?;
    }
    if only(Only::Processes) {
        plan.processes(now)?;
    }
    let candidates = if only(Only::Manual) {
        Some(manual::candidates()?)
    } else {
        None
    };
    let signal = cli.only.contains(&Only::Processes);
    let outcome = cli.yes.then(|| apply::run(&plan, now, signal));
    let mut report = json!({
        "dry_run": !cli.yes,
        "now": clock::format(now),
        "retention_days": PRUNE_AFTER_SECS / 86_400,
        "plan": plan.json(),
        "totals": plan.totals(),
        "warnings": plan.warnings,
    });
    if let Some(map) = report.as_object_mut() {
        if let Some(outcome) = outcome {
            map.extend(outcome.json());
        }
        if let Some(worktrees) = worktrees {
            map.insert("worktrees".into(), worktrees);
        }
        if let Some(candidates) = &candidates {
            map.insert("manual".into(), manual::json(candidates));
            // What was already deleted must still be reported, so a failed save only warns.
            if cli.yes {
                match manual::save(candidates, now) {
                    Ok(path) => {
                        map.insert("manual_report".into(), json!(path));
                    }
                    Err(error) => {
                        if let Some(Value::Array(warnings)) = map.get_mut("warnings") {
                            warnings.push(json!(format!("manual report not saved: {error}")));
                        }
                    }
                }
            }
        }
    }
    Ok(Output::Json(report))
}
