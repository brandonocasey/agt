//! The task state directory.

use std::path::PathBuf;

use crate::core::error::Result;
use crate::core::local::{private_dir, tasks_dir};

pub fn root() -> Result<PathBuf> {
    tasks_dir()
}

/// The lock file that serialises changes to the task `id`.
pub fn lock_path(id: &str) -> Result<PathBuf> {
    Ok(root()?.join(format!(".{id}.flock")))
}

pub fn ensure_root() -> Result<PathBuf> {
    let root = root()?;
    private_dir(&root)?;
    Ok(root)
}
