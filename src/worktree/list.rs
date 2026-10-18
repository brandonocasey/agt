use clap::Args;
use serde_json::{Value, json};

use super::info::{self, agents_in, default_branch, describe};
use super::roots;
use crate::core::agents;
use crate::core::error::Result;
use crate::core::output::Output;
use crate::git::Repo;

#[derive(Args)]
pub struct Cli {
    /// List the worktrees of every repository under the configured roots
    /// (`[worktree] roots` in the agt config), not only the current one.
    #[arg(long)]
    all: bool,
}

pub fn run(cli: Cli) -> Result<Output> {
    let repos = if cli.all {
        roots::repositories()?.into_iter().map(Repo::at).collect()
    } else {
        vec![Repo::discover()?]
    };
    let live = agents::live();
    let mut rows: Vec<Value> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for repo in repos {
        match rows_of(&repo, &live) {
            Ok(found) => rows.extend(found),
            Err(error) if cli.all => {
                warnings.push(format!("{}: {error}", repo.root().display()));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(Output::Json(
        json!({ "worktrees": rows, "warnings": warnings }),
    ))
}

fn rows_of(repo: &Repo, live: &[agents::Live]) -> Result<Vec<Value>> {
    let entries = info::entries(repo)?;
    let default = default_branch(repo)?;
    let mut rows = Vec::new();
    for entry in entries.iter().skip(1) {
        let mut row = describe(repo, entry, default.as_ref())?;
        row["repository"] = json!(entries[0].path);
        row["agents"] = agents_in(live, &entry.path);
        rows.push(row);
    }
    Ok(rows)
}
