//! The auto-merge settings `agt pr auto-merge off` paused, so `on` restores the same ones. They
//! live in `$XDG_STATE_HOME/agents/pr/auto-merge.json`, keyed by `<host>/<repo>#<number>`.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crate::core::error::{Result, ctx, fail};
use crate::core::local::{Lock, private_dir, write_private};
use crate::store::{Namespace, Store};

type Paused = BTreeMap<String, String>;

struct Files {
    document: PathBuf,
    lock: PathBuf,
}

fn files() -> Result<Files> {
    let folder = Store::open(Namespace::Tasks)?.agents_dir().join("pr");
    private_dir(&folder)?;
    Ok(Files {
        document: folder.join("auto-merge.json"),
        lock: folder.join("auto-merge.lock"),
    })
}

fn load(files: &Files) -> Result<Paused> {
    match fs::read(&files.document) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(ctx(format!("parse {}", files.document.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Paused::new()),
        Err(error) => fail(format!("read {}: {error}", files.document.display())),
    }
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
    let text = serde_json::to_vec_pretty(&paused).map_err(ctx("serialize paused auto-merge"))?;
    let temporary = files
        .document
        .with_extension(format!("json.{}.tmp", std::process::id()));
    write_private(&files.document, &temporary, &text, false)?;
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
