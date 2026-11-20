//! Work tree scans for trees libgit2 cannot walk. Its directory iterator stops 100 levels down
//! (`FILESYSTEM_MAX_DEPTH` in `libgit2/src/libgit2/iterator.c`), where git has no limit. These
//! run only after libgit2 reports that error. They read the tree with the standard library and
//! ask libgit2 only what walks no directory: ignore rules and submodule state.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::GITLINK_MODE;
use super::files::{ancestor_dirs, staged_diff};
use super::index::{STAGE_MASK, Scope, StatusItem, UntrackedMode, is_hidden};
use crate::core::error::{Error, Result};

const SYMLINK_MODE: u32 = 0o120_000;

/// Whether `error` is libgit2 refusing a directory nested too deep for its iterator.
pub(super) fn is_too_deep(error: &git2::Error) -> bool {
    error.class() == git2::ErrorClass::Repository
        && error.message().starts_with("directory nesting too deep")
}

/// The status listing for `scope`, as libgit2 builds it, sorted by path. Differences: renames
/// are found only among the staged changes, and a submodule's own state is libgit2's.
pub(super) fn walk(repo: &git2::Repository, scope: Scope) -> Result<Vec<StatusItem>> {
    let mut walk = Walk {
        repo,
        root: work_tree(repo)?.to_owned(),
        scope,
        tracked: HashSet::new(),
        gitlinks: HashSet::new(),
        tracked_dirs: HashSet::new(),
        found: BTreeMap::new(),
    };
    walk.compare_index()?;
    if scope.untracked != UntrackedMode::No || scope.ignored {
        walk.visit(&[])?;
    }
    if scope.staged {
        walk.add_staged()?;
    }
    Ok(walk.found.into_values().collect())
}

/// The paths that differ between `head` and the work tree, as `git diff HEAD` lists them.
pub(super) fn changed_since(
    repo: &git2::Repository,
    head: &git2::Tree<'_>,
) -> Result<Vec<Vec<u8>>> {
    let scope = Scope {
        untracked: UntrackedMode::No,
        ignored: false,
        staged: true,
        renames: false,
        submodules: true,
    };
    let root = work_tree(repo)?;
    let filemode = honours_filemode(repo);
    let mut changed = Vec::new();
    for item in walk(repo, scope)? {
        let path = Path::new(OsStr::from_bytes(&item.path));
        let full = root.join(path);
        let on_disk = stat(&full)?;
        let in_head = head.get_path(path).ok();
        let differs = match (&on_disk, &in_head) {
            // A submodule or a directory has no content to compare: it is listed because it changed.
            (Some(meta), Some(entry))
                if !meta.is_dir() && entry.kind() != Some(git2::ObjectType::Commit) =>
            {
                meta.is_symlink() != (entry.filemode() == 0o120_000)
                    || blob_id(&full, meta)? != entry.id()
                    || (filemode
                        && !meta.is_symlink()
                        && is_executable(meta) != (entry.filemode() & 0o111 != 0))
            }
            (None, None) => false,
            _ => true,
        };
        if differs {
            changed.push(item.path);
        }
    }
    Ok(changed)
}

/// Writes `paths` of `tree` into the work tree and the index, replacing what is there, for a
/// tree libgit2's checkout cannot walk. A file is written as the object holds it, with no
/// line-ending conversion.
pub(super) fn check_out(
    repo: &git2::Repository,
    tree: &git2::Tree<'_>,
    paths: &[Vec<u8>],
) -> Result<()> {
    let root = work_tree(repo)?;
    let mut index = repo.index()?;
    for path in paths {
        let relative = Path::new(OsStr::from_bytes(path));
        let entry = tree.get_path(relative)?;
        let blob = repo.find_blob(entry.id())?;
        let full = root.join(relative);
        let written = write_entry(&full, entry.filemode(), blob.content());
        written.map_err(|error| Error(format!("{}: {error}", full.display())))?;
        index.add_path(relative)?;
    }
    index.write()?;
    Ok(())
}

fn write_entry(full: &Path, filemode: i32, content: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    // A link is replaced, never written through.
    if fs::symlink_metadata(full).is_ok() {
        fs::remove_file(full)?;
    }
    if filemode == 0o120_000 {
        return std::os::unix::fs::symlink(OsStr::from_bytes(content), full);
    }
    let mode = if filemode & 0o111 != 0 { 0o777 } else { 0o666 };
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(full)?
        .write_all(content)
}

fn work_tree(repo: &git2::Repository) -> Result<&Path> {
    repo.workdir()
        .ok_or_else(|| Error("this operation must be run in a work tree".into()))
}

fn honours_filemode(repo: &git2::Repository) -> bool {
    repo.config()
        .and_then(|config| config.get_bool("core.filemode"))
        .unwrap_or(true)
}

fn is_executable(meta: &fs::Metadata) -> bool {
    meta.mode() & 0o111 != 0
}

/// The metadata of `path` without following a link, or none when nothing is there.
fn stat(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            Ok(None)
        }
        Err(error) => Err(Error(format!("{}: {error}", path.display()))),
    }
}

/// The id git gives the blob of the file or link at `path`, without filters.
fn blob_id(path: &Path, meta: &fs::Metadata) -> Result<git2::Oid> {
    let id = if meta.is_symlink() {
        let target =
            fs::read_link(path).map_err(|error| Error(format!("{}: {error}", path.display())))?;
        git2::Oid::hash_object(git2::ObjectType::Blob, target.as_os_str().as_bytes())
    } else {
        git2::Oid::hash_file(git2::ObjectType::Blob, path)
    };
    id.map_err(|error| Error(format!("{}: {error}", path.display())))
}

struct Walk<'r> {
    repo: &'r git2::Repository,
    root: PathBuf,
    scope: Scope,
    /// The paths the index holds at any stage, submodules included.
    tracked: HashSet<Vec<u8>>,
    /// The submodule paths the index holds at stage 0.
    gitlinks: HashSet<Vec<u8>>,
    /// The directories (with their trailing slash) above any index entry.
    tracked_dirs: HashSet<Vec<u8>>,
    found: BTreeMap<Vec<u8>, StatusItem>,
}

impl Walk<'_> {
    fn note(&mut self, path: &[u8], status: git2::Status) {
        self.found
            .entry(path.to_vec())
            .or_insert_with(|| StatusItem {
                path: path.to_vec(),
                shown: path.to_vec(),
                status: git2::Status::empty(),
            })
            .status |= status;
    }

    /// Notes each index entry the work tree changed or lacks, and each conflicted path.
    fn compare_index(&mut self) -> Result<()> {
        let index = self.repo.index()?;
        let filemode = honours_filemode(self.repo);
        // An entry as new as the index file may hold a change its timestamp cannot show.
        let index_seconds =
            fs::metadata(self.repo.path().join("index")).map_or(i64::MIN, |meta| meta.mtime());
        for entry in index.iter() {
            self.tracked_dirs.extend(ancestor_dirs(&entry.path));
            self.tracked.insert(entry.path.clone());
            if entry.flags & STAGE_MASK != 0 {
                self.note(&entry.path, git2::Status::CONFLICTED);
                continue;
            }
            if entry.mode == GITLINK_MODE {
                self.gitlinks.insert(entry.path.clone());
            }
            let status = self.compare(&entry, filemode, index_seconds)?;
            if !status.is_empty() {
                self.note(&entry.path, status);
            }
        }
        Ok(())
    }

    /// How the work tree file of `entry` differs from it: not at all, or deleted, type changed
    /// or modified.
    fn compare(
        &self,
        entry: &git2::IndexEntry,
        filemode: bool,
        index_seconds: i64,
    ) -> Result<git2::Status> {
        let path = Path::new(OsStr::from_bytes(&entry.path));
        let full = self.root.join(path);
        let Some(meta) = stat(&full)? else {
            return Ok(git2::Status::WT_DELETED);
        };
        if entry.mode == GITLINK_MODE {
            return self.compare_submodule(path);
        }
        if meta.is_dir() {
            return Ok(git2::Status::WT_DELETED);
        }
        if is_hidden(entry) {
            return Ok(git2::Status::empty());
        }
        if meta.is_symlink() != (entry.mode == SYMLINK_MODE) {
            return Ok(git2::Status::WT_TYPECHANGE);
        }
        if filemode && !meta.is_symlink() && is_executable(&meta) != (entry.mode & 0o111 != 0) {
            return Ok(git2::Status::WT_MODIFIED);
        }
        let seconds = i64::from(entry.mtime.seconds());
        let same_stat = u64::from(entry.file_size) == meta.size()
            && seconds == meta.mtime()
            && i64::from(entry.mtime.nanoseconds()) == meta.mtime_nsec();
        if same_stat && seconds < index_seconds {
            return Ok(git2::Status::empty());
        }
        Ok(if blob_id(&full, &meta)? == entry.id {
            git2::Status::empty()
        } else {
            git2::Status::WT_MODIFIED
        })
    }

    fn compare_submodule(&self, path: &Path) -> Result<git2::Status> {
        if !self.scope.submodules {
            return Ok(git2::Status::empty());
        }
        let name = String::from_utf8_lossy(path.as_os_str().as_bytes()).into_owned();
        let state = match self
            .repo
            .submodule_status(&name, git2::SubmoduleIgnore::Unspecified)
        {
            Ok(state) => state,
            // A gitlink that .gitmodules does not name is no submodule.
            Err(error) if error.code() == git2::ErrorCode::NotFound => {
                return Ok(git2::Status::empty());
            }
            Err(error) => return Err(error.into()),
        };
        let dirty = git2::SubmoduleStatus::WD_MODIFIED
            | git2::SubmoduleStatus::WD_INDEX_MODIFIED
            | git2::SubmoduleStatus::WD_WD_MODIFIED
            | git2::SubmoduleStatus::WD_UNTRACKED;
        Ok(if state.contains(git2::SubmoduleStatus::WD_DELETED) {
            git2::Status::WT_DELETED
        } else if state.intersects(dirty) {
            git2::Status::WT_MODIFIED
        } else {
            git2::Status::empty()
        })
    }

    /// Notes the changes staged against HEAD, a rename under its new path.
    fn add_staged(&mut self) -> Result<()> {
        let mut diff = staged_diff(self.repo)?;
        if self.scope.renames {
            diff.find_similar(Some(git2::DiffFindOptions::new().renames(true)))?;
        }
        for delta in diff.deltas() {
            let status = match delta.status() {
                git2::Delta::Added => git2::Status::INDEX_NEW,
                git2::Delta::Deleted => git2::Status::INDEX_DELETED,
                git2::Delta::Modified => git2::Status::INDEX_MODIFIED,
                git2::Delta::Typechange => git2::Status::INDEX_TYPECHANGE,
                git2::Delta::Renamed => git2::Status::INDEX_RENAMED,
                _ => continue,
            };
            if let Some(path) = delta.new_file().path_bytes() {
                self.note(path, status);
            }
        }
        Ok(())
    }

    /// Notes the untracked and ignored paths below the directory `dir` (empty for the root, else
    /// with a trailing slash).
    fn visit(&mut self, dir: &[u8]) -> Result<()> {
        let full = self.root.join(OsStr::from_bytes(dir));
        let children = match fs::read_dir(&full) {
            Ok(children) => children,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error(format!("{}: {error}", full.display()))),
        };
        for child in children {
            let child = child.map_err(|error| Error(format!("{}: {error}", full.display())))?;
            let name = child.file_name();
            if name.as_bytes() == b".git" {
                continue;
            }
            let path = [dir, name.as_bytes()].concat();
            let kind = child
                .file_type()
                .map_err(|error| Error(format!("{}: {error}", child.path().display())))?;
            if kind.is_dir() {
                self.visit_dir(path)?;
            } else if (kind.is_file() || kind.is_symlink()) && !self.tracked.contains(&path) {
                self.visit_file(&path)?;
            }
        }
        Ok(())
    }

    fn visit_file(&mut self, path: &[u8]) -> Result<()> {
        if self.is_ignored(path)? {
            if self.scope.ignored {
                self.note(path, git2::Status::IGNORED);
            }
        } else if self.scope.untracked != UntrackedMode::No {
            self.note(path, git2::Status::WT_NEW);
        }
        Ok(())
    }

    /// A tracked submodule is left to its own repository. A nested repository, or a directory
    /// that is ignored and holds nothing tracked, is one entry; any other directory is read,
    /// whatever tracked file it replaced.
    fn visit_dir(&mut self, mut path: Vec<u8>) -> Result<()> {
        if self.gitlinks.contains(&path) {
            return Ok(());
        }
        let nested = self
            .root
            .join(OsStr::from_bytes(&path))
            .join(".git")
            .exists();
        let ignored = self.is_ignored(&path)?;
        path.push(b'/');
        if nested || (ignored && !self.tracked_dirs.contains(&path)) {
            if ignored && self.scope.ignored {
                self.note(&path, git2::Status::IGNORED);
            } else if !ignored && self.scope.untracked != UntrackedMode::No {
                self.note(&path, git2::Status::WT_NEW);
            }
            return Ok(());
        }
        self.visit(&path)?;
        if self.scope.untracked == UntrackedMode::Normal && !self.tracked_dirs.contains(&path) {
            self.fold_untracked(&path);
        }
        Ok(())
    }

    /// Lists the untracked files below the directory `dir` as the directory alone.
    fn fold_untracked(&mut self, dir: &[u8]) {
        let files: Vec<Vec<u8>> = self
            .found
            .range(dir.to_vec()..)
            .take_while(|(path, _)| path.starts_with(dir))
            .filter(|(_, item)| item.status.is_wt_new())
            .map(|(path, _)| path.clone())
            .collect();
        if files.is_empty() {
            return;
        }
        for file in files {
            self.found.remove(&file);
        }
        self.note(dir, git2::Status::WT_NEW);
    }

    fn is_ignored(&self, path: &[u8]) -> Result<bool> {
        Ok(self
            .repo
            .is_path_ignored(Path::new(OsStr::from_bytes(path)))?)
    }
}
