//! Which files trees, the index and the work tree hold or differ in, through libgit2. Paths are
//! bytes: git does not require a file name to be UTF-8.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::{Repo, commit_id, peeled_commit};
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
        let repo = self.open()?;
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
        let repo = self.open()?;
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
        let head = match peeled_commit(&repo, "HEAD")? {
            Some(head) => Some(peeled_tree(&repo, &head)?),
            None => None,
        };
        let mut options = git2::DiffOptions::new();
        options.include_typechange(true);
        for path in paths {
            options.pathspec(self.within_repository(&repo, path)?);
        }
        let diff = repo.diff_tree_to_index(head.as_ref(), None, Some(&mut options))?;
        Ok(changes(&diff)
            .into_iter()
            .filter(|change| matches!(change.status, 'A' | 'M' | 'T'))
            .collect())
    }

    /// Whether the index, or what is staged against HEAD, has anything under `path`.
    pub fn has_staged_path(&self, path: &Path) -> Result<bool> {
        let repo = self.open()?;
        let relative = self.within_repository(&repo, path)?;
        let index = repo.index()?;
        let spec = git2::Pathspec::new([relative.as_os_str()])?;
        let tracked = index.iter().any(|entry| {
            spec.matches_path(
                Path::new(OsStr::from_bytes(&entry.path)),
                git2::PathspecFlags::DEFAULT,
            )
        });
        Ok(tracked || staged_anything(&repo, &relative)?)
    }

    /// The tracked files under `paths` and the untracked ones git does not ignore, sorted.
    pub fn tracked_and_untracked(&self, paths: &[&Path]) -> Result<Vec<Vec<u8>>> {
        let repo = self.open()?;
        let specs: Vec<_> = paths
            .iter()
            .map(|path| self.within_repository(&repo, path))
            .collect::<Result<_>>()?;
        let spec = if specs.is_empty() {
            None
        } else {
            Some(git2::Pathspec::new(
                specs.iter().map(|path| path.as_os_str()),
            )?)
        };
        let wanted = |path: &[u8]| {
            spec.as_ref().is_none_or(|spec| {
                spec.matches_path(
                    Path::new(OsStr::from_bytes(path)),
                    git2::PathspecFlags::DEFAULT,
                )
            })
        };
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
        let repo = self.open()?;
        Ok(repo.find_blob(git2::Oid::from_str(id)?)?.content().to_vec())
    }

    /// The files that differ between HEAD and the work tree, staged or not, as `git diff HEAD`.
    pub fn changed_since_head(&self) -> Result<Vec<Vec<u8>>> {
        let repo = self.open()?;
        let head = peeled_tree(&repo, "HEAD")?;
        let diff = repo.diff_tree_to_workdir_with_index(Some(&head), None)?;
        Ok(changes(&diff)
            .into_iter()
            .map(|change| change.path)
            .collect())
    }

    /// The untracked files that git does not ignore, one entry per file.
    pub fn untracked_files(&self) -> Result<Vec<Vec<u8>>> {
        untracked_in(&*self.open()?)
    }

    /// The ignored paths no commit tracks, as `git ls-files --others --ignored --directory`
    /// lists them: a directory the ignore rules name is one entry ending in a slash, and so is
    /// an untracked directory that holds nothing else than ignored paths, followed by those
    /// paths.
    pub fn ignored_entries(&self) -> Result<Vec<Vec<u8>>> {
        let repo = self.open()?;
        let mut options = git2::StatusOptions::new();
        options
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(true)
            .recurse_ignored_dirs(false);
        let statuses = repo.statuses(Some(&mut options))?;
        let mut ignored = BTreeSet::new();
        let mut occupied = BTreeSet::new();
        for entry in statuses.iter() {
            let path = entry.path_bytes().to_vec();
            if entry.status().is_ignored() && !is_nested_repo(&repo, &path) {
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
        let repo = self.open()?;
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
        let repo = self.open()?;
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

    /// `path` relative to the work tree, which libgit2 pathspecs require; an absolute path
    /// outside it is an error.
    fn within_repository(
        &self,
        repo: &git2::Repository,
        path: &Path,
    ) -> Result<std::path::PathBuf> {
        if !path.is_absolute() {
            return Ok(path.to_owned());
        }
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

/// Whether anything under `relative` is staged against HEAD, a deletion included.
fn staged_anything(repo: &git2::Repository, relative: &Path) -> Result<bool> {
    let Some(head) = peeled_commit(repo, "HEAD")? else {
        return Ok(false);
    };
    let tree = peeled_tree(repo, &head)?;
    let mut options = git2::DiffOptions::new();
    options.pathspec(relative);
    let diff = repo.diff_tree_to_index(Some(&tree), None, Some(&mut options))?;
    Ok(diff.deltas().len() > 0)
}

/// The tree of a commit or tree.
fn peeled_tree<'r>(repo: &'r git2::Repository, spec: &str) -> Result<git2::Tree<'r>> {
    let id = commit_or_tree(repo, spec)?;
    Ok(repo.find_object(id, None)?.peel_to_tree()?)
}

fn commit_or_tree(repo: &git2::Repository, spec: &str) -> Result<git2::Oid> {
    match commit_id(repo, spec) {
        Ok(id) => Ok(id),
        Err(_) => repo
            .revparse_single(spec)
            .map(|found| found.id())
            .map_err(|_| Error(format!("unknown tree: {spec}"))),
    }
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
                old_mode: u32::from(delta.old_file().mode()),
                new_mode: u32::from(delta.new_file().mode()),
                new_id: delta.new_file().id().to_string(),
                status,
            })
        })
        .collect()
}

fn untracked_in(repo: &git2::Repository) -> Result<Vec<Vec<u8>>> {
    let mut options = git2::StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(true)
        .recurse_ignored_dirs(false);
    let statuses = repo.statuses(Some(&mut options))?;
    Ok(statuses
        .iter()
        .filter(|entry| {
            let status = entry.status();
            (status.is_wt_new() && !status.is_index_new())
                || (status.is_ignored() && is_nested_repo(repo, entry.path_bytes()))
        })
        .map(|entry| entry.path_bytes().to_vec())
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
fn ancestor_dirs(path: &[u8]) -> Vec<Vec<u8>> {
    path.iter()
        .enumerate()
        .filter(|(at, byte)| **byte == b'/' && *at + 1 < path.len())
        .map(|(at, _)| path[..=at].to_vec())
        .collect()
}

/// Whether the status entry `path` is a directory holding a repository of its own that no ignore
/// rule names. libgit2 reports it as ignored when nothing else is in it; git lists it as
/// untracked.
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
