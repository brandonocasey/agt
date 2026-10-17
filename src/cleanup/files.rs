//! Agent working files under the cache root that nothing touched for a while: long-output
//! copies in `agents/output`, scratch in `agents/scratch`, and Claude Code's per-session
//! scratchpads. Each top-level entry goes whole, and only when every file in it is old, so a
//! task that is still writing keeps all of its files.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::core::error::{Error, Result};
use crate::store::prune::Removal;

pub const OUTPUT_DAYS: i64 = 7;
pub const SCRATCH_DAYS: i64 = 30;
const DAY: i64 = 86_400;
/// Test-only replacement for `/tmp`, so tests never read the real one.
const TMP_OVERRIDE: &str = "AGT_CLEANUP_TMP";

/// One entry whose whole tree is older than the cutoff.
pub struct Stale {
    pub path: PathBuf,
    /// The directory scanned, which `remove` checks again from here down.
    pub root: PathBuf,
    pub key: String,
    pub newest: i64,
    pub bytes: u64,
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Error("HOME is not set".into()))
}

/// `$XDG_CACHE_HOME`, or `~/.cache` when it is unset or relative.
pub fn cache_root() -> Result<PathBuf> {
    match std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => Ok(path),
        _ => Ok(home()?.join(".cache")),
    }
}

/// Where Claude Code keeps session scratchpads: `<tmp>/claude-<uid>/<cwd-slug>/<session>`.
/// Settings point it at `~/.cache/claude-tmp`; older sessions used `/tmp`.
pub fn harness_roots() -> Result<Vec<PathBuf>> {
    let uid = unsafe { libc::getuid() };
    let tmp = std::env::var_os(TMP_OVERRIDE).map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    Ok(vec![home()?.join(".cache").join("claude-tmp"), tmp]
        .into_iter()
        .map(|base| base.join(format!("claude-{uid}")))
        .collect())
}

fn mtime(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// The newest modification time in a tree and its total size. Links count as themselves and
/// are never followed.
pub fn measure(path: &Path) -> io::Result<(i64, u64)> {
    let meta = fs::symlink_metadata(path)?;
    let (mut newest, mut bytes) = (mtime(&meta), meta.len());
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            let (entry_newest, entry_bytes) = measure(&entry?.path())?;
            newest = newest.max(entry_newest);
            bytes += entry_bytes;
        }
    }
    Ok((newest, bytes))
}

/// A real directory, not a link, owned by this user. `/tmp/claude-<uid>` may be made by
/// anyone, so every directory between a root and what is deleted must pass this.
fn own_dir(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_dir() && meta.uid() == unsafe { libc::getuid() })
}

/// Whether `dir` and each directory from it down to `path`'s parent pass [`own_dir`].
fn trusted(root: &Path, path: &Path) -> bool {
    let Ok(below) = path.parent().unwrap_or(root).strip_prefix(root) else {
        return false;
    };
    let mut dir = root.to_path_buf();
    if !own_dir(&dir) {
        return false;
    }
    for part in below.components() {
        dir.push(part);
        if !own_dir(&dir) {
            return false;
        }
    }
    true
}

fn children(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut found: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<_>>()?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    found.sort();
    Ok(found)
}

/// Entries of `dirs` (each below `root`) untouched for `days` days. `key` names an entry for
/// the plan. A directory that is a link or another user's is skipped with a warning.
fn stale_in(
    root: &Path,
    dirs: &[PathBuf],
    days: i64,
    now: i64,
    key: impl Fn(&Path) -> String,
    warnings: &mut Vec<String>,
) -> Vec<Stale> {
    let mut found = Vec::new();
    for dir in dirs {
        if fs::symlink_metadata(dir).is_err() {
            continue;
        }
        if !trusted(root, &dir.join("entry")) {
            warnings.push(format!(
                "{}: skipped, not a directory of this user (a link or another owner)",
                dir.display()
            ));
            continue;
        }
        let entries = match children(dir) {
            Ok(entries) => entries,
            Err(error) => {
                warnings.push(format!("{}: {error}", dir.display()));
                continue;
            }
        };
        for path in entries {
            if path.file_name().is_some_and(|name| name == ".keep") {
                continue;
            }
            match measure(&path) {
                Ok((newest, bytes)) if now - newest > days * DAY => found.push(Stale {
                    key: key(&path),
                    root: root.to_path_buf(),
                    path,
                    newest,
                    bytes,
                }),
                Ok(_) => {}
                // Something deleted it while we looked; there is nothing left to plan.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => warnings.push(format!("{}: {error}", path.display())),
            }
        }
    }
    found
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Task directories and files in `agents/output`.
pub fn output(now: i64, warnings: &mut Vec<String>) -> Result<Vec<Stale>> {
    let root = cache_root()?.join("agents").join("output");
    Ok(stale_in(
        &root,
        std::slice::from_ref(&root),
        OUTPUT_DAYS,
        now,
        |path| relative(&root, path),
        warnings,
    ))
}

/// Task directories in `agents/scratch`, and harness session directories.
pub fn scratch(now: i64, warnings: &mut Vec<String>) -> Result<Vec<Stale>> {
    let root = cache_root()?.join("agents").join("scratch");
    let mut found = stale_in(
        &root,
        std::slice::from_ref(&root),
        SCRATCH_DAYS,
        now,
        |path| relative(&root, path),
        warnings,
    );
    for harness in harness_roots()? {
        if fs::symlink_metadata(&harness).is_err() {
            continue;
        }
        if !own_dir(&harness) {
            warnings.push(format!(
                "{}: skipped, not a directory of this user (a link or another owner)",
                harness.display()
            ));
            continue;
        }
        let slugs = match children(&harness) {
            Ok(slugs) => slugs,
            Err(error) => {
                warnings.push(format!("{}: {error}", harness.display()));
                continue;
            }
        };
        // Links among the slugs are not followed: a link is not a real directory here.
        let slugs: Vec<PathBuf> = slugs
            .into_iter()
            .filter(|path| fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()))
            .collect();
        found.extend(stale_in(
            &harness,
            &slugs,
            SCRATCH_DAYS,
            now,
            |path| path.to_string_lossy().into_owned(),
            warnings,
        ));
    }
    Ok(found)
}

pub fn reason(stale: &Stale, now: i64, days: i64) -> String {
    format!(
        "nothing in it changed for {} day(s) (over {days}); newest change {}",
        (now - stale.newest) / DAY,
        crate::core::clock::format(stale.newest)
    )
}

/// Deletes an entry if it is still untouched for `days` days and every directory from `root`
/// down to it is still this user's own. A harness session directory's parent is removed too
/// once it is empty.
pub fn remove(
    root: &Path,
    path: &Path,
    days: i64,
    now: i64,
    prune_parent: bool,
) -> Result<Removal> {
    if !trusted(root, path) {
        return Err(Error(format!(
            "{}: not deleted, a directory above it is a link or another user's",
            path.display()
        )));
    }
    let (newest, bytes) = match measure(path) {
        Ok(found) => found,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Removal::Kept),
        Err(error) => return Err(Error(format!("{}: {error}", path.display()))),
    };
    if now - newest <= days * DAY {
        return Ok(Removal::Kept);
    }
    let meta = fs::symlink_metadata(path)
        .map_err(|error| Error(format!("{}: {error}", path.display())))?;
    let result = if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|error| Error(format!("{}: {error}", path.display())))?;
    if prune_parent && let Some(parent) = path.parent() {
        // Fails, and is meant to, while anything else is in it.
        let _ = fs::remove_dir(parent);
    }
    Ok(Removal::Removed(bytes))
}
