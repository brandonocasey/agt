//! Moving the checked-out branch forward, and the branches that other worktrees hold: the
//! parts of `git merge --ff-only` and `git fetch .` that libgit2 leaves to the caller.

use std::path::{Component, Path, PathBuf};

use super::{Repo, Worktree, commit_id};
use crate::core::error::{Error, Result, ctx};

/// The directories that hold a rebase in progress.
const REBASE_DIRS: [&str; 2] = ["rebase-merge", "rebase-apply"];

/// What a work tree is busy with on a branch, which stops git from moving that branch.
pub enum Busy {
    Rebasing,
    Bisecting,
}

/// A work tree that is rebasing or bisecting a branch.
pub struct InUse {
    pub path: PathBuf,
    pub busy: Busy,
    /// Its directory is gone and `git worktree prune` would drop the entry.
    pub stale: bool,
}

impl Repo {
    /// The work trees rebasing or bisecting `branch`, also deleted ones. Git will not move a
    /// branch that such a work tree uses.
    pub fn busy_on(&self, branch: &str) -> Result<Vec<InUse>> {
        let mut found = Vec::new();
        for entry in self.worktrees()? {
            let Some(git_dir) = self.git_dir_of(&entry)? else {
                continue;
            };
            let busy = if rebases(&git_dir, branch) {
                Busy::Rebasing
            } else if bisects(&git_dir, branch) {
                Busy::Bisecting
            } else {
                continue;
            };
            found.push(InUse {
                stale: !entry.path.exists(),
                path: entry.path,
                busy,
            });
        }
        Ok(found)
    }

    /// The work trees that have `branch` checked out. A rebase detaches HEAD, so one
    /// rebasing `branch` counts too.
    pub fn holders(&self, branch: &str) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        for entry in self.worktrees()? {
            let rebasing = self
                .git_dir_of(&entry)?
                .is_some_and(|git_dir| rebases(&git_dir, branch));
            if rebasing || entry.branch.as_deref() == Some(branch) {
                found.push(entry.path);
            }
        }
        Ok(found)
    }

    /// The git directory of the work tree `entry`. A deleted work tree has an entry under
    /// `worktrees` in the common git directory. `None` for a bare repository or a missing
    /// entry.
    pub fn git_dir_of(&self, entry: &Worktree) -> Result<Option<PathBuf>> {
        if entry.bare {
            return Ok(None);
        }
        if entry.path.exists() {
            return Repo::at(&entry.path).git_dir().map(Some);
        }
        let admin = Path::new(&self.common_dir()?).join("worktrees");
        let listed = match std::fs::read_dir(&admin) {
            Ok(listed) => listed,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ctx(format!("read {}", admin.display()))(error)),
        };
        let wanted = entry.path.join(".git");
        Ok(listed
            .flatten()
            .map(|dir| dir.path())
            .find(|dir| gitdir_of(dir).is_some_and(|recorded| recorded == wanted)))
    }

    /// Moves the checked-out branch and the work tree to `to`, a descendant of HEAD, as
    /// `git merge --ff-only --no-overwrite-ignore`. A local change or an untracked or ignored
    /// file that the move would overwrite is refused with git's wording.
    pub fn fast_forward_checkout(&self, to: &str) -> Result<()> {
        let repo = self.open_writing()?;
        let target = repo.find_commit(commit_id(&repo, to)?)?;
        let head = repo.head()?;
        let current = head.peel_to_commit()?.id();
        if current == target.id() {
            return Ok(());
        }
        if !repo.graph_descendant_of(target.id(), current)? {
            return Err(Error("Not possible to fast-forward, aborting.".into()));
        }
        let mut blocked = Vec::new();
        let mut options = git2::build::CheckoutBuilder::new();
        options
            .safe()
            .overwrite_ignored(false)
            .notify_on(git2::CheckoutNotificationType::CONFLICT)
            .notify(|_, path, _, _, _| {
                blocked.extend(path.map(Path::to_owned));
                true
            });
        let checked = repo.checkout_tree(target.as_object(), Some(&mut options));
        drop(options);
        if checked.is_err() && !blocked.is_empty() {
            return Err(Error(overwrite_message(&repo, &blocked)?));
        }
        checked?;
        let mut branch = head.resolve()?;
        branch.set_target(target.id(), &format!("merge {to}: Fast-forward"))?;
        if self.sparse_checkout_enabled()? {
            self.sparse_reapply()?;
        }
        Ok(())
    }

    /// Moves `refs/heads/<name>` from `from` to `to` without a work tree, as `git fetch . to:name`
    /// does: refused for a branch that a work tree is rebasing or bisecting.
    pub fn fast_forward_ref(&self, name: &str, from: &str, to: &str) -> Result<()> {
        if let Some(busy) = self.busy_on(name)?.first() {
            return Err(Error(format!(
                "refusing to fetch into branch 'refs/heads/{name}' checked out at '{}'",
                busy.path.display()
            )));
        }
        self.move_ref(
            &format!("refs/heads/{name}"),
            from,
            to,
            &format!("fetch . {to}: fast-forward"),
        )
    }
}

/// Git's message for a merge that would overwrite `paths`: local changes to tracked files, or
/// untracked ones.
fn overwrite_message(repo: &git2::Repository, paths: &[PathBuf]) -> Result<String> {
    let index = repo.index()?;
    let (tracked, untracked): (Vec<&PathBuf>, Vec<&PathBuf>) = paths
        .iter()
        .partition(|path| index.get_path(path, 0).is_some());
    let list = |paths: &[&PathBuf]| -> String {
        paths
            .iter()
            .map(|path| format!("\t{}\n", path.display()))
            .collect()
    };
    let mut message = String::new();
    if !tracked.is_empty() {
        message.push_str(&format!(
            "Your local changes to the following files would be overwritten by merge:\n{}Please commit your changes or stash them before you merge.\n",
            list(&tracked)
        ));
    }
    if !untracked.is_empty() {
        message.push_str(&format!(
            "The following untracked working tree files would be overwritten by merge:\n{}Please move or remove them before you merge.\n",
            list(&untracked)
        ));
    }
    Ok(message)
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

/// What the rebase in `dir` of `git_dir` records under `name`.
fn rebase_record(git_dir: &Path, dir: &str, name: &str) -> Option<String> {
    std::fs::read_to_string(git_dir.join(dir).join(name))
        .ok()
        .map(|text| text.trim().to_owned())
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
