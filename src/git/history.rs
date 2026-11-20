//! Commit history, diffs, patch-ids and merges, read through libgit2.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::index::{STAGE_MASK, conflict_path, copy_entry};
use super::patch_id;
use super::{CommitInfo, LogEntry, Repo, commit_id, peeled_commit, peeled_tree, subject_of};
use crate::core::error::{Error, Result, fail};

/// A commit of the branch that `git cherry` lists.
pub struct Cherry {
    pub sha: String,
    /// The upstream has a commit with the same patch-id.
    pub in_upstream: bool,
}

impl Repo {
    /// The non-merge commits of `head` that `upstream` lacks, oldest first (`git cherry`), each
    /// marked when the upstream has a commit with the same patch-id.
    pub fn cherry(&self, upstream: &str, head: &str) -> Result<Vec<Cherry>> {
        let repo = self.open_refs()?;
        let mut upstream_ids = HashSet::new();
        for id in self.walk(&repo, Some(head), upstream)? {
            let commit = repo.find_commit(id?)?;
            if commit.parent_count() <= 1 {
                upstream_ids.insert(commit_patch_id(&repo, &commit)?);
            }
        }
        let mut found = Vec::new();
        for id in self.listed(&repo, Some(upstream), head, false)? {
            let commit = repo.find_commit(id)?;
            if commit.parent_count() > 1 {
                continue;
            }
            found.push(Cherry {
                sha: id.to_string(),
                in_upstream: upstream_ids.contains(&commit_patch_id(&repo, &commit)?),
            });
        }
        Ok(found)
    }

    /// Non-merge commits in `from..to` (or all of `to` when `from` is `None`), oldest first.
    pub fn commits(&self, from: Option<&str>, to: &str) -> Result<Vec<CommitInfo>> {
        self.commits_with(from, to, false)
    }

    /// Like [`Repo::commits`], following only the first parent of each merge, so commits a merge
    /// brought in are left out.
    pub fn commits_first_parent(&self, from: Option<&str>, to: &str) -> Result<Vec<CommitInfo>> {
        self.commits_with(from, to, true)
    }

    fn commits_with(
        &self,
        from: Option<&str>,
        to: &str,
        first_parent: bool,
    ) -> Result<Vec<CommitInfo>> {
        let repo = self.open_refs()?;
        let mut found = Vec::new();
        for id in self.listed(&repo, from, to, first_parent)? {
            let commit = repo.find_commit(id)?;
            if commit.parent_count() > 1 {
                continue;
            }
            found.push(CommitInfo {
                sha: id.to_string(),
                tree: commit.tree_id().to_string(),
                subject: subject_of(commit.message_bytes()),
                patch_id: commit_patch_id(&repo, &commit)?,
            });
        }
        Ok(found)
    }

    /// The ids of the commits of `from..to`, oldest first, parents before children.
    fn listed(
        &self,
        repo: &git2::Repository,
        from: Option<&str>,
        to: &str,
        first_parent: bool,
    ) -> Result<Vec<git2::Oid>> {
        super::reject_bad_config(repo)?;
        let mut walk = self.walk(repo, from, to)?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
        if first_parent {
            walk.simplify_first_parent()?;
        }
        Ok(walk.collect::<std::result::Result<_, _>>()?)
    }

    /// The patch-id of what a commit changes against its parent (all of it for a root commit);
    /// `None` for a merge or a commit that changes nothing.
    pub fn commit_patch_id(&self, commit: &str) -> Result<Option<String>> {
        let repo = self.open_refs()?;
        let found = repo.find_commit(commit_id(&repo, commit)?)?;
        if found.parent_count() > 1 {
            return Ok(None);
        }
        commit_patch_id(&repo, &found)
    }

    /// How many commits `commit` has that `other` lacks, and how many `other` has that
    /// `commit` lacks.
    pub fn ahead_behind(&self, commit: &str, other: &str) -> Result<(usize, usize)> {
        let repo = self.open_refs()?;
        Ok(repo.graph_ahead_behind(commit_id(&repo, commit)?, commit_id(&repo, other)?)?)
    }

    /// Every commit in `from..to`, merges included, oldest first.
    pub fn log(&self, from: Option<&str>, to: &str) -> Result<Vec<LogEntry>> {
        self.log_with(from, to, false)
    }

    fn log_with(&self, from: Option<&str>, to: &str, first_parent: bool) -> Result<Vec<LogEntry>> {
        let repo = self.open_refs()?;
        let mut entries = Vec::new();
        for id in self.listed(&repo, from, to, first_parent)? {
            let commit = repo.find_commit(id)?;
            entries.push(LogEntry {
                sha: id.to_string(),
                tree: commit.tree_id().to_string(),
                parents: commit
                    .parent_ids()
                    .map(|parent| parent.to_string())
                    .collect(),
                subject: subject_of(commit.message_bytes()),
            });
        }
        Ok(entries)
    }

    /// The author name and subject of each commit of `shas` that exists; others are skipped.
    pub fn authored(&self, shas: &[&str]) -> Result<HashMap<String, (String, String)>> {
        let repo = self.open_refs()?;
        super::reject_bad_config(&repo)?;
        let mut found = HashMap::new();
        for sha in shas {
            let Some(id) = peeled_commit(&repo, sha)? else {
                continue;
            };
            let commit = repo.find_commit(git2::Oid::from_str(&id)?)?;
            let author = String::from_utf8_lossy(commit.author().name_bytes()).into_owned();
            found.insert(id, (author, subject_of(commit.message_bytes())));
        }
        Ok(found)
    }

    /// The merge commits in `from..to`.
    pub fn merges(&self, from: Option<&str>, to: &str) -> Result<HashSet<String>> {
        let repo = self.open_refs()?;
        super::reject_bad_config(&repo)?;
        let mut merges = HashSet::new();
        for id in self.walk(&repo, from, to)? {
            let id = id?;
            if repo.find_commit(id)?.parent_count() > 1 {
                merges.insert(id.to_string());
            }
        }
        Ok(merges)
    }

    /// The tree of git's automatic merge of `ours` and `theirs`, conflict markers included;
    /// `None` when the two share no history. It writes objects, so it only runs on a read-only
    /// repository.
    pub fn merge_tree(&self, ours: &str, theirs: &str) -> Result<Option<String>> {
        self.merged(ours, theirs, |_, tree, _| Ok(tree))
    }

    /// The paths git cannot merge when it merges `theirs` into `ours`; empty for a clean merge. An
    /// error when the two share no history.
    pub fn merge_conflicts(&self, ours: &str, theirs: &str) -> Result<Vec<String>> {
        self.merged(ours, theirs, |_, _, paths| Ok(paths))?
            .ok_or_else(|| {
                Error("git merge-tree failed: fatal: refusing to merge unrelated histories".into())
            })
    }

    fn merged<T>(
        &self,
        ours: &str,
        theirs: &str,
        finish: impl FnOnce(&git2::Repository, String, Vec<String>) -> Result<T>,
    ) -> Result<Option<T>> {
        if self.held.is_none() {
            return fail("merge-tree needs a read-only repository");
        }
        let repo = self.open_refs()?;
        let (ours, theirs) = (commit_id(&repo, ours)?, commit_id(&repo, theirs)?);
        if repo.merge_base(ours, theirs).is_err() {
            return Ok(None);
        }
        let mut index =
            repo.merge_commits(&repo.find_commit(ours)?, &repo.find_commit(theirs)?, None)?;
        let mut paths = Vec::new();
        if index.has_conflicts() {
            let conflicts: Vec<git2::IndexConflict> =
                index.conflicts()?.collect::<std::result::Result<_, _>>()?;
            for conflict in conflicts {
                paths.push(settle(&repo, &mut index, &conflict)?);
            }
        }
        paths.dedup();
        let tree = index.write_tree_to(&repo)?.to_string();
        finish(&repo, tree, paths).map(Some)
    }

    /// Patch-id of the whole change from `from` (empty tree when `None`) to a commit or tree.
    pub fn range_patch_id(&self, from: Option<&str>, to: &str) -> Result<Option<String>> {
        let repo = self.open_refs()?;
        let old = from.map(|from| peeled_tree(&repo, from)).transpose()?;
        patch_id(&repo, old.as_ref(), &peeled_tree(&repo, to)?)
    }

    /// `git diff -U3 -M`, the context forges show around review comments.
    pub fn diff_context(&self, from: &str, to: &str) -> Result<String> {
        self.diff(3, from, to)
    }

    pub fn diff_u0(&self, from: &str, to: &str) -> Result<String> {
        self.diff(0, from, to)
    }

    fn diff(&self, context: u32, from: &str, to: &str) -> Result<String> {
        let repo = self.open_refs()?;
        let (old, new) = (peeled_tree(&repo, from)?, peeled_tree(&repo, to)?);
        let mut diff =
            repo.diff_tree_to_tree(Some(&old), Some(&new), Some(&mut diff_options(context)))?;
        diff.find_similar(Some(git2::DiffFindOptions::new().renames(true)))?;
        Ok(String::from_utf8_lossy(&patch_id::patch_text(&diff)?).into_owned())
    }
}

/// The patch-id of what a non-merge `commit` changes against its parent.
fn commit_patch_id(repo: &git2::Repository, commit: &git2::Commit<'_>) -> Result<Option<String>> {
    let parent = commit
        .parents()
        .next()
        .map(|parent| parent.tree())
        .transpose()?;
    patch_id(repo, parent.as_ref(), &commit.tree()?)
}

/// Diff options independent of the user's diff config, so patch-ids and anchors stay stable.
/// Rename detection is off here; `diff` turns it on for anchors.
fn diff_options(context: u32) -> git2::DiffOptions {
    let mut options = git2::DiffOptions::new();
    options
        .context_lines(context)
        .indent_heuristic(true)
        .old_prefix("a/")
        .new_prefix("b/");
    options
}

/// The patch-id of the change from `old` (the empty tree when `None`) to `new`, `None` when
/// there is no change.
fn patch_id(
    repo: &git2::Repository,
    old: Option<&git2::Tree<'_>>,
    new: &git2::Tree<'_>,
) -> Result<Option<String>> {
    let diff = repo.diff_tree_to_tree(old, Some(new), Some(&mut diff_options(3)))?;
    if diff.deltas().len() == 0 {
        return Ok(None);
    }
    Ok(Some(patch_id::stable(&diff)?))
}

/// Puts the conflicted path into the merged index, as `git merge-tree --write-tree` does.
/// It returns the path. Content conflicts keep their markers; other conflicts take the side
/// with the file.
fn settle(
    repo: &git2::Repository,
    index: &mut git2::Index,
    conflict: &git2::IndexConflict,
) -> Result<String> {
    let Some(path) = conflict_path(conflict) else {
        return fail("a merge conflict names no path");
    };
    let text = String::from_utf8_lossy(path).into_owned();
    index.conflict_remove(Path::new(OsStr::from_bytes(path)))?;
    let (Some(our), Some(their)) = (&conflict.our, &conflict.their) else {
        if let Some(kept) = side_entry(conflict) {
            index.add(&kept)?;
        }
        return Ok(text);
    };
    let ancestor = match &conflict.ancestor {
        Some(ancestor) => copy_entry(ancestor),
        None => {
            let mut empty = copy_entry(our);
            empty.id = repo.blob(b"")?;
            empty
        }
    };
    let merged = repo.merge_file_from_index(&ancestor, our, their, None)?;
    let id = repo.blob(merged.content())?;
    let mut entry = copy_entry(our);
    entry.id = id;
    entry.flags &= !STAGE_MASK;
    index.add(&entry)?;
    Ok(text)
}

/// The side of a conflict that still has the file, as an entry at stage 0.
fn side_entry(conflict: &git2::IndexConflict) -> Option<git2::IndexEntry> {
    let mut entry = copy_entry(conflict.our.as_ref().or(conflict.their.as_ref())?);
    entry.flags &= !STAGE_MASK;
    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{commit, rev};
    use crate::core::testing::Sandbox;
    use crate::git::Repo;

    /// `git cherry`'s lines, as the sha and whether the upstream has an equivalent.
    fn cli_cherry(sandbox: &Sandbox, upstream: &str, head: &str) -> Vec<(String, bool)> {
        let dir = sandbox.repo();
        sandbox
            .git_output(&dir, &["cherry", upstream, head])
            .lines()
            .map(|line| (line[2..].to_owned(), line.starts_with('-')))
            .collect()
    }

    #[test]
    fn cherry_marks_the_commits_an_upstream_has_in_the_order_git_cherry_prints_them() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        commit(&sandbox, &dir, &[("a.txt", "1\n2\n3\n")], "base");
        sandbox.git(&dir, &["switch", "-q", "-c", "up"]);
        commit(&sandbox, &dir, &[("u1.txt", "u1\n")], "up one");
        let picked = rev(&sandbox, &dir, "HEAD");
        commit(&sandbox, &dir, &[("a.txt", "1\n2\n3\n4\n")], "up two");
        sandbox.git(&dir, &["switch", "-q", "-c", "topic", "main"]);
        commit(&sandbox, &dir, &[("t0.txt", "t0\n")], "topic first");
        sandbox.git(&dir, &["cherry-pick", &picked]);
        commit(&sandbox, &dir, &[("t2.txt", "t2\n")], "topic third");
        // The same lines as "up two" on a different parent and message.
        commit(&sandbox, &dir, &[("a.txt", "1\n2\n3\n4\n")], "topic fourth");

        let repo = Repo::at(&dir);
        let ours: Vec<(String, bool)> = repo
            .cherry("up", "topic")
            .unwrap()
            .into_iter()
            .map(|entry| (entry.sha, entry.in_upstream))
            .collect();

        let expected = cli_cherry(&sandbox, "up", "topic");
        assert_eq!(expected.len(), 4);
        assert!(expected.iter().any(|(_, in_upstream)| *in_upstream));
        assert!(expected.iter().any(|(_, in_upstream)| !*in_upstream));
        assert_eq!(ours, expected);
    }

    /// The patch-id of `git show` of `rev` (the whole tree for a root commit), by the command
    /// line.
    fn cli_patch_id(sandbox: &Sandbox, rev: &str) -> Option<String> {
        let script = format!("git diff-tree --root -p {rev} | git patch-id --stable");
        let printed = sandbox.shell_output(&sandbox.repo(), &script);
        printed.split_whitespace().next().map(str::to_owned)
    }

    #[test]
    fn patch_ids_match_git_patch_id_stable() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        commit(
            &sandbox,
            &dir,
            &[("a.txt", "1\n2\n3\n"), ("dir/b.txt", "b\n")],
            "root",
        );
        commit(
            &sandbox,
            &dir,
            &[("a.txt", "1\n2 changed\n3\n"), ("c.txt", "c\n")],
            "edit and add",
        );
        sandbox.git(&dir, &["rm", "-q", "dir/b.txt"]);
        sandbox.git(&dir, &["commit", "-q", "-m", "delete"]);
        sandbox.git(&dir, &["mv", "c.txt", "moved.txt"]);
        sandbox.git(&dir, &["commit", "-q", "-m", "rename"]);
        sandbox.git(&dir, &["update-index", "--chmod=+x", "a.txt"]);
        sandbox.git(&dir, &["commit", "-q", "-m", "mode"]);
        sandbox.git(&dir, &["commit", "-q", "--allow-empty", "-m", "nothing"]);
        // A diff config that changes the paths the library prints must not change the ids.
        sandbox.git(&dir, &["config", "diff.mnemonicPrefix", "true"]);
        let repo = Repo::at(&dir);

        for step in ["HEAD~5", "HEAD~4", "HEAD~3", "HEAD~2", "HEAD~1", "HEAD"] {
            assert_eq!(
                repo.commit_patch_id(step).unwrap(),
                cli_patch_id(&sandbox, step),
                "{step}"
            );
        }
        let whole = sandbox.shell_output(
            &dir,
            "git diff --no-renames HEAD~4 HEAD~2 | git patch-id --stable",
        );
        assert_eq!(
            repo.range_patch_id(Some("HEAD~4"), "HEAD~2")
                .unwrap()
                .as_deref(),
            whole.split_whitespace().next()
        );
    }

    #[test]
    fn patch_ids_match_git_for_a_mode_only_file_next_to_others_and_for_odd_files() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        commit(
            &sandbox,
            &dir,
            &[
                ("a.txt", "1\n2\n"),
                ("m.sh", "echo m\n"),
                ("z.txt", "z\n"),
                ("no-newline.txt", "end"),
            ],
            "root",
        );
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 3, 0]).unwrap();
        std::fs::write(dir.join("a.txt"), "1\n2 changed\n").unwrap();
        std::fs::write(dir.join("z.txt"), "z changed\n").unwrap();
        std::fs::write(dir.join("no-newline.txt"), "end changed").unwrap();
        sandbox.git(&dir, &["add", "-A"]);
        sandbox.git(&dir, &["update-index", "--chmod=+x", "m.sh"]);
        sandbox.git(&dir, &["commit", "-q", "-m", "mode next to edits"]);
        sandbox.git(&dir, &["update-index", "--chmod=-x", "m.sh"]);
        sandbox.git(&dir, &["update-index", "--chmod=+x", "z.txt"]);
        sandbox.git(&dir, &["commit", "-q", "-m", "two modes"]);
        sandbox.git(&dir, &["update-index", "--chmod=-x", "z.txt"]);
        commit(&sandbox, &dir, &[("a.txt", "again\n")], "mode then edit");
        std::fs::write(dir.join("bin.dat"), [9u8, 1, 2, 3, 0]).unwrap();
        commit(&sandbox, &dir, &[], "binary edit");
        let repo = Repo::at(&dir);

        for step in ["HEAD~3", "HEAD~2", "HEAD~1", "HEAD"] {
            assert_eq!(
                repo.commit_patch_id(step).unwrap(),
                cli_patch_id(&sandbox, step),
                "{step}"
            );
        }
        let whole = sandbox.shell_output(
            &dir,
            "git diff --no-renames HEAD~3 HEAD | git patch-id --stable",
        );
        assert_eq!(
            repo.range_patch_id(Some("HEAD~3"), "HEAD")
                .unwrap()
                .as_deref(),
            whole.split_whitespace().next()
        );
    }
}
