//! Moving the checked-out branch forward, and the branches that other worktrees hold: the
//! parts of `git merge --ff-only` and `git fetch .` that libgit2 leaves to the caller.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::filter::{self, Refusal};
use super::index::{copy_entry, left_out_paths};
use super::{Repo, Worktree, commit_id, folded, sparse};
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

    /// The work tree that stops git from checking `branch` out a second time: one that has it
    /// checked out, or is rebasing or bisecting it.
    pub fn checkout_blocker(&self, branch: &str) -> Result<Option<PathBuf>> {
        if let Some(holder) = self.holders(branch)?.into_iter().next() {
            return Ok(Some(holder));
        }
        Ok(self
            .busy_on(branch)?
            .into_iter()
            .next()
            .map(|busy| busy.path))
    }

    /// The git directory of work tree `entry`; a deleted one has an entry under `worktrees`.
    /// `None` for a bare repository or a missing entry.
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
        move_branch_to(&repo, &target, &format!("merge {to}: Fast-forward"))
    }

    /// Why checking the `paths` out of commit `source` into this work tree would be refused over
    /// a required filter, as [`Repo::fast_forward_checkout`] refuses; `None` when nothing stops
    /// it. A path `source` lacks is removed, and one a sparse checkout leaves out is not written.
    pub fn required_filter_refusal(
        &self,
        source: &str,
        paths: &[Vec<u8>],
    ) -> Result<Option<String>> {
        let repo = self.open()?;
        let guard = filter::Guard::new(&repo);
        if !guard.any_required {
            return Ok(None);
        }
        let tree = repo.find_commit(commit_id(&repo, source)?)?.tree()?;
        let left_out = left_out_paths(&repo)?;
        let mut outside_patterns = sparse::leaves_out(&repo)?;
        for path in paths {
            let at = Path::new(OsStr::from_bytes(path));
            let Ok(entry) = tree.get_path(at) else {
                continue;
            };
            if left_out.contains(path) || outside_patterns(path) {
                continue;
            }
            let mode = entry.filemode() as u32;
            if let Some(message) = guard.refusal_for_writing(at, mode) {
                return Ok(Some(message));
            }
        }
        Ok(None)
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

/// Moves the work tree, the index and HEAD's branch from HEAD's commit to `target`.
/// It refuses with git's wording over a local change or an untracked or ignored file.
/// A file a sparse checkout left out is no local change; it stays off disk.
pub(super) fn move_branch_to(
    repo: &git2::Repository,
    target: &git2::Commit<'_>,
    reflog: &str,
) -> Result<()> {
    let head = repo.head()?;
    let target_tree = target.tree()?;
    let diff = repo.diff_tree_to_tree(Some(&head.peel_to_tree()?), Some(&target_tree), None)?;
    let left_out = left_out_paths(repo)?;
    let mut changed = BTreeSet::new();
    for delta in diff.deltas() {
        changed.extend(delta.old_file().path_bytes().map(<[u8]>::to_vec));
        changed.extend(delta.new_file().path_bytes().map(<[u8]>::to_vec));
    }
    let (kept, written): (Vec<_>, Vec<_>) =
        changed.iter().partition(|path| left_out.contains(*path));
    if !written.is_empty() {
        check_out_safely(repo, target, &written)?;
    }
    update_left_out(repo, &target_tree, &kept)?;
    head.resolve()?.set_target(target.id(), reflog)?;
    if sparse::is_enabled(repo)? {
        sparse::reapply(repo)?;
    }
    Ok(())
}

/// Writes the `paths` of `target` into the work tree and the index, refusing, with nothing
/// changed, when that would overwrite a local change or an untracked or ignored file, or write a
/// path a required filter applies to.
fn check_out_safely(
    repo: &git2::Repository,
    target: &git2::Commit<'_>,
    paths: &[&Vec<u8>],
) -> Result<()> {
    let mut blocked = Vec::new();
    let refusal = Refusal::default();
    let mut outside_patterns = sparse::leaves_out(repo)?;
    let guard = filter::Guard::new(repo);
    let mut options = git2::build::CheckoutBuilder::new();
    options
        .safe()
        .overwrite_ignored(false)
        .disable_pathspec_match(true)
        .notify_on(
            git2::CheckoutNotificationType::CONFLICT | git2::CheckoutNotificationType::UPDATED,
        )
        .notify(|kind, path, _, target, _| {
            if kind.contains(git2::CheckoutNotificationType::UPDATED) {
                if !guard.any_required {
                    return true;
                }
                // The patterns leave this path out: it ends skip-worktree, so no filter applies.
                let skipped =
                    path.is_some_and(|path| outside_patterns(path.as_os_str().as_bytes()));
                return skipped || filter::may_write(&guard, &refusal, path, target.as_ref());
            }
            blocked.extend(path.map(Path::to_owned));
            true
        });
    for path in paths {
        options.path(path.as_slice());
    }
    let checked = repo.checkout_tree(target.as_object(), Some(&mut options));
    drop(options);
    if let Some(message) = refusal.take() {
        return Err(Error(message));
    }
    if checked.is_err() && !blocked.is_empty() {
        return Err(Error(overwrite_message(repo, &blocked)?));
    }
    Ok(checked?)
}

/// Gives the left-out `paths` of a sparse checkout the index content they have in `tree`.
/// They keep their skip-worktree mark; a path `tree` lacks loses its entry.
fn update_left_out(
    repo: &git2::Repository,
    tree: &git2::Tree<'_>,
    paths: &[&Vec<u8>],
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut index = repo.index()?;
    for path in paths {
        let at = Path::new(OsStr::from_bytes(path));
        let found = tree
            .get_path(at)
            .ok()
            .filter(|item| item.kind() != Some(git2::ObjectType::Tree));
        let Some(item) = found else {
            if index.get_path(at, 0).is_some() {
                index.remove_path(at)?;
            }
            continue;
        };
        let Some(old) = index.get_path(at, 0) else {
            continue;
        };
        let mut entry = copy_entry(&old);
        entry.id = item.id();
        entry.mode = item.filemode() as u32;
        index.add(&entry)?;
    }
    index.write()?;
    Ok(())
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
    Some(folded(&admin.join(text.trim_end())))
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
