//! Which files trees, the index and the work tree hold or differ in, through libgit2. Paths are
//! bytes: git does not require a file name to be UTF-8.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::deep::{changed_since, is_too_deep};
use super::file_mode::file_mode;
use super::index::{Scope, UntrackedMode, left_out_paths, status_items};
use super::{Repo, folded, peeled_commit, peeled_tree};
use crate::core::error::{Error, Result};

/// One file that differs between two trees, or between a tree and the index.
pub struct TreeChange {
    pub path: Vec<u8>,
    pub old_mode: u32,
    pub new_mode: u32,
    pub new_id: String,
    /// `A`dded, `D`eleted, `M`odified or `T`ype changed.
    pub status: char,
}

impl Repo {
    /// The paths of every file of `tree` (a tree or commit), the submodules included, in tree
    /// order.
    pub fn tree_files(&self, tree: &str) -> Result<Vec<Vec<u8>>> {
        let repo = self.open_refs()?;
        let tree = peeled_tree(&repo, tree)?;
        let mut files = Vec::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() != Some(git2::ObjectType::Tree) {
                files.push([dir.as_bytes(), entry.name_bytes()].concat());
            }
            git2::TreeWalkResult::Ok
        })?;
        Ok(files)
    }

    /// The files that differ between two commits or trees, without rename detection.
    pub fn tree_changes(&self, from: &str, to: &str) -> Result<Vec<TreeChange>> {
        let repo = self.open_refs()?;
        let (old, new) = (peeled_tree(&repo, from)?, peeled_tree(&repo, to)?);
        let mut options = git2::DiffOptions::new();
        options.include_typechange(true);
        let diff = repo.diff_tree_to_tree(Some(&old), Some(&new), Some(&mut options))?;
        Ok(changes(&diff))
    }

    /// What is staged under `paths` (all of the repository when empty) against HEAD: the files
    /// added, modified or type changed. A repository without a commit compares to nothing.
    pub fn staged_changes(&self, paths: &[&Path]) -> Result<Vec<TreeChange>> {
        let repo = self.open()?;
        let wanted = self.under_any(&repo, paths)?;
        let diff = staged_diff(&repo)?;
        Ok(changes(&diff)
            .into_iter()
            .filter(|change| matches!(change.status, 'A' | 'M' | 'T') && wanted(&change.path))
            .collect())
    }

    /// Whether the index, or what is staged against HEAD, has anything under `path`.
    pub fn has_staged_path(&self, path: &Path) -> Result<bool> {
        let repo = self.open()?;
        let wanted = self.under_any(&repo, &[path])?;
        let index = repo.index()?;
        let tracked = index.iter().any(|entry| wanted(&entry.path));
        Ok(tracked || staged_anything(&repo, &wanted)?)
    }

    /// The tracked files under `paths` and the untracked ones git does not ignore, sorted.
    pub fn tracked_and_untracked(&self, paths: &[&Path]) -> Result<Vec<Vec<u8>>> {
        let repo = self.open()?;
        let wanted = self.under_any(&repo, paths)?;
        let index = repo.index()?;
        let mut found: BTreeSet<Vec<u8>> = index
            .iter()
            .filter(|entry| wanted(&entry.path))
            .map(|entry| entry.path)
            .collect();
        found.extend(untracked_in(&repo)?.into_iter().filter(|path| wanted(path)));
        Ok(found.into_iter().collect())
    }

    /// The content of blob `id`.
    pub fn blob_content(&self, id: &str) -> Result<Vec<u8>> {
        let repo = self.open_refs()?;
        Ok(repo.find_blob(git2::Oid::from_str(id)?)?.content().to_vec())
    }

    /// The files that differ between HEAD and the work tree, staged or not, as `git diff HEAD`.
    pub fn changed_since_head(&self) -> Result<Vec<Vec<u8>>> {
        let repo = self.open()?;
        let head = peeled_tree(&repo, "HEAD")?;
        let changed = match repo.diff_tree_to_workdir_with_index(Some(&head), None) {
            Ok(diff) => changes(&diff)
                .into_iter()
                .map(|change| change.path)
                .collect(),
            Err(error) if is_too_deep(&error) => changed_since(&repo, &head)?,
            Err(error) => return Err(error.into()),
        };
        let left_out = left_out_paths(&repo)?;
        Ok(changed
            .into_iter()
            .filter(|path| !left_out.contains(path))
            .collect())
    }

    /// The untracked files that git does not ignore, one entry per file.
    pub fn untracked_files(&self) -> Result<Vec<Vec<u8>>> {
        untracked_in(&*self.open()?)
    }

    /// The ignored paths no commit tracks, as `git ls-files --others --ignored --directory`
    /// lists them. A directory the ignore rules name is one entry ending in a slash.
    /// So is an untracked directory holding only ignored paths, followed by those paths.
    pub fn ignored_entries(&self) -> Result<Vec<Vec<u8>>> {
        let repo = self.open()?;
        let mut ignored = BTreeSet::new();
        let mut occupied = BTreeSet::new();
        for item in status_items(&repo, UNTRACKED_AND_IGNORED)? {
            let path = item.path;
            if item.status.is_ignored() && !is_nested_repo(&repo, &path) {
                ignored.insert(path);
            } else {
                occupied.extend(ancestor_dirs(&path));
            }
        }
        for entry in repo.index()?.iter() {
            occupied.extend(ancestor_dirs(&entry.path));
        }
        let collapsed: BTreeSet<Vec<u8>> = ignored
            .iter()
            .flat_map(|path| ancestor_dirs(path))
            .filter(|dir| !occupied.contains(dir))
            .collect();
        ignored.extend(collapsed);
        Ok(ignored.into_iter().collect())
    }

    /// The files that differ between commits `from` and `to`, as `git diff --name-only` lists
    /// them: a renamed file by its new name.
    pub fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<Vec<u8>>> {
        let repo = self.open_refs()?;
        let (old, new) = (peeled_tree(&repo, from)?, peeled_tree(&repo, to)?);
        let mut diff = repo.diff_tree_to_tree(Some(&old), Some(&new), None)?;
        diff.find_similar(Some(git2::DiffFindOptions::new().renames(true)))?;
        Ok(changes(&diff)
            .into_iter()
            .map(|change| change.path)
            .collect())
    }

    /// The paths each non-merge commit of `base..tip` touches, listed once per commit.
    pub fn touched_paths(&self, base: &str, tip: &str) -> Result<Vec<Vec<u8>>> {
        let repo = self.open_refs()?;
        let mut paths = Vec::new();
        for id in self.walk(&repo, Some(base), tip)? {
            let commit = repo.find_commit(id?)?;
            if commit.parent_count() > 1 {
                continue;
            }
            let parent = commit
                .parents()
                .next()
                .map(|parent| parent.tree())
                .transpose()?;
            let diff = repo.diff_tree_to_tree(parent.as_ref(), Some(&commit.tree()?), None)?;
            paths.extend(changes(&diff).into_iter().map(|change| change.path));
        }
        Ok(paths)
    }

    /// A test for the files named by `paths`, or below them (every file when there are none).
    /// A path is taken literally: `[`, `*` and `?` in it match themselves.
    fn under_any(
        &self,
        repo: &git2::Repository,
        paths: &[&Path],
    ) -> Result<impl Fn(&[u8]) -> bool + use<>> {
        let roots: Vec<PathBuf> = paths
            .iter()
            .map(|path| self.within_repository(repo, path))
            .collect::<Result<_>>()?;
        Ok(move |file: &[u8]| {
            let file = Path::new(OsStr::from_bytes(file));
            roots.is_empty() || roots.iter().any(|root| file.starts_with(root))
        })
    }

    /// `path` relative to the work tree, with `.` and `..` folded away; an absolute path
    /// outside it is an error.
    fn within_repository(
        &self,
        repo: &git2::Repository,
        path: &Path,
    ) -> Result<std::path::PathBuf> {
        if !path.is_absolute() {
            return Ok(path.to_owned());
        }
        let path = &folded(path);
        let workdir = repo
            .workdir()
            .ok_or_else(|| Error("this operation must be run in a work tree".into()))?;
        let root = std::fs::canonicalize(workdir).unwrap_or_else(|_| workdir.to_owned());
        path.strip_prefix(&root)
            .or_else(|_| path.strip_prefix(workdir))
            .map(Path::to_owned)
            .map_err(|_| Error(format!("{} is outside repository", path.display())))
    }
}

/// What is staged against HEAD, as a diff of HEAD's tree (none before the first commit) and
/// the index.
pub(super) fn staged_diff(repo: &git2::Repository) -> Result<git2::Diff<'_>> {
    let head = match peeled_commit(repo, "HEAD")? {
        Some(head) => Some(peeled_tree(repo, &head)?),
        None => None,
    };
    let mut options = git2::DiffOptions::new();
    options.include_typechange(true);
    Ok(repo.diff_tree_to_index(head.as_ref(), None, Some(&mut options))?)
}

/// Whether anything `wanted` is staged against HEAD, a deletion included.
fn staged_anything(repo: &git2::Repository, wanted: &impl Fn(&[u8]) -> bool) -> Result<bool> {
    let Some(head) = peeled_commit(repo, "HEAD")? else {
        return Ok(false);
    };
    let tree = peeled_tree(repo, &head)?;
    let diff = repo.diff_tree_to_index(Some(&tree), None, None)?;
    Ok(diff.deltas().any(|delta| {
        [delta.old_file(), delta.new_file()]
            .iter()
            .filter_map(|file| file.path_bytes())
            .any(wanted)
    }))
}

fn changes(diff: &git2::Diff<'_>) -> Vec<TreeChange> {
    diff.deltas()
        .filter_map(|delta| {
            let status = match delta.status() {
                git2::Delta::Added => 'A',
                git2::Delta::Deleted => 'D',
                git2::Delta::Modified => 'M',
                git2::Delta::Typechange => 'T',
                git2::Delta::Renamed => 'M',
                _ => return None,
            };
            Some(TreeChange {
                path: delta.new_file().path_bytes()?.to_vec(),
                old_mode: file_mode(&delta.old_file()),
                new_mode: file_mode(&delta.new_file()),
                new_id: delta.new_file().id().to_string(),
                status,
            })
        })
        .collect()
}

/// The status listing of the untracked and ignored files, each file of an untracked directory
/// on its own.
const UNTRACKED_AND_IGNORED: Scope = Scope {
    untracked: UntrackedMode::All,
    ignored: true,
    staged: true,
    renames: false,
    submodules: true,
};

fn untracked_in(repo: &git2::Repository) -> Result<Vec<Vec<u8>>> {
    Ok(status_items(repo, UNTRACKED_AND_IGNORED)?
        .into_iter()
        .filter(|item| {
            (item.status.is_wt_new() && !item.status.is_index_new())
                || (item.status.is_ignored() && is_nested_repo(repo, &item.path))
        })
        .map(|item| item.path)
        .collect())
}

/// The untracked directories (with their trailing slash) that hold a repository of their own,
/// which `git add` stages as gitlinks.
pub(super) fn nested_repos(repo: &git2::Repository) -> Result<Vec<Vec<u8>>> {
    Ok(untracked_in(repo)?
        .into_iter()
        .filter(|path| is_nested_repo(repo, path))
        .collect())
}

/// The directories above `path`, each with its trailing slash, from the outermost down.
pub(super) fn ancestor_dirs(path: &[u8]) -> Vec<Vec<u8>> {
    path.iter()
        .enumerate()
        .filter(|(at, byte)| **byte == b'/' && *at + 1 < path.len())
        .map(|(at, _)| path[..=at].to_vec())
        .collect()
}

/// Whether status entry `path` is a nested repository that no ignore rule names.
/// libgit2 reports it as ignored when it holds nothing else; git lists it as untracked.
pub(super) fn is_nested_repo(repo: &git2::Repository, path: &[u8]) -> bool {
    let Some(dir) = path.strip_suffix(b"/") else {
        return false;
    };
    let Some(workdir) = repo.workdir() else {
        return false;
    };
    let nested = workdir.join(OsStr::from_bytes(dir));
    nested.join(".git").exists() && !repo.is_path_ignored(&nested).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::commit;
    use crate::core::testing::Sandbox;
    use crate::git::Repo;

    /// The commit on top of `HEAD` whose tree holds `name` with the raw `mode` (git and libgit2
    /// write only canonical modes, so the tree object is written byte by byte).
    fn commit_with_raw_mode(root: &std::path::Path, name: &str, mode: &str) -> String {
        let repo = git2::Repository::open(root).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let blob = repo.blob(b"content\n").unwrap();
        let mut raw = format!("{mode} {name}\0").into_bytes();
        raw.extend(blob.as_bytes());
        let tree = repo
            .odb()
            .unwrap()
            .write(git2::ObjectType::Tree, &raw)
            .unwrap();
        let signature = git2::Signature::now("n", "n@example.com").unwrap();
        let tree = repo.find_tree(tree).unwrap();
        repo.commit(None, &signature, &signature, "raw", &tree, &[&head])
            .unwrap()
            .to_string()
    }

    #[test]
    fn a_legacy_mode_regular_file_reads_as_git_reads_it() {
        for (raw, expected) in [
            ("100775", 0o100_755),
            ("100755", 0o100_755),
            ("100664", 0o100_644),
            ("100600", 0o100_644),
            ("120755", 0o120_000),
        ] {
            let sandbox = Sandbox::new();
            let root = sandbox.repo();
            sandbox.git(&root, &["init", "-q", "-b", "main"]);
            commit(&sandbox, &root, &[("a.txt", "a\n")], "first");
            let legacy = commit_with_raw_mode(&root, "script", raw);

            let changes = Repo::at(&root).tree_changes("HEAD", &legacy).unwrap();

            let change = changes.iter().find(|change| change.path == b"script");
            let change = change.unwrap_or_else(|| panic!("script is not a change for {raw}"));
            assert_eq!((change.old_mode, change.new_mode), (0, expected), "{raw}");
        }
    }
}
