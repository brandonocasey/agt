//! Local auto-merge state in `$XDG_STATE_HOME/agents/pr/local-auto-merge.json`. It holds the
//! waiting intents and the methods `auto-merge off` paused. Each is keyed by the git common
//! directory and the branch. `auto-merge.json` stays the forges' alone.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::core::error::Result;
use crate::core::hash::sha256_hex;
use crate::core::local::{agents_state, lock_file, private_dir, read_json, update_json};

/// A request to merge a branch once its gates pass at `sha`.
#[derive(Clone, Serialize, Deserialize)]
pub struct Intent {
    pub branch: String,
    pub sha: String,
    /// `fast-forward` or `rebase`.
    pub method: String,
    /// The call that is waiting on it.
    /// A newer call replaces it, and the older one stops.
    pub waiter: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Document {
    #[serde(default)]
    intents: BTreeMap<String, Intent>,
    #[serde(default)]
    paused: BTreeMap<String, String>,
}

fn folder() -> Result<PathBuf> {
    let folder = agents_state()?.join("pr");
    private_dir(&folder)?;
    Ok(folder)
}

fn document() -> Result<PathBuf> {
    Ok(folder()?.join("local-auto-merge.json"))
}

/// Holds the lock of `key` until dropped. Code that reads, merges or removes the intent MUST
/// hold it. Take it before the document's own lock.
pub fn lock(key: &str) -> Result<File> {
    let name = format!("local-{}.flock", &sha256_hex(key.as_bytes())[..32]);
    lock_file(&folder()?.join(name))
}

pub fn get(key: &str) -> Result<Option<Intent>> {
    Ok(read_json::<Document>(&document()?)?.and_then(|mut found| found.intents.remove(key)))
}

pub fn set(key: &str, intent: Intent) -> Result<()> {
    update_json::<Document>(&document()?, |found| {
        found.intents.insert(key.to_owned(), intent);
        Ok(true)
    })
    .map(|_| ())
}

/// Removes the intent; false when there was none.
pub fn clear(key: &str) -> Result<bool> {
    update_json::<Document>(&document()?, |found| {
        Ok(found.intents.remove(key).is_some())
    })
}

/// The method `auto-merge off` left for the branch.
pub fn paused(key: &str) -> Result<Option<String>> {
    Ok(read_json::<Document>(&document()?)?.and_then(|mut found| found.paused.remove(key)))
}

pub fn pause(key: &str, method: &str) -> Result<()> {
    update_json::<Document>(&document()?, |found| {
        found.paused.insert(key.to_owned(), method.to_owned());
        Ok(true)
    })
    .map(|_| ())
}

pub fn forget_paused(key: &str) -> Result<()> {
    update_json::<Document>(&document()?, |found| Ok(found.paused.remove(key).is_some()))
        .map(|_| ())
}

/// Drops the intent and the paused method of a branch that merged.
pub fn finish(key: &str) -> Result<()> {
    update_json::<Document>(&document()?, |found| {
        let intent = found.intents.remove(key).is_some();
        let paused = found.paused.remove(key).is_some();
        Ok(intent || paused)
    })
    .map(|_| ())
}
