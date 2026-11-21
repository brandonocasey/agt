//! Find the branch a worktree is rebasing or bisecting.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use super::{Repo, Worktree};
use crate::core::error::{Result, ctx};

/// The directories that hold a rebase in progress.
pub(crate) const REBASE_DIRS: [&str; 2] = ["rebase-merge", "rebase-apply"];

#[derive(PartialEq, Eq)]
pub(crate) enum BranchOperation {
    Rebase,
    Bisect,
}

impl Repo {
    pub(crate) fn branch_in_use(&self, branch: &str) -> Result<bool> {
        for entry in self.worktrees()? {
            if entry.branch.as_deref() == Some(branch)
                || self.branch_operation(&entry, branch)?.is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A rebase or bisect can hold the branch even while HEAD is detached.
    pub(crate) fn branch_operation(
        &self,
        entry: &Worktree,
        branch: &str,
    ) -> Result<Option<BranchOperation>> {
        let Some(git_dir) = self.git_dir_of(entry)? else {
            return Ok(None);
        };
        if rebases(&git_dir, branch) {
            return Ok(Some(BranchOperation::Rebase));
        }
        Ok(bisects(&git_dir, branch).then_some(BranchOperation::Bisect))
    }

    /// The git directory of the work tree `entry`.
    /// A deleted work tree has an entry under `worktrees` in the common git directory.
    /// `None` for a bare repository or a missing entry.
    fn git_dir_of(&self, entry: &Worktree) -> Result<Option<PathBuf>> {
        if entry.bare {
            return Ok(None);
        }
        if entry.path.exists() {
            return Repo::at(&entry.path).git_dir().map(Some);
        }
        let admin = Path::new(&self.common_dir()?).join("worktrees");
        let listed = match std::fs::read_dir(&admin) {
            Ok(listed) => listed,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ctx(format!("read {}", admin.display()))(error)),
        };
        let wanted = entry.path.join(".git");
        Ok(listed
            .flatten()
            .map(|dir| dir.path())
            .find(|dir| gitdir_of(dir).is_some_and(|recorded| recorded == wanted)))
    }
}

/// What the rebase in `dir` of `git_dir` records under `name`.
pub(crate) fn rebase_record(git_dir: &Path, dir: &str, name: &str) -> Option<String> {
    std::fs::read_to_string(git_dir.join(dir).join(name))
        .ok()
        .map(|text| text.trim().to_owned())
}

/// The `.git` file path an entry under `worktrees` records as its work tree's. With
/// `worktree.useRelativePaths` (Git 2.48) it is relative to `admin`, so it is joined and folded.
fn gitdir_of(admin: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(admin.join("gitdir")).ok()?;
    let mut folded = PathBuf::new();
    for part in admin.join(text.trim_end()).components() {
        match part {
            Component::ParentDir => {
                folded.pop();
            }
            Component::CurDir => {}
            other => folded.push(other),
        }
    }
    Some(folded)
}

/// Whether the rebase in progress in `git_dir` is of `branch`.
fn rebases(git_dir: &Path, branch: &str) -> bool {
    let head = format!("refs/heads/{branch}");
    REBASE_DIRS
        .iter()
        .any(|dir| rebase_record(git_dir, dir, "head-name").is_some_and(|name| name == head))
}

/// Whether the bisect in progress in `git_dir` was started from `branch`.
fn bisects(git_dir: &Path, branch: &str) -> bool {
    std::fs::read_to_string(git_dir.join("BISECT_START")).is_ok_and(|start| start.trim() == branch)
}
