//! The backup of a namespace taken before its first document is overwritten by a newer schema.

use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::Ordering;

use serde_json::Value;

use super::json::read_json;
use super::{Store, create_dir};
use crate::core::error::{Result, ctx, fail};
use crate::core::local::{lock_file, sync_dir};

impl Store {
    /// Copies the namespace to `backups/<namespace>-v<old version>-<timestamp>/` before the first
    /// overwrite of a document written under an older schema version. One backup per namespace
    /// and old version, across processes: a later run that finds one does not copy again.
    pub(super) fn backup_before_migration(&self, path: &Path) -> Result<()> {
        if self.backed_up.load(Ordering::Acquire) {
            return Ok(());
        }
        let stored = read_json::<Value>(path)?
            .and_then(|document| document.get("version").and_then(Value::as_u64));
        let Some(stored) = stored.filter(|version| *version < u64::from(self.schema.version))
        else {
            return Ok(());
        };
        let name = self.namespace.dir_name();
        let prefix = format!("{name}-v{stored}-");
        create_dir(&self.backups)?;
        // Beside the backups, not in them, so the backups directory holds only backups.
        let lock_path = self.backups.with_file_name(format!(".{name}-backup.lock"));
        // One backup at a time per namespace, across processes: the check, the copy and the
        // rename are one step, so no run deletes or renames another's copy.
        let _guard = lock_file(&lock_path)?;
        let done = fs::read_dir(&self.backups).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        });
        if done {
            self.backed_up.store(true, Ordering::Release);
            return Ok(());
        }
        // Copies that failed part way are no backup; under the lock none is in progress.
        let leftover = format!(".{prefix}");
        for entry in fs::read_dir(&self.backups)
            .map_err(ctx(format!("read {}", self.backups.display())))?
            .flatten()
        {
            let file = entry.file_name().to_string_lossy().into_owned();
            if file.starts_with(&leftover) && file.ends_with(".partial") {
                fs::remove_dir_all(entry.path())
                    .map_err(ctx(format!("remove {}", entry.path().display())))?;
            }
        }
        let stamp = crate::core::clock::now()?.replace(':', "-");
        let mut target = self.backups.join(format!("{prefix}{stamp}"));
        let mut attempt = 1;
        while target.exists() {
            attempt += 1;
            target = self.backups.join(format!("{prefix}{stamp}-{attempt}"));
        }
        // Copy under a hidden name and rename when complete, so a failed copy never looks
        // like a finished backup to the next run.
        let partial = self.backups.join(format!(
            ".{}.partial",
            target.file_name().unwrap_or_default().to_string_lossy()
        ));
        copy_tree(&self.root, &partial)?;
        fs::rename(&partial, &target).map_err(ctx(format!(
            "rename {} to {}",
            partial.display(),
            target.display()
        )))?;
        sync_dir(&self.backups)?;
        self.backed_up.store(true, Ordering::Release);
        Ok(())
    }
}

/// Copies regular files and directories from `from` into the new directory `to`, skipping lock
/// files and anything that is not a plain file or directory.
pub(super) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    create_dir(to)?;
    let entries = match fs::read_dir(from) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return fail(format!("read {}: {error}", from.display())),
    };
    for entry in entries {
        let entry = entry.map_err(ctx(format!("read {}", from.display())))?;
        let kind = entry
            .file_type()
            .map_err(ctx(format!("stat {}", entry.path().display())))?;
        let destination = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &destination)?;
        } else if kind.is_file() && !entry.file_name().to_string_lossy().ends_with(".lock") {
            fs::copy(entry.path(), &destination)
                .map_err(ctx(format!("copy {}", entry.path().display())))?;
            File::open(&destination)
                .and_then(|copied| copied.sync_all())
                .map_err(ctx(format!("sync {}", destination.display())))?;
        }
    }
    sync_dir(to)
}
