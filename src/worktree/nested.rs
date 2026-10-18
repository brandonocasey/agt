//! Nested repositories: a directory with a `.git` of its own that the worktree's repository
//! does not track as a submodule. A snapshot stores one only as a gitlink and removing the
//! worktree deletes its history, so none may be removed.

use std::fs;
use std::path::Path;

use crate::core::error::{Result, ctx, fail};
use crate::git::Repo;

/// The nested repositories under `root`, as sorted `/`-separated paths relative to it. Ignored
/// and disposable directories are searched too; registered submodules are not descended into.
pub fn find(root: &Path) -> Result<Vec<String>> {
    let registered = submodule_paths(root)?;
    let mut found = Vec::new();
    descend(root, "", &registered, &mut found)?;
    found.sort();
    Ok(found)
}

/// Refuses when the worktree at `root` holds a nested repository, naming each path.
pub fn check(root: &Path) -> Result<()> {
    let found = find(root)?;
    if found.is_empty() {
        return Ok(());
    }
    fail(reason(&found))
}

/// Why a worktree with the nested repositories `found` must stay.
pub fn reason(found: &[String]) -> String {
    format!(
        "it contains nested git repositories that a backup cannot hold: {}; move or push them first",
        found.join(", ")
    )
}

fn submodule_paths(root: &Path) -> Result<Vec<String>> {
    if !root.join(".gitmodules").is_file() {
        return Ok(Vec::new());
    }
    // NUL ends an entry and the first newline splits key from value, so a submodule name or
    // path with spaces reads back whole.
    let listing = Repo::at(root).git_opt(&[
        "config",
        "-z",
        "--file",
        ".gitmodules",
        "--get-regexp",
        r"^submodule\..*\.path$",
    ])?;
    Ok(listing
        .unwrap_or_default()
        .split('\0')
        .filter_map(|entry| entry.split_once('\n').map(|(_, path)| path.to_owned()))
        .collect())
}

fn descend(dir: &Path, rel: &str, registered: &[String], found: &mut Vec<String>) -> Result<()> {
    for child in fs::read_dir(dir).map_err(ctx(format!("read {}", dir.display())))? {
        let child = child.map_err(ctx(format!("read {}", dir.display())))?;
        let name = child.file_name().to_string_lossy().into_owned();
        let is_dir = child
            .file_type()
            .map_err(ctx(format!("stat {}", child.path().display())))?
            .is_dir();
        if !is_dir || name == ".git" {
            continue;
        }
        let path = format!("{rel}{name}");
        let child = child.path();
        if child.join(".git").exists() {
            if !registered.contains(&path) {
                found.push(path);
            }
            continue;
        }
        descend(&child, &format!("{path}/"), registered, found)?;
    }
    Ok(())
}
