//! What `agt cleanup` lists for branches and worktree backups: branches already in the default
//! branch by local ancestry alone, and the backups `agt worktree remove` made; and the checkouts
//! whose scratch it sweeps. Nothing here
//! touches the network or runs `lsof`; removing the worktrees of landed work is
//! `agt cleanup --only worktrees`.

use std::path::PathBuf;

use super::backup;
use super::info;
use super::landed::Scene;
use super::roots;
use crate::core::error::{Result, ctx};
use crate::git::Repo;

/// A merged branch that `agt cleanup --only worktrees` would delete and no worktree holds.
pub struct Found {
    pub repo: PathBuf,
    pub branch: String,
    /// When the branch's tip was committed, in Unix seconds.
    pub committed: i64,
}

/// A worktree backup `agt worktree remove` made.
pub struct Backup {
    pub id: String,
    /// The backup's directory.
    pub dir: PathBuf,
    /// When it was made, as the record states it.
    pub created: String,
}

/// Every worktree backup, oldest first.
pub fn backups() -> Result<Vec<Backup>> {
    Ok(backup::all()?
        .into_iter()
        .map(|(record, dir)| Backup {
            id: record.id,
            dir,
            created: record.created,
        })
        .collect())
}

/// The candidates of every repository under the configured roots, in repository order.
pub fn candidates() -> Result<Vec<Found>> {
    let mut found = Vec::new();
    for root in roots::repositories()? {
        // A repository git cannot read is none of cleanup's business.
        if let Ok(mut here) = of(&Repo::at(&root)) {
            found.append(&mut here);
        }
    }
    Ok(found)
}

/// The repositories under the configured roots, then the one holding the current directory.
pub fn repositories_and_current() -> Result<Vec<PathBuf>> {
    let mut found = roots::repositories()?;
    if let Ok(here) = Repo::discover() {
        found.push(here.root().to_path_buf());
    }
    Ok(found)
}

/// Every checkout, main and linked, of [`repositories_and_current`] where git tracks nothing
/// under `path`.
pub fn untracked_checkouts(path: &str) -> Result<Vec<PathBuf>> {
    let mut found: Vec<PathBuf> = repositories_and_current()?
        .into_iter()
        .filter_map(|root| Repo::at(root).worktrees().ok())
        .flatten()
        .filter(|worktree| !worktree.bare && !worktree.prunable)
        .filter_map(|worktree| std::fs::canonicalize(worktree.path).ok())
        .collect();
    found.sort();
    found.dedup();
    found.retain(|checkout| {
        Repo::at(checkout)
            .git(&["ls-files", "-z", "--", path])
            .is_ok_and(|tracked| tracked.is_empty())
    });
    Ok(found)
}

fn of(repo: &Repo) -> Result<Vec<Found>> {
    let Some(scene) = Scene::read(repo)? else {
        return Ok(Vec::new());
    };
    let main = info::real(&scene.main().path)?;
    let mut found = Vec::new();
    for local in scene.local_findings(repo)? {
        if !local.finding.verdict.landed() || local.worktree.is_some() {
            continue;
        }
        if let Some(branch) = local.branch {
            let tip = format!("refs/heads/{branch}");
            let committed = repo.git(&[
                "-c",
                "log.showSignature=false",
                "log",
                "-1",
                "--format=%ct",
                &tip,
            ])?;
            let committed = committed
                .trim()
                .parse()
                .map_err(ctx(format!("read the commit time of {tip}")))?;
            found.push(Found {
                repo: main.clone(),
                branch,
                committed,
            });
        }
    }
    Ok(found)
}
