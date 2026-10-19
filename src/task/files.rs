//! The task state directory.

use std::path::PathBuf;

use crate::core::error::Result;
use crate::core::local::{private_dir, tasks_dir};

pub fn root() -> Result<PathBuf> {
    tasks_dir()
}

pub fn ensure_root() -> Result<PathBuf> {
    let root = root()?;
    private_dir(&root)?;
    Ok(root)
}
