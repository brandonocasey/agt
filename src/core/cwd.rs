//! `-C` moves away from the directory agt started in. A guard on the caller's directory must
//! check both.

use std::path::PathBuf;
use std::sync::OnceLock;

static STARTED: OnceLock<PathBuf> = OnceLock::new();

/// Records the current directory as the one agt started in. Call it before `-C` moves away.
pub fn record_start() {
    if let Ok(dir) = std::env::current_dir().and_then(|dir| dir.canonicalize()) {
        let _ = STARTED.set(dir);
    }
}

/// The current directory and the one agt started in, resolved. One entry when they match.
pub fn held() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::env::current_dir()
        .and_then(|dir| dir.canonicalize())
        .into_iter()
        .collect();
    if let Some(started) = STARTED.get()
        && !found.contains(started)
    {
        found.push(started.clone());
    }
    found
}
