//! The index and the work tree through libgit2: status, submodule entries, and the trees
//! `agt` builds from a work tree without touching the real index.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::files::is_nested_repo;
use super::{Gitlink, Repo, StatusEntry};
use crate::core::error::{Error, Result};

/// The mode of a submodule entry.
const GITLINK_MODE: u32 = 0o160000;
/// `GIT_INDEX_ENTRY_VALID`: the file is assumed unchanged.
const ASSUME_UNCHANGED: u16 = 0x8000;
/// `GIT_INDEX_ENTRY_SKIP_WORKTREE`, in the extended flags.
pub(super) const SKIP_WORKTREE: u16 = 1 << 14;

/// Whether `git status` shows the changes inside a submodule's own work tree.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SubmoduleDirt {
    Report,
    Ignore,
}

/// How `git status` lists untracked files, from `status.showUntrackedFiles`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UntrackedMode {
    No,
    Normal,
    All,
}

impl Repo {
    /// What `git status` lists, without renames. `untracked` also lists every untracked file.
    pub fn status(&self, untracked: bool, dirt: SubmoduleDirt) -> Result<Vec<StatusEntry>> {
        let mode = if untracked {
            UntrackedMode::All
        } else {
            UntrackedMode::No
        };
        let repo = self.open()?;
        let mut found = listed(&repo, mode, false)?;
        if dirt == SubmoduleDirt::Ignore {
            found.retain(|entry| !submodule_is_only_dirty(&repo, &entry.path));
        }
        Ok(found)
    }

    /// How many changes the work tree has: the paths `git status` lists, a rename counting
    /// once, and the edits it hides.
    pub fn uncommitted_count(&self) -> Result<usize> {
        let configured = self.config_value("status.showUntrackedFiles")?;
        let mode = match configured
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("no" | "false") => UntrackedMode::No,
            Some("all") => UntrackedMode::All,
            _ => UntrackedMode::Normal,
        };
        self.count_changes(mode)
    }

    /// Like `uncommitted_count`, but ignores the user's `status.showUntrackedFiles` setting:
    /// untracked files always count.
    pub fn strict_uncommitted_count(&self) -> Result<usize> {
        let repo = self.open()?;
        let mut paths = self.changed_path_set(&repo, UntrackedMode::Normal)?;
        paths.extend(dirty_submodules(&repo)?);
        Ok(paths.len())
    }

    fn count_changes(&self, mode: UntrackedMode) -> Result<usize> {
        let repo = self.open()?;
        Ok(self.changed_path_set(&repo, mode)?.len())
    }

    fn changed_path_set(
        &self,
        repo: &git2::Repository,
        mode: UntrackedMode,
    ) -> Result<HashSet<String>> {
        let mut paths: HashSet<String> = self.hidden_edits()?.into_iter().collect();
        paths.extend(
            listed(repo, mode, true)?
                .into_iter()
                .map(|entry| entry.path),
        );
        Ok(paths)
    }

    /// The submodules the index records, limited to `pathspec` when it is not empty.
    pub fn gitlinks(&self, pathspec: &[&str]) -> Result<Vec<Gitlink>> {
        let repo = self.open()?;
        let mut index = repo.index()?;
        index.read(false)?;
        let spec = if pathspec.is_empty() {
            None
        } else {
            Some(git2::Pathspec::new(pathspec)?)
        };
        let mut found = Vec::new();
        for entry in index.iter() {
            if entry.mode != GITLINK_MODE {
                continue;
            }
            let path = Path::new(OsStr::from_bytes(&entry.path));
            let wanted = spec
                .as_ref()
                .is_none_or(|spec| spec.matches_path(path, git2::PathspecFlags::DEFAULT));
            if wanted {
                found.push(Gitlink {
                    commit: entry.id.to_string(),
                    path: String::from_utf8_lossy(&entry.path).into_owned(),
                });
            }
        }
        Ok(found)
    }

    /// Whether the index tracks a file under `pathspec`.
    pub fn tracks(&self, pathspec: &str) -> Result<bool> {
        Ok(!self.gitlinks_and_files(pathspec)?.is_empty())
    }

    fn gitlinks_and_files(&self, pathspec: &str) -> Result<Vec<Vec<u8>>> {
        let repo = self.open()?;
        let mut index = repo.index()?;
        index.read(false)?;
        let spec = git2::Pathspec::new([pathspec])?;
        Ok(index
            .iter()
            .filter(|entry| {
                spec.matches_path(
                    Path::new(OsStr::from_bytes(&entry.path)),
                    git2::PathspecFlags::DEFAULT,
                )
            })
            .map(|entry| entry.path)
            .collect())
    }

    /// Runs `body` against a copy of the index that is never written back, so index-changing
    /// calls leave the repository's own index as it was.
    fn with_scratch_index<T>(
        &self,
        body: impl FnOnce(&git2::Repository, &mut git2::Index) -> Result<T>,
    ) -> Result<T> {
        let repo = self.open()?;
        let mut scratch = super::sparse_index::open_index_file(&repo)?;
        repo.set_index(&mut scratch)?;
        let result = body(&repo, &mut scratch);
        if self.held.is_some() {
            repo.set_index(&mut super::sparse_index::open_index_file(&repo)?)?;
        }
        result
    }

    /// Tree id of the work tree as `git add -A` would stage it, built in a copy of the index so
    /// the real index is untouched and tracked ignored files stay in the tree.
    pub fn worktree_tree(&self) -> Result<String> {
        self.with_scratch_index(|repo, index| {
            let absent = absent_hidden(repo, index);
            let mut refused = None;
            let mut check = |path: &Path, _: &[u8]| {
                refused = required_filter(repo, path);
                if refused.is_some() { -1 } else { 0 }
            };
            let added = index.add_all(["*"], git2::IndexAddOption::DEFAULT, Some(&mut check));
            if let Some(message) = refused {
                return Err(Error(message));
            }
            added?;
            index.update_all(["*"], None)?;
            restore_entries(index, &absent)?;
            Ok(index.write_tree_to(repo)?.to_string())
        })
    }

    /// Like [`Self::worktree_tree`], but a tracked file marked skip-worktree or assume-unchanged
    /// that exists on disk is taken as it is, not as the index has it. A file that is gone (a
    /// sparse checkout) keeps its entry, or the tree would record it as deleted. Untracked
    /// files are left out: a backup never keeps them.
    pub fn worktree_tree_with_hidden_edits(&self) -> Result<String> {
        self.with_scratch_index(|repo, index| {
            unhide(repo, index)?;
            let absent = absent_hidden(repo, index);
            index.update_all(["*"], None)?;
            restore_entries(index, &absent)?;
            Ok(index.write_tree_to(repo)?.to_string())
        })
    }

    /// The tracked files marked skip-worktree or assume-unchanged that exist on disk and differ
    /// from the index: edits `git status` does not show.
    pub fn hidden_edits(&self) -> Result<Vec<String>> {
        self.with_scratch_index(|repo, index| {
            let flagged = unhide(repo, index)?;
            if flagged.is_empty() {
                return Ok(Vec::new());
            }
            let mut options = git2::DiffOptions::new();
            let diff = repo.diff_index_to_workdir(Some(index), Some(&mut options))?;
            Ok(diff
                .deltas()
                .map(|delta| {
                    let path = delta.new_file().path_bytes().unwrap_or_default();
                    String::from_utf8_lossy(path).into_owned()
                })
                .filter(|name| flagged.contains(name))
                .collect())
        })
    }
}

/// Clears skip-worktree and assume-unchanged in `index` for each flagged file that exists on
/// disk; returns those paths.
fn unhide(repo: &git2::Repository, index: &mut git2::Index) -> Result<Vec<String>> {
    let Some(workdir) = repo.workdir() else {
        return Ok(Vec::new());
    };
    let mut flagged = Vec::new();
    let entries: Vec<git2::IndexEntry> = index.iter().collect();
    for mut entry in entries {
        let file = workdir.join(OsStr::from_bytes(&entry.path));
        if !is_hidden(&entry) || fs::symlink_metadata(file).is_err() {
            continue;
        }
        flagged.push(String::from_utf8_lossy(&entry.path).into_owned());
        entry.flags &= !ASSUME_UNCHANGED;
        entry.flags_extended &= !SKIP_WORKTREE;
        index.add(&entry)?;
    }
    Ok(flagged)
}

/// The entries marked skip-worktree or assume-unchanged whose file is not on disk, which
/// libgit2 would take for deleted.
fn absent_hidden(repo: &git2::Repository, index: &git2::Index) -> Vec<git2::IndexEntry> {
    let Some(workdir) = repo.workdir() else {
        return Vec::new();
    };
    index
        .iter()
        .filter(is_hidden)
        .filter(|entry| fs::symlink_metadata(workdir.join(OsStr::from_bytes(&entry.path))).is_err())
        .collect()
}

/// Puts back the `entries` that the update dropped.
fn restore_entries(index: &mut git2::Index, entries: &[git2::IndexEntry]) -> Result<()> {
    for entry in entries {
        let path = Path::new(OsStr::from_bytes(&entry.path));
        if index.get_path(path, 0).is_none() {
            index.add(&copy_entry(entry))?;
        }
    }
    Ok(())
}

fn is_hidden(entry: &git2::IndexEntry) -> bool {
    entry.flags & ASSUME_UNCHANGED != 0 || entry.flags_extended & SKIP_WORKTREE != 0
}

/// Whether the only change `git status` could show at `path` is a dirty work tree of the
/// submodule there, which `--ignore-submodules=dirty` hides.
fn submodule_is_only_dirty(repo: &git2::Repository, path: &str) -> bool {
    repo.submodule_status(path, git2::SubmoduleIgnore::Dirty)
        .is_ok_and(|status| !status.contains(git2::SubmoduleStatus::WD_MODIFIED))
        && repo.find_submodule(path).is_ok()
}

/// The submodules with a change of their own, whatever `diff.ignoreSubmodules` says: a commit
/// other than the recorded one, or edits or untracked files in their work tree.
fn dirty_submodules(repo: &git2::Repository) -> Result<Vec<String>> {
    let dirt = git2::SubmoduleStatus::WD_MODIFIED
        | git2::SubmoduleStatus::WD_INDEX_MODIFIED
        | git2::SubmoduleStatus::WD_WD_MODIFIED
        | git2::SubmoduleStatus::WD_UNTRACKED;
    let mut found = Vec::new();
    for submodule in repo.submodules()? {
        let status = repo.submodule_status(
            submodule.name().unwrap_or_default(),
            git2::SubmoduleIgnore::None,
        )?;
        if status.intersects(dirt) {
            found.push(
                String::from_utf8_lossy(submodule.path().as_os_str().as_bytes()).into_owned(),
            );
        }
    }
    Ok(found)
}

/// The status entries of `repo`, as `git status --porcelain=v1` columns.
fn listed(repo: &git2::Repository, mode: UntrackedMode, renames: bool) -> Result<Vec<StatusEntry>> {
    let mut options = git2::StatusOptions::new();
    options
        .include_untracked(mode != UntrackedMode::No)
        .recurse_untracked_dirs(mode == UntrackedMode::All)
        .include_ignored(mode != UntrackedMode::No)
        .recurse_ignored_dirs(false)
        .renames_head_to_index(renames)
        .renames_index_to_workdir(renames);
    let statuses = repo.statuses(Some(&mut options))?;
    let conflicts = conflict_columns(repo)?;
    let index = repo.index()?;
    let mut found = Vec::new();
    for entry in statuses.iter() {
        let mut flags = entry.status();
        // A file a sparse checkout left out is not a deletion.
        if flags.is_wt_deleted()
            && index
                .get_path(Path::new(OsStr::from_bytes(entry.path_bytes())), 0)
                .is_some_and(|hidden| is_hidden(&hidden))
        {
            flags.remove(git2::Status::WT_DELETED);
            if flags.is_empty() {
                continue;
            }
        }
        let path = String::from_utf8_lossy(entry_path(&entry)).into_owned();
        if flags.is_conflicted() {
            let (staged, unstaged) = conflicts
                .iter()
                .find(|(conflicted, _, _)| *conflicted == path)
                .map_or((b'U', b'U'), |(_, staged, unstaged)| (*staged, *unstaged));
            found.push(StatusEntry {
                staged,
                unstaged,
                path,
            });
        } else if (flags.is_wt_new() && !flags.is_index_new())
            || (flags.is_ignored() && is_nested_repo(repo, entry.path_bytes()))
        {
            found.push(StatusEntry {
                staged: b'?',
                unstaged: b'?',
                path,
            });
        } else if !flags.is_empty() && !flags.is_ignored() {
            found.push(StatusEntry {
                staged: index_column(flags),
                unstaged: worktree_column(flags),
                path,
            });
        }
    }
    Ok(found)
}

/// The path git prints for a status entry: the new path of a rename.
fn entry_path<'a>(entry: &'a git2::StatusEntry<'_>) -> &'a [u8] {
    let moved = entry.head_to_index().or_else(|| entry.index_to_workdir());
    moved
        .and_then(|delta| delta.new_file().path_bytes())
        .or_else(|| entry.path_bytes().into())
        .unwrap_or_default()
}

fn index_column(flags: git2::Status) -> u8 {
    if flags.is_index_new() {
        b'A'
    } else if flags.is_index_modified() {
        b'M'
    } else if flags.is_index_deleted() {
        b'D'
    } else if flags.is_index_renamed() {
        b'R'
    } else if flags.is_index_typechange() {
        b'T'
    } else {
        b' '
    }
}

fn worktree_column(flags: git2::Status) -> u8 {
    if flags.is_wt_modified() {
        b'M'
    } else if flags.is_wt_deleted() {
        b'D'
    } else if flags.is_wt_renamed() {
        b'R'
    } else if flags.is_wt_typechange() {
        b'T'
    } else {
        b' '
    }
}

/// The two columns of each conflicted path, from which sides of the merge have the file:
/// `UU` both changed, `AA` both added, `DD` both deleted, `AU`/`UA` and `DU`/`UD` one side only.
fn conflict_columns(repo: &git2::Repository) -> Result<Vec<(String, u8, u8)>> {
    let index = repo.index()?;
    if !index.has_conflicts() {
        return Ok(Vec::new());
    }
    let mut columns = Vec::new();
    for conflict in index.conflicts()? {
        let conflict = conflict?;
        let side = conflict
            .our
            .as_ref()
            .or(conflict.their.as_ref())
            .or(conflict.ancestor.as_ref());
        let Some(side) = side else { continue };
        let (staged, unstaged) = match (
            conflict.ancestor.is_some(),
            conflict.our.is_some(),
            conflict.their.is_some(),
        ) {
            (_, true, true) if conflict.ancestor.is_some() => (b'U', b'U'),
            (_, true, true) => (b'A', b'A'),
            (true, false, false) => (b'D', b'D'),
            (true, true, false) => (b'U', b'D'),
            (true, false, true) => (b'D', b'U'),
            (false, true, false) => (b'A', b'U'),
            _ => (b'U', b'A'),
        };
        columns.push((
            String::from_utf8_lossy(&side.path).into_owned(),
            staged,
            unstaged,
        ));
    }
    Ok(columns)
}

/// A copy of an index entry, which libgit2 does not make clonable.
pub(super) fn copy_entry(entry: &git2::IndexEntry) -> git2::IndexEntry {
    git2::IndexEntry {
        ctime: entry.ctime,
        mtime: entry.mtime,
        dev: entry.dev,
        ino: entry.ino,
        mode: entry.mode,
        uid: entry.uid,
        gid: entry.gid,
        file_size: entry.file_size,
        id: entry.id,
        flags: entry.flags,
        flags_extended: entry.flags_extended,
        path: entry.path.clone(),
    }
}

/// Why `path` cannot be staged: its `filter` attribute names a driver the config marks
/// required, and libgit2 runs no filter drivers, so the content would be hashed unfiltered.
fn required_filter(repo: &git2::Repository, path: &Path) -> Option<String> {
    let flags = git2::AttrCheckFlags::FILE_THEN_INDEX;
    let driver = repo.get_attr(path, "filter", flags).ok()??;
    let required = repo
        .config()
        .ok()?
        .get_bool(&format!("filter.{driver}.required"))
        .unwrap_or(false);
    required.then(|| {
        format!(
            "{}: the required filter '{driver}' cannot be run, so the file cannot be read as git would stage it",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::commit;
    use super::SubmoduleDirt;
    use crate::core::testing::Sandbox;
    use crate::git::Repo;

    #[test]
    fn a_nested_repository_is_untracked_as_git_status_lists_it() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        commit(&sandbox, &dir, &[("a.txt", "a\n")], "one");
        sandbox.git(&dir, &["init", "-q", "nested"]);
        sandbox.git(
            &dir.join("nested"),
            &["commit", "-q", "--allow-empty", "-m", "x"],
        );
        std::fs::write(dir.join(".gitignore"), "ignored-repo/\n").unwrap();
        sandbox.git(&dir, &["init", "-q", "ignored-repo"]);
        let repo = Repo::at(&dir);

        let ours: Vec<String> = repo
            .status(true, SubmoduleDirt::Report)
            .unwrap()
            .into_iter()
            .map(|entry| {
                format!(
                    "{}{} {}",
                    entry.staged as char, entry.unstaged as char, entry.path
                )
            })
            .collect();

        let expected: Vec<String> = sandbox
            .git_output(&dir, &["status", "--porcelain"])
            .lines()
            .map(str::to_owned)
            .collect();
        assert!(
            expected.iter().any(|line| line == "?? nested/"),
            "{expected:?}"
        );
        assert_eq!(ours, expected);
        assert_eq!(repo.uncommitted_count().unwrap(), expected.len());
    }
}
