//! What `agt cleanup` lists for worktrees and branches: work that is already in the default
//! branch by local ancestry alone. Nothing here touches the network or runs `lsof`.

use std::path::PathBuf;

use super::idle::{Allow, Cwds, blocker};
use super::info;
use super::landed::Scene;
use super::roots;
use crate::core::error::Result;
use crate::git::Repo;

/// A merged worktree or branch that `agt cleanup --only worktrees` would remove.
pub struct Found {
    pub repo: PathBuf,
    pub worktree: Option<PathBuf>,
    pub branch: Option<String>,
}

/// The candidates of every repository under the configured roots, in repository order.
pub fn candidates() -> Result<Vec<Found>> {
    let mut found = Vec::new();
    let mut cwds = Cwds::default();
    for root in roots::repositories()? {
        // A repository git cannot read is none of cleanup's business.
        if let Ok(mut here) = of(&Repo::at(&root), &mut cwds) {
            found.append(&mut here);
        }
    }
    Ok(found)
}

fn of(repo: &Repo, cwds: &mut Cwds) -> Result<Vec<Found>> {
    let Some(scene) = Scene::read(repo)? else {
        return Ok(Vec::new());
    };
    let main = info::real(&scene.main().path)?;
    // What the sweep may remove, offline: the same check, with no force flag.
    let allow = Allow {
        offline: true,
        recent: false,
        dirty: false,
    };
    let mut found = Vec::new();
    for local in scene.local_findings(repo)? {
        if !local.finding.verdict.landed() {
            continue;
        }
        match local.worktree {
            Some(entry) if blocker(&scene, entry, &main, allow, cwds)?.is_none() => {
                found.push(Found {
                    repo: main.clone(),
                    worktree: Some(entry.path.clone()),
                    branch: None,
                });
            }
            Some(_) => {}
            None => found.push(Found {
                repo: main.clone(),
                worktree: None,
                branch: local.branch,
            }),
        }
    }
    Ok(found)
}
