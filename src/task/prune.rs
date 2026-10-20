//! Which task records `agt cleanup` may drop, taking the same lock file as a task update.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::files;
use super::records::{is_id, read_task, task_path};
use crate::core::clock;
use crate::core::error::{Result, ctx, fail};
use crate::core::local::{is_current, lock_file_within, try_lock};
use crate::store::prune::{Removal, past_retention};

/// A finished task record that is old enough to drop.
pub struct Expired {
    pub id: String,
    pub path: PathBuf,
    pub updated_at: String,
    pub bytes: u64,
}

/// Whether the record is `complete` and last updated 90 days or more before `now`. Active,
/// paused and blocked tasks are work in progress and never expire.
fn is_expired(task: &Value, now: i64) -> bool {
    let text = |field: &str| task.get(field).and_then(Value::as_str);
    text("status") == Some("complete")
        && text("updatedAt")
            .or_else(|| text("createdAt"))
            .and_then(|stamp| clock::parse(stamp).ok())
            .is_some_and(|updated| past_retention(now, updated))
}

/// The expired task records, by id. A record that cannot be read is left alone.
pub fn expired(now: i64) -> Result<Vec<Expired>> {
    let root = files::root()?;
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return fail(format!("read {}: {error}", root.display())),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(ctx(format!("read {}", root.display())))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".json").filter(|id| is_id(id)) else {
            continue;
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(task) = read_task(id) else { continue };
        if is_expired(&task, now) {
            found.push(Expired {
                id: id.to_owned(),
                path: entry.path(),
                updated_at: task
                    .get("updatedAt")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                bytes: entry.metadata().map_or(0, |meta| meta.len()),
            });
        }
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(found)
}

/// Deletes the record under its lock after reading it again; gives up when the lock is held.
pub fn remove(task: &Expired, now: i64) -> Result<Removal> {
    let lock = files::lock_path(&task.id)?;
    let Some(_lock) = lock_file_within(&lock, None)? else {
        return Ok(Removal::Busy);
    };
    match read_task(&task.id) {
        Ok(current) if is_expired(&current, now) => {}
        _ => return Ok(Removal::Kept),
    }
    // The lock file stays: unlinking it would let a racing `update` that already read the
    // record open a fresh lock file that no record owns. [`orphan_locks`] collects it later.
    fs::remove_file(&task.path).map_err(ctx(format!("remove {}", task.path.display())))?;
    Ok(Removal::Removed(task.bytes))
}

/// Task lock files whose record does not exist, sorted.
pub fn orphan_locks() -> Result<Vec<PathBuf>> {
    let root = files::root()?;
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return fail(format!("read {}: {error}", root.display())),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(ctx(format!("read {}", root.display())))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name
            .strip_prefix('.')
            .and_then(|name| name.strip_suffix(".flock"))
            .filter(|id| is_id(id))
        else {
            continue;
        };
        if entry.file_type().is_ok_and(|kind| kind.is_file()) && !task_path(id)?.exists() {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

/// Deletes a lock file from [`orphan_locks`] while holding its lock, unless another process
/// holds it, it was replaced, or its record came back. Opening without `create` keeps a lock
/// that vanished gone.
pub fn remove_orphan_lock(path: &Path) -> Result<Removal> {
    let file = match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Removal::Kept),
        Err(error) => return fail(format!("open {}: {error}", path.display())),
    };
    if !try_lock(&file, path)? {
        return Ok(Removal::Busy);
    }
    let id = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix('.')?.strip_suffix(".flock"))
        .filter(|id| is_id(id));
    let Some(id) = id else {
        return Ok(Removal::Kept);
    };
    if !is_current(&file, path)? || task_path(id)?.exists() {
        return Ok(Removal::Kept);
    }
    let bytes = file.metadata().map_or(0, |meta| meta.len());
    fs::remove_file(path).map_err(ctx(format!("remove {}", path.display())))?;
    Ok(Removal::Removed(bytes))
}
