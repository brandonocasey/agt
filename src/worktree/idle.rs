//! Whether a linked worktree may be removed now: the one check that `agt cleanup`'s candidate
//! list and the sweep behind `agt cleanup --only worktrees` share. It includes whether any
//! process other than agt, its ancestors and the calling session's own processes has its
//! current directory inside the worktree.

use std::path::{Path, PathBuf};

use super::landed::{Scene, recent};
use super::remove::refuse;
use super::{gate, info};
use crate::core::error::Result;
use crate::core::{agents, clock};
use crate::git::Worktree;

/// What the check lets through, and how far it may reach.
#[derive(Clone, Copy)]
pub struct Allow {
    /// Check the gate without the network.
    pub offline: bool,
    /// Pass a worktree where something changed in the last 3 days.
    pub recent: bool,
    /// Pass a worktree with uncommitted work, which a snapshot backup keeps.
    pub dirty: bool,
}

/// Why a worktree must stay.
pub struct Blocker {
    /// The verdict the kept row reports instead of the branch's own.
    pub verdict: Option<&'static str>,
    pub reason: String,
}

fn blocked(reason: impl Into<String>) -> Result<Option<Blocker>> {
    Ok(Some(Blocker {
        verdict: None,
        reason: reason.into(),
    }))
}

/// Why the worktree must stay, if it must.
pub fn blocker(
    scene: &Scene,
    entry: &Worktree,
    main: &Path,
    allow: Allow,
    cwds: &mut Cwds,
) -> Result<Option<Blocker>> {
    if !entry.path.exists() {
        return blocked("the directory is missing; run git worktree prune");
    }
    let real = info::real(&entry.path)?;
    if let Err(error) = refuse(&scene.entries, &real, main) {
        return blocked(error.to_string());
    }
    let gated = if allow.offline {
        gate::check_offline(&real, main)
    } else {
        gate::check(&real, main)
    };
    if let Err(error) = gated {
        return blocked(error.to_string());
    }
    if let Some(reason) = cwds.busy(&real) {
        return blocked(reason);
    }
    if !allow.recent && recent(&real, clock::now_secs()?)? {
        return Ok(Some(Blocker {
            verdict: Some("recent"),
            reason: "something in it changed in the last 3 days; pass --recent to remove it anyway"
                .into(),
        }));
    }
    let unsaved = info::unsaved(&real)?;
    if unsaved > 0 && !allow.dirty {
        return blocked(format!(
            "dirty: {unsaved} modified or untracked files; pass --dirty to remove it after a snapshot backup"
        ));
    }
    Ok(None)
}

/// The current directories of all processes, read once on first use.
#[derive(Default)]
pub struct Cwds {
    all: Option<Result<Vec<(i32, PathBuf)>, String>>,
    /// The calling session's other processes, read when one is found inside a worktree.
    session: Option<Vec<i32>>,
}

impl Cwds {
    /// Why `root` is not idle: a process in it that is not the calling session's, or the
    /// reason this cannot be checked.
    pub fn busy(&mut self, root: &Path) -> Option<String> {
        let inside = match self.inside(root) {
            Err(why) => return Some(format!("cannot check running processes: {why}")),
            Ok(inside) => inside,
        };
        let session = self.session_tree(&inside);
        inside
            .iter()
            .find(|(pid, _)| !session.contains(pid))
            .map(|(pid, cwd)| describe(*pid, cwd, true))
    }

    /// The calling session's own processes inside `root`, which do not block it: stop them
    /// before removing it.
    pub fn own_in(&mut self, root: &Path) -> Vec<String> {
        let Ok(inside) = self.inside(root) else {
            return Vec::new();
        };
        let session = self.session_tree(&inside);
        inside
            .iter()
            .filter(|(pid, _)| session.contains(pid))
            .map(|(pid, cwd)| format!("{} (of the calling session)", describe(*pid, cwd, false)))
            .collect()
    }

    fn inside(&mut self, root: &Path) -> Result<Vec<(i32, PathBuf)>, String> {
        match self.all.get_or_insert_with(read) {
            Err(why) => Err(why.clone()),
            Ok(all) => Ok(all
                .iter()
                .filter(|(_, cwd)| cwd.starts_with(root))
                .cloned()
                .collect()),
        }
    }

    fn session_tree(&mut self, inside: &[(i32, PathBuf)]) -> &[i32] {
        if inside.is_empty() {
            return &[];
        }
        self.session.get_or_insert_with(agents::session_tree)
    }
}

/// `pid` and its program name; `full` adds the command line, which names what blocks a removal.
fn describe(pid: i32, cwd: &Path, full: bool) -> String {
    let command = agents::read(pid).map_or_else(
        || "?".to_owned(),
        |p| match p.cmd.as_str() {
            cmd if full && !cmd.is_empty() => format!("{}: {cmd}", p.comm),
            _ => p.comm,
        },
    );
    format!("process {pid} ({command}) runs in {}", cwd.display())
}

fn read() -> Result<Vec<(i32, PathBuf)>, String> {
    let own = agents::lineage();
    let mut all = agents::cwds_checked().map_err(|error| error.to_string())?;
    all.retain(|(pid, _)| !own.contains(pid));
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_failure_refuses_an_idle_worktree() {
        let mut cwds = Cwds {
            all: Some(Err("process table unavailable".to_owned())),
            session: None,
        };
        assert_eq!(
            cwds.busy(Path::new("/unused/worktree")),
            Some("cannot check running processes: process table unavailable".to_owned())
        );
    }
}
