//! Rebases a local branch onto the local default branch before it is fast-forwarded.
//! It runs only where nothing a rebase could damage is in the way.
//! A rebase that stops on conflicts is aborted, so the branch is as it was.

use std::path::{Path, PathBuf};

use super::Local;
use crate::core::error::{Result, fail};
use crate::git::fast_forward::{check, moves_submodule};
use crate::git::{Repo, error_line, nul_fields, unfinished_markers};

use crate::git::worktrees::{BranchOperation, REBASE_DIRS, rebase_record};

impl Local<'_> {
    /// The work tree where `branch` can be rebased onto `target`, or what stops it.
    /// `tip` and `target_tip` are the commits of `branch` and `target`.
    /// The fast-forward that follows is checked first, and the target's work tree last.
    pub(super) fn rebase_place(
        &self,
        branch: &str,
        target: &str,
        target_tip: &str,
        tip: &str,
    ) -> Result<std::result::Result<PathBuf, Vec<String>>> {
        let mut blockers = Vec::new();
        if let Some(refused) = check(self.repo, target, target_tip, tip)? {
            blockers.push(format!("{target} cannot be updated: {}", refused.reason));
        }
        let in_use = self.target_in_use(target)?;
        let target_busy = !in_use.is_empty();
        blockers.extend(in_use);
        let holders = self.holders(branch)?;
        let [holder] = holders.as_slice() else {
            blockers.push(if holders.is_empty() {
                format!("{branch} is not checked out in any work tree, and a rebase needs one")
            } else {
                format!("{branch} is checked out in {} work trees", holders.len())
            });
            return Ok(Err(blockers));
        };
        if !holder.exists() {
            blockers.push(format!(
                "{branch} is checked out in {}, which no longer exists",
                holder.display()
            ));
            return Ok(Err(blockers));
        }
        let work = Repo::at(holder);
        if work.strict_uncommitted_count()? > 0 {
            blockers.push(format!("{} has local changes", holder.display()));
        }
        blockers.extend(operations_in_progress(holder)?);
        let Some(base) = self.repo.merge_base(tip, target_tip)? else {
            blockers.push(format!("{branch} and {target} share no history"));
            return Ok(Err(blockers));
        };
        if moves_submodule(self.repo, &base, tip)? || moves_submodule(self.repo, &base, target_tip)?
        {
            blockers.push(
                "a submodule moves, and a rebase would leave it at its old commit".to_owned(),
            );
        }
        let ignored = ignored_paths(&work)?;
        let changed = self.changed_paths(&base, tip)?;
        let mut written = self.changed_paths(tip, target_tip)?;
        written.extend(self.replayed_paths(&base, tip)?);
        if let Some(path) = written.iter().find(|path| in_the_way(path, &ignored)) {
            blockers.push(format!(
                "the ignored file {path} sits where the rebase would write"
            ));
        }
        if !target_busy {
            blockers.extend(self.target_checkout(target, &changed)?);
        }
        if blockers.is_empty() {
            Ok(Ok(holder.clone()))
        } else {
            Ok(Err(blockers))
        }
    }

    /// One line for each work tree rebasing or bisecting `target`, also a deleted one.
    /// Git will not move a branch that such a work tree uses. The rebase MUST NOT start while one
    /// exists.
    fn target_in_use(&self, target: &str) -> Result<Vec<String>> {
        let mut found = Vec::new();
        for entry in self.repo.worktrees()? {
            let operation = match self.repo.branch_operation(&entry, target)? {
                Some(BranchOperation::Rebase) => "rebasing",
                Some(BranchOperation::Bisect) => "bisecting",
                None => continue,
            };
            let stale = if entry.path.exists() {
                ""
            } else {
                "; it no longer exists, and `git worktree prune` drops that entry"
            };
            found.push(format!(
                "{} is {operation} {target}, and git will not move {target} meanwhile{stale}",
                entry.path.display()
            ));
        }
        Ok(found)
    }

    /// The work trees that have `branch` checked out.
    /// A rebase detaches HEAD, so one rebasing `branch` counts too.
    fn holders(&self, branch: &str) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        for entry in self.repo.worktrees()? {
            let rebasing =
                self.repo.branch_operation(&entry, branch)? == Some(BranchOperation::Rebase);
            if rebasing || entry.branch.as_deref() == Some(branch) {
                found.push(entry.path);
            }
        }
        Ok(found)
    }

    /// Fails when `branch` is checked out in several work trees. A fast-forward would leave the
    /// others behind.
    /// It also fails when one has a rebase, merge, cherry-pick, revert or bisect in progress.
    pub(super) fn check_holders(&self, branch: &str) -> Result<()> {
        let holders = self.holders(branch)?;
        if holders.len() > 1 {
            let paths: Vec<String> = holders
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            return fail(format!(
                "{branch} is checked out in {} work trees ({}), and a merge would leave them in different states",
                holders.len(),
                paths.join(", ")
            ));
        }
        for path in holders.iter().filter(|path| path.exists()) {
            if let Some(blocker) = operations_in_progress(path)?.into_iter().next() {
                return fail(blocker);
            }
        }
        Ok(())
    }

    /// Rebases `branch` (at `tip`) onto `target` (at `target_tip`) and returns its new tip. When
    /// the rebase stops on conflicts it is aborted and the branch stays at `tip`.
    pub(super) fn rebase(
        &self,
        branch: &str,
        target: &str,
        target_tip: &str,
        tip: &str,
    ) -> Result<String> {
        let path = match self.rebase_place(branch, target, target_tip, tip)? {
            Ok(path) => path,
            Err(blockers) => {
                return fail(format!(
                    "cannot rebase {branch} onto {target}: {}",
                    blockers.join("; ")
                ));
            }
        };
        let work = Repo::at(path);
        // The `-c` form works on Git 2.31; `--no-update-refs` needs 2.38.
        let (code, error) = work.probe(&[
            "-c",
            "rebase.updateRefs=false",
            "rebase",
            "--no-autostash",
            target_tip,
        ])?;
        if code == Some(0) {
            let from = work.git(&["rev-parse", "ORIG_HEAD"])?;
            let now = self.tip(branch)?;
            if from != tip {
                return fail(format!(
                    "{branch} was at {from}, not {tip}, when the rebase started; it is at {now} and was not merged"
                ));
            }
            return Ok(now);
        }
        let error = error_line(&error);
        if !started_here(&work.git_dir()?, branch, target_tip, tip) {
            let now = self.tip(branch)?;
            return fail(format!("git rebase refused: {error}; {branch} is at {now}"));
        }
        let conflicts = work.git(&["diff", "--name-only", "--diff-filter=U", "-z"]);
        work.git(&["rebase", "--abort"])?;
        let paths = names(&conflicts?);
        let cause = if paths.is_empty() {
            format!("stops: {error}")
        } else {
            format!("stops on conflicts in {}", paths.join(", "))
        };
        fail(format!(
            "rebasing {branch} onto {target} {cause}; the rebase was aborted and {branch} is still at {tip}"
        ))
    }

    /// The paths each commit from `base` to `tip` touches.
    /// A rebase writes each commit in turn, so a path added then removed still counts.
    fn replayed_paths(&self, base: &str, tip: &str) -> Result<Vec<String>> {
        let range = format!("{base}..{tip}");
        let listed = self.repo.git(&[
            "log",
            "--no-renames",
            "--name-only",
            "--format=",
            "-z",
            "--end-of-options",
            &range,
        ])?;
        Ok(names(&listed))
    }

    /// The paths that differ between the commits `from` and `to`.
    fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let listed = self.repo.git(&["diff", "--name-only", "-z", from, to])?;
        Ok(names(&listed))
    }

    /// One line for each reason the target's work tree would refuse the fast-forward.
    /// It looks for an operation in progress. A local change, untracked file or ignored file at a
    /// `changed` path counts too.
    /// Empty when no work tree has `target`.
    fn target_checkout(&self, target: &str, changed: &[String]) -> Result<Vec<String>> {
        let holder = self
            .repo
            .worktrees()?
            .into_iter()
            .find(|entry| entry.branch.as_deref() == Some(target) && entry.path.exists());
        let Some(holder) = holder else {
            return Ok(Vec::new());
        };
        let work = Repo::at(&holder.path);
        let mut found = operations_in_progress(&holder.path)?;
        let kinds = [
            (
                "a local change",
                names(&work.git(&["diff", "--name-only", "-z", "HEAD"])?),
            ),
            (
                "an untracked file",
                names(&work.git(&["ls-files", "--others", "--exclude-standard", "-z"])?),
            ),
            ("an ignored file", ignored_paths(&work)?),
        ];
        for (kind, paths) in kinds {
            if let Some(path) = changed.iter().find(|path| in_the_way(path, &paths)) {
                found.push(format!(
                    "{} has {kind} at {path}, where the merge would write",
                    holder.path.display()
                ));
            }
        }
        Ok(found)
    }
}

/// One line for each operation in progress in the work tree at `path`.
fn operations_in_progress(path: &Path) -> Result<Vec<String>> {
    let git_dir = Repo::at(path).git_dir()?;
    Ok(unfinished_markers(&git_dir)
        .into_iter()
        .map(|name| format!("{} has a {name} in progress", path.display()))
        .collect())
}

/// The ignored paths no commit tracks in `work`. A whole ignored directory is one entry ending in a
/// slash.
fn ignored_paths(work: &Repo) -> Result<Vec<String>> {
    let listed = work.git(&[
        "ls-files",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
        "-z",
    ])?;
    Ok(names(&listed))
}

/// The paths in NUL-separated git output.
fn names(listed: &str) -> Vec<String> {
    nul_fields(listed.as_bytes())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect()
}

/// Whether writing `path` would overwrite, block on or remove an ignored entry.
/// A file written where a directory is removes every entry below it.
fn in_the_way(path: &str, ignored: &[String]) -> bool {
    ignored.iter().any(|entry| {
        let entry = entry.trim_end_matches('/');
        within(path, entry) || within(entry, path)
    })
}

/// Whether `path` is `directory` or lies below it.
fn within(path: &str, directory: &str) -> bool {
    path.strip_prefix(directory)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Whether the rebase in `git_dir` is of `branch` onto `target_tip` from `tip`.
/// That is the one this call started, not one a user began.
fn started_here(git_dir: &Path, branch: &str, target_tip: &str, tip: &str) -> bool {
    REBASE_DIRS.iter().any(|dir| {
        let same = |name: &str, want: &str| {
            rebase_record(git_dir, dir, name).is_some_and(|text| text == want)
        };
        same("head-name", &format!("refs/heads/{branch}"))
            && same("onto", target_tip)
            && same("orig-head", tip)
    })
}
