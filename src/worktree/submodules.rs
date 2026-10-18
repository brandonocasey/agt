//! Submodule state that removing a worktree would lose.

use std::path::Path;

use crate::core::error::{Result, fail};
use crate::git::Repo;

/// Refuses when an initialized submodule of the worktree has work that exists nowhere else: a
/// checkout off the recorded commit, uncommitted changes, or a HEAD that no remote-tracking
/// branch of the submodule and no clone in the main checkout holds.
pub fn check(worktree: &Path, main: &Path) -> Result<()> {
    let repo = Repo::at(worktree);
    let status = repo.git(&["submodule", "status", "--recursive"])?;
    for line in status.lines() {
        let Some(prefix) = line.chars().next().filter(|c| *c != '-') else {
            continue;
        };
        let mut fields = line[prefix.len_utf8()..].split_whitespace();
        let (Some(commit), Some(path)) = (fields.next(), fields.next()) else {
            continue;
        };
        let name = format!("submodule {path} at {commit}");
        if prefix == '+' || prefix == 'U' {
            return fail(format!(
                "{name} is not at the commit the worktree records; check it in or reset it first"
            ));
        }
        let sub = Repo::at(worktree.join(path));
        let dirty = sub.git(&["status", "--porcelain", "--ignore-submodules=none"])?;
        if !dirty.is_empty() {
            return fail(format!("{name} has uncommitted changes"));
        }
        let remote = sub.git(&["branch", "-r", "--contains", commit])?;
        if !remote.is_empty() {
            continue;
        }
        let main_sub = main.join(path);
        let spec = format!("{commit}^{{commit}}");
        let in_main = main_sub.join(".git").exists() && Repo::at(&main_sub).object_exists(&spec)?;
        if !in_main {
            return fail(format!(
                "{name} is on no remote-tracking branch and is not in the main checkout's clone; push it first"
            ));
        }
    }
    Ok(())
}
