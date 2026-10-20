//! Reading and writing the store's JSON files: regular-file checks, the race with renaming
//! writers, durable writes and the check for fields this build would drop.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::core::error::{Result, ctx};
use crate::core::local::write_atomic;

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
    use crate::core::local::read_json;

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
