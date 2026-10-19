//! `agt branch landable`.

use serde_json::{Value, json};

use super::{CONFLICTS, tip};
use crate::core::error::Result;
use crate::git::Repo;
use crate::git::base::{candidates, choose};

pub fn run(repo: &Repo, branch: &str, target: Option<&str>) -> Result<(Value, u8)> {
    let tip = tip(repo, branch)?;
    let (name, base) = match target {
        Some(target) => (target.to_owned(), repo.commit(target)?),
        None => {
            let found = candidates(repo)?;
            let (chosen, _) = choose(repo, &found)?;
            let chosen = &found[chosen];
            let name = match &chosen.remote {
                Some(remote) => format!("{remote}/{}", chosen.branch),
                None => chosen.branch.clone(),
            };
            (name, chosen.commit.clone())
        }
    };
    let conflicts = Repo::at(repo.root())
        .read_only()?
        .merge_conflicts(&base, &tip)?;
    let already: Vec<String> = repo
        .git(&["cherry", "--end-of-options", &base, &tip])?
        .lines()
        .filter_map(|line| line.strip_prefix("- "))
        .map(str::to_owned)
        .collect();
    let value = json!({
        "branch": branch,
        "commit": tip,
        "target": name,
        "target_commit": base,
        "behind": repo.distance(&tip, &base)?,
        "ahead": repo.distance(&base, &tip)?,
        "conflicts": conflicts,
        "already_in_target": already,
    });
    Ok((value, if conflicts.is_empty() { 0 } else { CONFLICTS }))
}
