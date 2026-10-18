//! Submodule state that removing a worktree would lose. `git worktree remove` deletes the
//! submodule clones kept under the worktree's git directory with it.

use std::path::Path;

use super::info;
use crate::core::error::{Result, fail};
use crate::git::Repo;

/// Whether a commit of the submodule `sub` is on a remote-tracking branch of it or in the
/// clone `main_sub` of the main checkout.
fn held_elsewhere(sub: &Repo, main_sub: &Path, commit: &str) -> Result<bool> {
    if !sub.git(&["branch", "-r", "--contains", commit])?.is_empty() {
        return Ok(true);
    }
    let spec = format!("{commit}^{{commit}}");
    Ok(main_sub.join(".git").exists() && Repo::at(main_sub).object_exists(&spec)?)
}

/// Refuses when an initialized submodule of the worktree (recursively) has work that exists
/// nowhere else: a checkout off the recorded commit, uncommitted changes, a stash, ignored files
/// that are not disposable, a HEAD or a local branch that no remote-tracking branch of the
/// submodule and no clone in the main checkout holds. Commits only a reflog holds are not
/// protected.
pub fn check(worktree: &Path, main: &Path) -> Result<()> {
    let repo = Repo::at(worktree);
    let status = repo.git(&["submodule", "status", "--recursive"])?;
    for line in status.lines() {
        let Some(prefix) = line.chars().next().filter(|c| *c != '-') else {
            continue;
        };
        // `<sha> <path>` with an optional ` (<describe>)`; the path may hold spaces.
        let Some((commit, rest)) = line[prefix.len_utf8()..].trim().split_once(' ') else {
            continue;
        };
        let path = match rest.rfind(" (").filter(|_| rest.ends_with(')')) {
            Some(end) => &rest[..end],
            None => rest,
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
        let main_sub = main.join(path);
        if sub
            .git_opt(&["rev-parse", "--verify", "--quiet", "refs/stash"])?
            .is_some()
        {
            return fail(format!("{name} has a stash; apply or drop it first"));
        }
        let ignored = info::ignored(&worktree.join(path))?.copy;
        if !ignored.is_empty() {
            return fail(format!(
                "{name} has ignored files that no backup keeps: {}; move them first",
                ignored.join(", ")
            ));
        }
        if !held_elsewhere(&sub, &main_sub, commit)? {
            return fail(format!(
                "{name} is on no remote-tracking branch and is not in the main checkout's clone; push it first"
            ));
        }
        let branches = sub.git(&[
            "for-each-ref",
            "--format=%(objectname) %(refname:short)",
            "refs/heads",
        ])?;
        for line in branches.lines() {
            let Some((tip, branch)) = line.split_once(' ') else {
                continue;
            };
            if !held_elsewhere(&sub, &main_sub, tip)? {
                return fail(format!(
                    "{name} has the local branch {branch} that is on no remote-tracking branch and is not in the main checkout's clone; push it first"
                ));
            }
        }
    }
    Ok(())
}
