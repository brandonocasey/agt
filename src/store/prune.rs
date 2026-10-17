//! What `agt prune` may delete from the store, and the locked, non-blocking ways to delete it, so
//! a prune never waits for or corrupts a concurrent agt run.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::{Namespace, Store, ctx, fail, read_target, try_lock, validate_key};
use crate::core::clock;
use crate::core::error::Result;
use crate::store::model::Target;

/// How long a finished thing is kept before `prune` drops it.
pub const PRUNE_AFTER_SECS: i64 = 90 * 24 * 60 * 60;

/// The outcome of one guarded deletion.
#[derive(Debug, PartialEq, Eq)]
pub enum Removal {
    /// Deleted; the bytes freed.
    Removed(u64),
    /// The thing is gone or no longer qualifies.
    Kept,
    /// Another process holds its lock.
    Busy,
}

/// A backup directory made by `agt` itself: `<namespace>-v<version>-<stamp>[-<n>]`, or the
/// hidden `.<name>.partial` copy of one that did not finish.
#[derive(Debug, Clone)]
pub struct Backup {
    pub path: PathBuf,
    pub namespace: Namespace,
    pub partial: bool,
    /// When it was taken, Unix seconds, and the `-<n>` suffix that separates two in one second.
    pub taken: (i64, u32),
    pub bytes: u64,
}

pub(super) fn remove_with_lock(path: &Path, lock_path: &Path) -> Result<u64> {
    let bytes = fs::metadata(path).map_or(0, |meta| meta.len());
    fs::remove_file(path).map_err(ctx(format!("remove {}", path.display())))?;
    // Waiters on the old lock inode notice the unlink and retry on a fresh file.
    fs::remove_file(lock_path).map_err(ctx(format!("remove {}", lock_path.display())))?;
    Ok(bytes)
}

impl Store {
    /// Like [`Store::remove_if`], but gives up with [`Removal::Busy`] instead of waiting.
    pub fn try_remove_if(
        &self,
        dir: &Path,
        key: &str,
        remove: impl FnOnce(&Target) -> Result<bool>,
    ) -> Result<Removal> {
        validate_key(key)?;
        let lock_path = dir.join(format!(".{key}.lock"));
        let Some(_lock) = try_lock(&lock_path)? else {
            return Ok(Removal::Busy);
        };
        let path = dir.join(format!("{key}.json"));
        let Some(target) = read_target(&path, self.schema)? else {
            return Ok(Removal::Kept);
        };
        if !remove(&target)? {
            return Ok(Removal::Kept);
        }
        remove_with_lock(&path, &lock_path).map(Removal::Removed)
    }

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
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return Ok(Removal::Kept);
        };
        // Opening without `create`, so a lock that vanished stays vanished.
        let file = match fs::OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Removal::Kept),
            Err(error) => return fail(format!("open {}: {error}", path.display())),
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Ok(Removal::Busy),
            Err(fs::TryLockError::Error(error)) => {
                return fail(format!("lock {}: {error}", path.display()));
            }
        }
        if !lock_target_gone(dir, &name.to_string_lossy()) {
            return Ok(Removal::Kept);
        }
        let bytes = file.metadata().map_or(0, |meta| meta.len());
        fs::remove_file(path).map_err(ctx(format!("remove {}", path.display())))?;
        Ok(Removal::Removed(bytes))
    }

    /// The backups `agt` made, finished and partial, oldest first. Anything else in the backups
    /// directory belongs to another tool and is not listed.
    pub fn agt_backups(&self) -> Result<Vec<Backup>> {
        let entries = match fs::read_dir(&self.backups) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return fail(format!("read {}: {error}", self.backups.display())),
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(ctx(format!("read {}", self.backups.display())))?;
            let kind = entry
                .file_type()
                .map_err(ctx(format!("stat {}", entry.path().display())))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !kind.is_dir() {
                continue;
            }
            let (partial, name) = match name
                .strip_prefix('.')
                .and_then(|name| name.strip_suffix(".partial"))
            {
                Some(inner) => (true, inner),
                None => (false, name.as_str()),
            };
            if let Some((namespace, taken)) = parse_backup_name(name) {
                found.push(Backup {
                    bytes: tree_size(&entry.path()),
                    path: entry.path(),
                    namespace,
                    partial,
                    taken,
                });
            }
        }
        found.sort_by(|a, b| (a.taken, &a.path).cmp(&(b.taken, &b.path)));
        Ok(found)
    }

    /// Deletes a backup unless a backup of its namespace is being made.
    pub fn remove_backup(&self, backup: &Backup) -> Result<Removal> {
        let name = backup.namespace.dir_name();
        let lock_path = self.backups.with_file_name(format!(".{name}-backup.lock"));
        let Some(_lock) = try_lock(&lock_path)? else {
            return Ok(Removal::Busy);
        };
        match fs::remove_dir_all(&backup.path) {
            Ok(()) => Ok(Removal::Removed(backup.bytes)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Removal::Kept),
            Err(error) => fail(format!("remove {}: {error}", backup.path.display())),
        }
    }
}

/// Whether `name` is a lock file in `dir` whose document does not exist.
fn lock_target_gone(dir: &Path, name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix('.')
        .and_then(|name| name.strip_suffix(".lock"))
        .filter(|stem| !stem.is_empty())
    else {
        return false;
    };
    let exists = |stem: &str| dir.join(format!("{stem}.json")).exists();
    // A target key may itself end in `.publish`, so either reading keeps the lock.
    !exists(stem) && !stem.strip_suffix(".publish").is_some_and(exists)
}

/// `reviews-v1-2026-07-01T10-20-30Z` or the same with `-2`: the namespace and when it was made.
fn parse_backup_name(name: &str) -> Option<(Namespace, (i64, u32))> {
    let namespace = Namespace::ALL
        .into_iter()
        .find(|namespace| name.starts_with(&format!("{}-v", namespace.dir_name())))?;
    let rest = name.strip_prefix(&format!("{}-v", namespace.dir_name()))?;
    let (version, stamp) = rest.split_once('-')?;
    if version.is_empty() || !version.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let time = stamp.get(..20)?;
    let attempt = match stamp.get(20..)? {
        "" => 1,
        suffix => suffix.strip_prefix('-')?.parse().ok().filter(|n| *n >= 2)?,
    };
    if time.as_bytes().get(10) != Some(&b'T') || !time.ends_with('Z') {
        return None;
    }
    // The stamp is the timestamp with its time colons turned into dashes.
    let iso = format!("{}{}", time.get(..11)?, time.get(11..)?.replace('-', ":"));
    Some((namespace, (clock::parse(&iso).ok()?, attempt)))
}

fn tree_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path).map_or(0, |entries| {
        entries
            .flatten()
            .map(|entry| tree_size(&entry.path()))
            .sum()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_exact_backup_names_parse() {
        let (namespace, (taken, attempt)) =
            parse_backup_name("reviews-v1-1970-01-02T00-00-01Z").unwrap();
        assert_eq!((namespace, taken, attempt), (Namespace::Reviews, 86_401, 1));
        assert_eq!(
            parse_backup_name("issues-v12-1970-01-02T00-00-01Z-3")
                .unwrap()
                .1,
            (86_401, 3)
        );
        for foreign in [
            "reviews-v1",
            "reviews-vx-1970-01-02T00-00-01Z",
            "reviews-v1-1970-01-02T00-00-01Z-1",
            "reviews-v1-1970-01-02T00-00-01Zjunk",
            "notes-v1-1970-01-02T00-00-01Z",
            "repo",
        ] {
            assert!(parse_backup_name(foreign).is_none(), "{foreign}");
        }
    }

    #[test]
    fn a_lock_is_stray_only_when_its_document_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.json"), "{}").unwrap();
        assert!(!lock_target_gone(dir.path(), ".a.lock"));
        assert!(!lock_target_gone(dir.path(), ".a.publish.lock"));
        assert!(lock_target_gone(dir.path(), ".b.lock"));
        assert!(lock_target_gone(dir.path(), ".b.publish.lock"));
        assert!(!lock_target_gone(dir.path(), ".account.json"));
        assert!(!lock_target_gone(dir.path(), "b.lock"));
    }
}
