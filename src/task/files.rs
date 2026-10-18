//! The task state directory.

use std::path::PathBuf;

use crate::core::error::Result;
use crate::core::local::private_dir;
use crate::store::{Namespace, Store};

pub fn root() -> Result<PathBuf> {
    Ok(Store::open(Namespace::Tasks)?.root().to_path_buf())
}

pub fn ensure_root() -> Result<PathBuf> {
    let root = root()?;
    private_dir(&root)?;
    Ok(root)
}
