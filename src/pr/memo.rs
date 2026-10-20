//! The auto-merge settings `agt pr auto-merge off` paused, so `on` restores the same ones. They
//! live in `$XDG_STATE_HOME/agents/pr/auto-merge.json`, keyed by `<host>/<repo>#<number>`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::core::error::Result;
use crate::core::local::{agents_state, private_dir, read_json, update_json};

type Paused = BTreeMap<String, String>;

fn document() -> Result<PathBuf> {
    let folder = agents_state()?.join("pr");
    private_dir(&folder)?;
    Ok(folder.join("auto-merge.json"))
}

pub fn record(key: &str, method: &str) -> Result<()> {
    update_json::<Paused>(&document()?, |paused| {
        paused.insert(key.to_owned(), method.to_owned());
        Ok(true)
    })
    .map(|_| ())
}

pub fn recorded(key: &str) -> Result<Option<String>> {
    Ok(read_json::<Paused>(&document()?)?.and_then(|mut paused| paused.remove(key)))
}

pub fn forget(key: &str) -> Result<()> {
    update_json::<Paused>(&document()?, |paused| Ok(paused.remove(key).is_some())).map(|_| ())
}
