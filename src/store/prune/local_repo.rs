//! Scanning and deleting a `local/` repository directory whose repository is gone or idle.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::super::sidecars::{ACCOUNT_FILE, LOOKUP_FILE, REPO_FILE};
use super::super::{Store, doc_path, validate_key};
use super::presence::{Presence, seen_by_mtime};
use super::stray::{StrayLock, lock_target_gone};
use super::{PRUNE_AFTER_SECS, Removal, remove_if_present};
use crate::core::clock;
use crate::core::error::{Result, ctx, fail};
use crate::core::local::{read_dir_opt, try_lock_file};
use crate::store::model::Target;

impl Store {
    /// What a `local/` repository directory holds, or `None` when it holds anything `prune` may
    /// not delete: a subdirectory, a link, or a file that is not a target, cache or lock.
    pub fn scan_local_repo(&self, dir: &Path, now: i64) -> Result<Option<LocalRepo>> {
        let Some(entries) = read_dir_opt(dir)? else {
            return Ok(None);
        };
        let (mut extras, mut stray_locks, mut documents) = (Vec::new(), Vec::new(), 0);
        for entry in entries {
            let entry = entry.map_err(ctx(format!("read {}", dir.display())))?;
            let kind = entry
                .file_type()
                .map_err(ctx(format!("stat {}", entry.path().display())))?;
            if !kind.is_file() {
                return Ok(None);
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ACCOUNT_FILE || name == REPO_FILE || name == LOOKUP_FILE {
                extras.push(entry.path());
            } else if lock_target_gone(dir, &name) {
                stray_locks.push(entry.path());
            } else if name.starts_with('.') && name.ends_with(".lock") {
                // A target's lock, deleted with the target.
            } else if !name.starts_with('.') && name.ends_with(".json") {
                documents += 1;
            } else {
                return Ok(None);
            }
        }
        let (targets, _) = self.targets_intact(dir)?;
        if targets.len() != documents {
            return Ok(None);
        }
        extras.sort();
        stray_locks.sort();
        let idle_secs = now - last_write(dir, &extras);
        Ok(Some(LocalRepo {
            idle_secs,
            targets,
            extras,
            stray_locks,
            presence: self.presence(dir, now),
        }))
    }

    /// Deletes a `local/` repository directory: its targets when the repository is gone (else
    /// none may be left), then its cache files and stray locks. Every lock is taken before the
    /// first file goes, so a repository that is partly busy loses nothing.
    ///
    /// `had_targets` says the plan listed the directory because its targets had all expired, so
    /// finding none left is the expected outcome. A directory that held none to begin with and is
    /// not gone goes only once nothing has been written there for [`PRUNE_AFTER_SECS`].
    pub fn remove_local_repo(&self, dir: &Path, now: i64, had_targets: bool) -> Result<Removal> {
        let Some(repo) = self.scan_local_repo(dir, now)? else {
            return Ok(Removal::Kept);
        };
        if repo.targets.is_empty()
            && !repo.presence.is_gone()
            && !had_targets
            && repo.idle_secs < PRUNE_AFTER_SECS
        {
            return Ok(Removal::Kept);
        }
        if !repo.targets.is_empty() && !repo.presence.is_gone() {
            return Ok(Removal::Kept);
        }
        let mut held = Vec::new();
        let (mut documents, mut locks) = (Vec::new(), Vec::new());
        for target in &repo.targets {
            validate_key(&target.key)?;
            for suffix in ["lock", "publish.lock"] {
                let lock_path = dir.join(format!(".{}.{suffix}", target.key));
                let Some(file) = try_lock_file(&lock_path)? else {
                    return Ok(Removal::Busy);
                };
                held.push(file);
                locks.push(lock_path);
            }
            documents.push(doc_path(dir, &target.key));
        }
        for path in &repo.stray_locks {
            match self.lock_stray(path)? {
                StrayLock::Held(file) => held.push(file),
                StrayLock::Busy => return Ok(Removal::Busy),
                StrayLock::Kept => return Ok(Removal::Kept),
            }
            locks.push(path.clone());
        }
        // Documents first, then locks, then cache files: the order `remove_with_lock` uses, so a
        // reader never finds a lock-less document.
        let doomed: Vec<PathBuf> = documents
            .into_iter()
            .chain(locks)
            .chain(repo.extras)
            .collect();
        // A writer that started after the scan has no lock of ours to wait on; if it left
        // anything we do not know, the repository is in use and nothing is deleted.
        if !only_holds(dir, &doomed)? {
            return Ok(Removal::Busy);
        }
        let mut freed = 0;
        for path in &doomed {
            freed += fs::symlink_metadata(path).map_or(0, |meta| meta.len());
            remove_if_present(path)?;
        }
        match fs::remove_dir(dir) {
            Ok(()) => Ok(Removal::Removed(freed)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Removal::Kept),
            Err(error) if error.kind() == ErrorKind::DirectoryNotEmpty => Ok(Removal::Busy),
            Err(error) => fail(format!("remove {}: {error}", dir.display())),
        }
    }
}

/// What a `local/` repository directory holds; see [`Store::scan_local_repo`].
pub struct LocalRepo {
    pub targets: Vec<Target>,
    /// Cache files (`.account.json`, `.repo.json`) deleted with the repository.
    pub extras: Vec<PathBuf>,
    /// Lock files whose document is gone.
    pub stray_locks: Vec<PathBuf>,
    pub presence: Presence,
    /// Seconds since agt last wrote in the directory: `last_seen`, else the newest cache file.
    pub idle_secs: i64,
}

/// When agt last wrote in `dir`, as `last_seen` in `.repo.json`, else the newest modification
/// time of its cache files, else of the directory.
fn last_write(dir: &Path, extras: &[PathBuf]) -> i64 {
    let recorded = fs::read(dir.join(REPO_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|doc| {
            doc["last_seen"]
                .as_str()
                .and_then(|seen| clock::parse(seen).ok())
        });
    recorded
        .or_else(|| extras.iter().filter_map(|path| seen_by_mtime(path)).max())
        .or_else(|| seen_by_mtime(dir))
        .unwrap_or(0)
}

/// Whether every entry of `dir` is one of `paths`.
pub(super) fn only_holds(dir: &Path, paths: &[PathBuf]) -> Result<bool> {
    let Some(entries) = read_dir_opt(dir)? else {
        return Ok(true);
    };
    for entry in entries {
        let entry = entry.map_err(ctx(format!("read {}", dir.display())))?;
        if !paths.contains(&entry.path()) {
            return Ok(false);
        }
    }
    Ok(true)
}
