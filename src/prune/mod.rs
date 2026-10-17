//! `agt prune`: one pass over every store namespace and its stale files. It reads each domain's
//! own rule for what is expired and deletes only under that item's lock, skipping what is busy.

mod apply;
mod plan;

use clap::{Args, ValueEnum};
use serde_json::json;

use crate::core::clock;
use crate::core::error::Result;
use crate::core::output::Output;
use crate::store::prune::PRUNE_AFTER_SECS;

#[derive(Args)]
pub struct Cli {
    /// Delete what the plan lists. Without it nothing is deleted and the plan is printed.
    #[arg(long)]
    yes: bool,
    /// Limit the pass to these kinds of thing; repeat for several. Default: all.
    #[arg(long, value_enum, value_name = "KIND")]
    only: Vec<Only>,
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
}

pub fn run(cli: Cli) -> Result<Output> {
    let now = clock::now_secs()?;
    let only = |kind: Only| cli.only.is_empty() || cli.only.contains(&kind);
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
    let outcome = cli.yes.then(|| apply::run(&plan, now));
    let mut report = json!({
        "dry_run": !cli.yes,
        "now": clock::format(now),
        "retention_days": PRUNE_AFTER_SECS / 86_400,
        "plan": plan.json(),
        "totals": plan.totals(),
        "warnings": plan.warnings,
    });
    if let (Some(outcome), Some(map)) = (outcome, report.as_object_mut()) {
        map.extend(outcome.json());
    }
    Ok(Output::Json(report))
}
