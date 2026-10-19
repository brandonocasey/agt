//! Lock files whose document is gone, and the guarded deletion of them.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::super::Store;
use super::Removal;
use crate::core::error::{Result, ctx, fail};
use crate::core::local::is_current;

impl Store {
    /// Lock files (`.<key>.lock`, `.<key>.publish.lock`) under the namespace whose document is
    /// gone, so nothing will ever lock them again.
    pub fn stray_locks(&self) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        let mut pending = vec![self.root.clone()];
        while let Some(dir) = pending.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return fail(format!("read {}: {error}", dir.display())),
            };
            for entry in entries {
                let entry = entry.map_err(ctx(format!("read {}", dir.display())))?;
                let kind = entry
                    .file_type()
                    .map_err(ctx(format!("stat {}", entry.path().display())))?;
                if kind.is_dir() {
                    pending.push(entry.path());
                } else if kind.is_file()
                    && lock_target_gone(&dir, &entry.file_name().to_string_lossy())
                {
                    found.push(entry.path());
                }
            }
        }
        found.sort();
        Ok(found)
    }

    /// Deletes a lock file from [`Store::stray_locks`] unless it is held or its document came
    /// back.
    pub fn remove_stray_lock(&self, path: &Path) -> Result<Removal> {
        match self.lock_stray(path)? {
            StrayLock::Held(file) => delete_stray(&file, path),
            StrayLock::Busy => Ok(Removal::Busy),
            StrayLock::Kept => Ok(Removal::Kept),
        }
    }

    /// Takes the lock of a stray lock file without waiting. Opening without `create`, so a lock
    /// that vanished stays vanished.
    pub(super) fn lock_stray(&self, path: &Path) -> Result<StrayLock> {
        match fs::OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => lock_stray_file(file, path),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(StrayLock::Kept),
            Err(error) => fail(format!("open {}: {error}", path.display())),
        }
    }
}

/// A stray lock file after trying to take its lock.
pub(super) enum StrayLock {
    Held(fs::File),
    Busy,
    Kept,
}

/// Locks an opened lock file and checks that it is still the one at `path` and still stray. A
/// lock file replaced since the open is a different inode, and deleting it would break the
/// lock its holder believes it has.
pub(super) fn lock_stray_file(file: fs::File, path: &Path) -> Result<StrayLock> {
    match file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(StrayLock::Busy),
        Err(fs::TryLockError::Error(error)) => {
            return fail(format!("lock {}: {error}", path.display()));
        }
    }
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(StrayLock::Kept);
    };
    if !is_current(&file, path)? || !lock_target_gone(dir, &name.to_string_lossy()) {
        return Ok(StrayLock::Kept);
    }
    Ok(StrayLock::Held(file))
}

fn delete_stray(file: &fs::File, path: &Path) -> Result<Removal> {
    let bytes = file.metadata().map_or(0, |meta| meta.len());
    fs::remove_file(path).map_err(ctx(format!("remove {}", path.display())))?;
    Ok(Removal::Removed(bytes))
}

/// Whether `name` is a lock file in `dir` whose document does not exist.
pub(super) fn lock_target_gone(dir: &Path, name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix('.')
        .and_then(|name| name.strip_suffix(".lock"))
        .filter(|stem| !stem.is_empty())
    else {
        return false;
    };
    let exists = |stem: &str| dir.join(format!("{stem}.json")).exists();
    // `agt issue push` holds `.push.publish.lock` for the whole tracker, which has no `push`
    // document.
    if stem == "push.publish" && exists("tracker") {
        return false;
    }
    // A target key may itself end in `.publish`, so either reading keeps the lock.
    !exists(stem) && !stem.strip_suffix(".publish").is_some_and(exists)
}
