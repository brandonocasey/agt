//! What `agt cleanup` lists for branches and worktree backups: branches already in the default
//! branch by local ancestry alone, and the backups `agt worktree remove` made. Nothing here
//! touches the network or runs `lsof`; removing the worktrees of landed work is
//! `agt cleanup --only worktrees`.

use std::path::PathBuf;

use super::backup;
use super::info;
use super::landed::Scene;
use super::roots;
use crate::core::error::Result;
use crate::git::Repo;

/// A merged branch that `agt cleanup --only worktrees` would delete and no worktree holds.
pub struct Found {
    pub repo: PathBuf,
    pub branch: String,
}

/// A worktree backup `agt worktree remove` made.
pub struct Backup {
    pub id: String,
    /// The backup's directory.
    pub dir: PathBuf,
}

/// Every worktree backup, oldest first.
pub fn backups() -> Result<Vec<Backup>> {
    Ok(backup::all()?
        .into_iter()
        .map(|(record, dir)| Backup { id: record.id, dir })
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
            found.push(Found {
                repo: main.clone(),
                branch,
            });
        }
    }
    Ok(found)
}
