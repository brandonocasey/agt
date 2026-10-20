//! Reading and writing the store's JSON files: regular-file checks, the race with renaming
//! writers, durable writes and the check for fields this build would drop.

use std::fs;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::core::error::{Result, ctx, fail};
use crate::core::local::write_atomic;

/// Reads a JSON file that must be a regular file. Writers replace files by rename, so a file
/// swapped between the check and the open is read again; only a path that keeps changing fails.
pub(super) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    const ATTEMPTS: usize = 100;
    for _ in 0..ATTEMPTS {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return fail(format!("stat {}: {error}", path.display())),
        };
        if !meta.file_type().is_file() {
            return fail(format!(
                "{} must be a regular file, not a symbolic link",
                path.display()
            ));
        }
        let mut file = match File::open(path) {
            Ok(file) => file,
            // Replaced by a rename of a file that is gone again, or removed.
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return fail(format!("open {}: {error}", path.display())),
        };
        let opened = file
            .metadata()
            .map_err(ctx(format!("stat {}", path.display())))?;
        if opened.ino() != meta.ino() || opened.dev() != meta.dev() {
            continue;
        }
        let mut raw = String::new();
        file.read_to_string(&mut raw)
            .map_err(ctx(format!("read {}", path.display())))?;
        return serde_json::from_str(&raw).map_err(ctx(format!("parse {}", path.display())));
    }
    fail(format!("{} changed while opening; retry", path.display()))
}

/// The first field of `original` that `written` lacks, as a path.
pub(super) fn lost_field(original: &Value, written: &Value, at: &str) -> Option<String> {
    match (original, written) {
        (Value::Object(original), Value::Object(written)) => {
            original.iter().find_map(|(name, value)| {
                let path = if at.is_empty() {
                    name.clone()
                } else {
                    format!("{at}.{name}")
                };
                match written.get(name) {
                    Some(written) => lost_field(value, written, &path),
                    None => Some(path),
                }
            })
        }
        (Value::Array(original), Value::Array(written)) => original
            .iter()
            .zip(written)
            .enumerate()
            .find_map(|(index, (original, written))| {
                lost_field(original, written, &format!("{at}[{index}]"))
            }),
        _ => None,
    }
}

/// Writes `value` as indented JSON to `path` durably, after `check` passes.
pub(super) fn persist_json(
    path: &Path,
    value: &impl Serialize,
    check: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let mut text =
        serde_json::to_vec_pretty(value).map_err(ctx(format!("serialize {}", path.display())))?;
    text.push(b'\n');
    write_atomic(path, &text, true, check)
}

#[cfg(test)]
mod read_race_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn a_reader_survives_the_file_being_replaced_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.json");
        persist_json(&path, &Value::from(0), || Ok(())).unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer = {
            let (path, stop) = (path.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut n = 0;
                while !stop.load(Ordering::Relaxed) {
                    n += 1;
                    persist_json(&path, &Value::from(n), || Ok(())).unwrap();
                }
            })
        };
        for _ in 0..3000 {
            read_json::<Value>(&path).unwrap().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }
}
