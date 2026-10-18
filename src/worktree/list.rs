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
    for repo in repos {
        let entries = info::entries(&repo)?;
        let default = default_branch(&repo)?;
        for entry in entries.iter().skip(1) {
            let mut row = describe(&repo, entry, default.as_ref())?;
            row["repository"] = json!(entries[0].path);
            row["agents"] = agents_in(&live, &entry.path);
            rows.push(row);
        }
    }
    Ok(Output::Json(json!({ "worktrees": rows })))
}
