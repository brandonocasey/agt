//! The linked worktrees of a repository, read through libgit2.

use std::fs;
use std::path::{Path, PathBuf};

use super::{Repo, Worktree, local_branch_name, without_trailing_slash};
use crate::core::error::{Error, Result};

/// The id git prints as the `HEAD` of a branch that has no commit yet.
const UNBORN_HEAD: &str = "0000000000000000000000000000000000000000";

impl Repo {
    /// Every worktree of the repository; the first is the main checkout.
    pub fn worktrees(&self) -> Result<Vec<Worktree>> {
        let repo = self.open_refs()?;
        let common = without_trailing_slash(repo.commondir());
        // A submodule's git directory may name a work tree that is gone: then only its refs
        // and config can be read.
        let main = git2::Repository::open(&common)
            .or_else(|_| git2::Repository::open_bare(&common))
            .map_err(|error| Error(format!("open {}: {}", common.display(), error.message())))?;
        let mut found = vec![main_entry(&main, &common)];
        let mut names: Vec<String> = main
            .worktrees()?
            .iter()
            .flatten()
            .map(str::to_owned)
            .collect();
        names.sort();
        for name in names {
            let linked = main.find_worktree(&name)?;
            found.push(linked_entry(&main, &common, &name, &linked));
        }
        Ok(found)
    }
}

/// The checkout the repository was made in, or the bare repository itself.
fn main_entry(main: &git2::Repository, common: &Path) -> Worktree {
    if super::env_config::is_bare(main) {
        return Worktree {
            path: common.to_owned(),
            head: String::new(),
            branch: None,
            locked: false,
            bare: true,
            prunable: false,
        };
    }
    let branch = main.find_reference("HEAD").ok().and_then(|head| {
        head.symbolic_target()
            .map(|name| local_branch_name(name).to_owned())
    });
    let head = main
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok())
        .map_or_else(|| UNBORN_HEAD.to_owned(), |commit| commit.id().to_string());
    Worktree {
        path: main
            .workdir()
            .map(without_trailing_slash)
            .unwrap_or_default(),
        head,
        branch,
        locked: false,
        bare: false,
        prunable: false,
    }
}

/// One linked worktree. Its `HEAD` file is read as it is: the directory may be gone.
fn linked_entry(
    main: &git2::Repository,
    common: &Path,
    name: &str,
    linked: &git2::Worktree,
) -> Worktree {
    let head_file =
        fs::read_to_string(common.join("worktrees").join(name).join("HEAD")).unwrap_or_default();
    let head_file = head_file.trim();
    let target = head_file.strip_prefix("ref: ");
    let head = match target {
        Some(name) => main
            .find_reference(name)
            .ok()
            .and_then(|found| found.peel_to_commit().ok())
            .map_or_else(|| UNBORN_HEAD.to_owned(), |commit| commit.id().to_string()),
        None => head_file.to_owned(),
    };
    Worktree {
        path: PathBuf::from(linked.path()),
        head,
        branch: target.map(|name| local_branch_name(name).to_owned()),
        locked: matches!(linked.is_locked(), Ok(git2::WorktreeLockStatus::Locked(_))),
        bare: false,
        prunable: linked.is_prunable(None).unwrap_or(false),
    }
}
