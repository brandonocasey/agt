//! Which task records `agt cleanup` may drop, taking the same lock directory as a task update.

use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use serde_json::Value;

use super::files;
use super::records::{is_id, read_task};
use crate::core::clock;
use crate::core::error::{Result, ctx, fail};
use crate::core::local::Lock;
use crate::store::prune::{PRUNE_AFTER_SECS, Removal};

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
            .is_some_and(|updated| now - updated >= PRUNE_AFTER_SECS)
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
    let root = files::root()?;
    let Ok(_lock) = Lock::acquire(root.join(format!(".{}.lock", task.id)), None, "busy") else {
        return Ok(Removal::Busy);
    };
    match read_task(&task.id) {
        Ok(current) if is_expired(&current, now) => {}
        _ => return Ok(Removal::Kept),
    }
    fs::remove_file(&task.path).map_err(ctx(format!("remove {}", task.path.display())))?;
    Ok(Removal::Removed(task.bytes))
}
