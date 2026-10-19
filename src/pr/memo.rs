//! The auto-merge settings `agt pr auto-merge off` paused, so `on` restores the same ones. They
//! live in `$XDG_STATE_HOME/agents/pr/auto-merge.json`, keyed by `<host>/<repo>#<number>`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::core::error::Result;
use crate::core::local::{Lock, agents_state, private_dir, read_json, write_json};

type Paused = BTreeMap<String, String>;

struct Files {
    document: PathBuf,
    lock: PathBuf,
}

fn files() -> Result<Files> {
    let folder = agents_state()?.join("pr");
    private_dir(&folder)?;
    Ok(Files {
        document: folder.join("auto-merge.json"),
        lock: folder.join("auto-merge.lock"),
    })
}

fn load(files: &Files) -> Result<Paused> {
    Ok(read_json(&files.document)?.unwrap_or_default())
}

/// Reads, changes and writes the document under its lock.
fn edit<T>(change: impl FnOnce(&mut Paused) -> T) -> Result<T> {
    let files = files()?;
    let _lock = Lock::acquire(
        files.lock.clone(),
        Some(Duration::from_secs(5)),
        "another agt pr command is changing the paused auto-merge records",
    )?;
    let mut paused = load(&files)?;
    let result = change(&mut paused);
    write_json(&files.document, &paused, false)?;
    Ok(result)
}

pub fn record(key: &str, method: &str) -> Result<()> {
    edit(|paused| paused.insert(key.to_owned(), method.to_owned())).map(|_| ())
}

pub fn recorded(key: &str) -> Result<Option<String>> {
    Ok(load(&files()?)?.remove(key))
}

pub fn forget(key: &str) -> Result<()> {
    edit(|paused| paused.remove(key)).map(|_| ())
}
