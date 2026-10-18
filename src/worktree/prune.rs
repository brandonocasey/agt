//! What `agt cleanup` lists for worktrees and branches: work that is already in the default
//! branch by local ancestry alone. Nothing here touches the network or runs `lsof`.

use std::path::PathBuf;

use super::info::{self, Entry};
use super::landed::Scene;
use super::roots;
use crate::core::agents;
use crate::core::error::Result;
use crate::git::Repo;

/// A merged worktree or branch that `agt worktree cleanup` would remove.
pub struct Found {
    pub repo: PathBuf,
    pub worktree: Option<PathBuf>,
    pub branch: Option<String>,
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
    for entry in scene.entries.iter().skip(1) {
        let merged = match &entry.branch {
            Some(branch) => !scene.protected(branch),
            None => true,
        } && scene.contains(repo, &entry.head)?;
        if merged && idle(&scene, entry)? {
            found.push(Found {
                repo: main.clone(),
                worktree: Some(entry.path.clone()),
                branch: None,
            });
        }
    }
    for (branch, tip) in scene.branches(repo)? {
        if scene.worktree_of(&branch).is_none() && scene.contains(repo, &tip)? {
            found.push(Found {
                repo: main.clone(),
                worktree: None,
                branch: Some(branch),
            });
        }
    }
    Ok(found)
}

/// Whether the worktree exists, is unlocked, clean and without a live agent session.
fn idle(scene: &Scene, entry: &Entry) -> Result<bool> {
    if entry.locked || !entry.path.exists() || scene.holds_cwd(&entry.path) {
        return Ok(false);
    }
    Ok(info::unsaved(&entry.path)? == 0 && agents::live_in(&entry.path).is_none())
}
