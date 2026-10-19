//! The backups agt made before migrating a namespace, and their guarded deletion.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::super::{Namespace, Store};
use super::Removal;
use crate::core::clock;
use crate::core::error::{Result, ctx, fail};
use crate::core::local::try_lock_file;

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

impl Store {
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
            if name == "repositories" && self.backups == self.agents.join("backups") {
                for clone in fs::read_dir(entry.path()).map_err(ctx("read clone backups"))? {
                    let clone = clone.map_err(ctx("read clone backup"))?;
                    let id = clone.file_name().to_string_lossy().into_owned();
                    if clone
                        .file_type()
                        .map_err(ctx("stat clone backup"))?
                        .is_dir()
                        && id.len() == 40
                        && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                    {
                        let mut store = Self::open(self.namespace)?;
                        store.backups = clone.path();
                        found.extend(store.agt_backups()?);
                    }
                }
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
        let lock_path = backup
            .path
            .parent()
            .unwrap_or(&self.backups)
            .with_file_name(format!(".{name}-backup.lock"));
        let Some(_lock) = try_lock_file(&lock_path)? else {
            return Ok(Removal::Busy);
        };
        match fs::remove_dir_all(&backup.path) {
            Ok(()) => Ok(Removal::Removed(backup.bytes)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Removal::Kept),
            Err(error) => fail(format!("remove {}: {error}", backup.path.display())),
        }
    }
}

/// `reviews-v1-2026-07-01T10-20-30Z` or the same with `-2`: the namespace and when it was made.
pub(super) fn parse_backup_name(name: &str) -> Option<(Namespace, (i64, u32))> {
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
