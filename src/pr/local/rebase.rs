//! Rebasing a local branch onto the local default branch before it is fast-forwarded. It runs
//! only where nothing a rebase could damage is in the way, and a rebase that stops on conflicts
//! is aborted, so the branch is as it was.

use std::path::{Path, PathBuf};

use super::Local;
use crate::core::error::{Result, fail};
use crate::git::fast_forward::{check, moves_submodule};
use crate::git::{Repo, nul_fields};

/// What a rebase in progress leaves in the git directory of its work tree; so do a merge, a
/// cherry-pick, a revert and a bisect, which a rebase would trample.
const IN_PROGRESS: [&str; 8] = [
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "sequencer",
    "BISECT_LOG",
    "BISECT_START",
];

/// The directories that hold a rebase in progress.
const REBASE_DIRS: [&str; 2] = ["rebase-merge", "rebase-apply"];

impl Local<'_> {
    /// The work tree where `branch` can be rebased onto `target` (at `target_tip`; `tip` is the
    /// branch), or what stops it. The fast-forward that follows is checked first.
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
        let worktrees = self.repo.worktrees()?;
        let holders: Vec<_> = worktrees
            .iter()
            .filter(|entry| entry.branch.as_deref() == Some(branch))
            .collect();
        let [holder] = holders.as_slice() else {
            blockers.push(if holders.is_empty() {
                format!("{branch} is not checked out in any work tree, and a rebase needs one")
            } else {
                format!("{branch} is checked out in {} work trees", holders.len())
            });
            return Ok(Err(blockers));
        };
        if !holder.path.exists() {
            blockers.push(format!(
                "{branch} is checked out in {}, which no longer exists",
                holder.path.display()
            ));
            return Ok(Err(blockers));
        }
        let work = Repo::at(&holder.path);
        if work.strict_uncommitted_count()? > 0 {
            blockers.push(format!("{} has local changes", holder.path.display()));
        }
        let git_dir = work.git_dir()?;
        for name in IN_PROGRESS
            .iter()
            .filter(|name| git_dir.join(name).exists())
        {
            blockers.push(format!(
                "{} has a {name} in progress",
                holder.path.display()
            ));
        }
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
        let written = self.written_paths(&base, tip, target_tip)?;
        if let Some(path) = written.iter().find(|path| in_the_way(path, &ignored)) {
            blockers.push(format!(
                "the ignored file {path} sits where the rebase would write"
            ));
        }
        if blockers.is_empty() {
            Ok(Ok(holder.path.clone()))
        } else {
            Ok(Err(blockers))
        }
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
        let (code, error) =
            work.probe(&["rebase", "--no-autostash", "--no-update-refs", target_tip])?;
        if code == Some(0) {
            return self.tip(branch);
        }
        if !started_here(&work.git_dir()?, branch, target_tip, tip) {
            let now = self.tip(branch)?;
            return fail(format!("git rebase refused: {error}; {branch} is at {now}"));
        }
        let conflicts = work.git(&["diff", "--name-only", "--diff-filter=U", "-z"]);
        work.git(&["rebase", "--abort"])?;
        let paths: Vec<String> = nul_fields(conflicts?.as_bytes())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect();
        fail(format!(
            "rebasing {branch} onto {target} stops on conflicts in {}; the rebase was aborted and {branch} is still at {tip}",
            paths.join(", ")
        ))
    }

    /// The paths a rebase writes in the work tree: those it checks out from `target_tip` and
    /// those the branch's commits change.
    fn written_paths(&self, base: &str, tip: &str, target_tip: &str) -> Result<Vec<String>> {
        let mut paths = Vec::new();
        for (from, to) in [(tip, target_tip), (base, tip)] {
            let listed = self.repo.git(&["diff", "--name-only", "-z", from, to])?;
            paths.extend(
                nul_fields(listed.as_bytes())
                    .map(|path| String::from_utf8_lossy(path).into_owned()),
            );
        }
        Ok(paths)
    }
}

/// The ignored paths no commit tracks in `work`; a whole ignored directory is one entry ending
/// in a slash.
fn ignored_paths(work: &Repo) -> Result<Vec<String>> {
    let listed = work.git(&[
        "ls-files",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
        "-z",
    ])?;
    Ok(nul_fields(listed.as_bytes())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

/// Whether writing `path` would overwrite or block on an ignored entry.
fn in_the_way(path: &str, ignored: &[String]) -> bool {
    ignored.iter().any(|entry| {
        path == entry
            || (entry.ends_with('/') && path.starts_with(entry.as_str()))
            || path.starts_with(&format!("{entry}/"))
    })
}

/// Whether the rebase in `git_dir` is the one of `branch` onto `target_tip` from `tip`, which
/// this call started, and not one a user began.
fn started_here(git_dir: &Path, branch: &str, target_tip: &str, tip: &str) -> bool {
    REBASE_DIRS.iter().any(|dir| {
        let read = |name: &str| std::fs::read_to_string(git_dir.join(dir).join(name));
        let same = |name: &str, want: &str| read(name).is_ok_and(|text| text.trim() == want);
        same("head-name", &format!("refs/heads/{branch}"))
            && same("onto", target_tip)
            && same("orig-head", tip)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ignored_entry_is_in_the_way_of_its_own_path_and_those_beside_it() {
        let ignored = vec!["out.log".to_owned(), "build/".to_owned()];
        assert!(in_the_way("out.log", &ignored));
        assert!(in_the_way("build/x.o", &ignored));
        assert!(in_the_way("out.log/inner", &ignored));
        assert!(!in_the_way("out.logs", &ignored));
        assert!(!in_the_way("builder/x", &ignored));
    }

    #[test]
    fn only_a_rebase_of_this_branch_from_this_tip_onto_this_target_is_ours() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("rebase-merge");
        std::fs::create_dir(&state).unwrap();
        for (name, text) in [
            ("head-name", "refs/heads/feat\n"),
            ("onto", "target\n"),
            ("orig-head", "tip\n"),
        ] {
            std::fs::write(state.join(name), text).unwrap();
        }
        assert!(started_here(dir.path(), "feat", "target", "tip"));
        assert!(!started_here(dir.path(), "other", "target", "tip"));
        assert!(!started_here(dir.path(), "feat", "elsewhere", "tip"));
        assert!(!started_here(dir.path(), "feat", "target", "moved"));
        std::fs::remove_dir_all(&state).unwrap();
        assert!(!started_here(dir.path(), "feat", "target", "tip"));
    }
}
