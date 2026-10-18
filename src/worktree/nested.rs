//! Nested repositories: a directory with a `.git` of its own that the worktree's repository
//! does not track as a submodule. A snapshot stores one only as a gitlink and removing the
//! worktree deletes its history, so none may be removed.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::core::error::{Result, ctx};
use crate::git::Repo;

/// A directory with a `.git` of its own somewhere under a worktree.
pub struct Found {
    pub dir: PathBuf,
    /// Its path relative to the worktree, `/`-separated.
    pub rel: String,
    /// The root of the repository whose files contain it: the worktree, or the submodule
    /// above it.
    pub parent: PathBuf,
    /// Whether `.gitmodules` of that repository lists it.
    pub registered: bool,
}

/// Every repository under `root`, registered submodules and nested ones alike, at any depth:
/// ignored and disposable directories are searched, and so are the submodules themselves.
pub fn repositories(root: &Path) -> Result<Vec<Found>> {
    let mut found = Vec::new();
    let mut cache = HashMap::new();
    descend(root, "", root, &mut cache, &mut found)?;
    found.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(found)
}

/// Why a worktree with the nested repositories `found` must stay.
pub fn reason(found: &[String]) -> String {
    format!(
        "it contains nested git repositories that a backup cannot hold: {}; move or push them first",
        found.join(", ")
    )
}

pub fn submodule_paths(root: &Path) -> Result<Vec<String>> {
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

fn descend(
    dir: &Path,
    rel: &str,
    parent: &Path,
    cache: &mut HashMap<PathBuf, Vec<String>>,
    found: &mut Vec<Found>,
) -> Result<()> {
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
        let mut owner = parent.to_owned();
        if child.join(".git").exists() {
            if !cache.contains_key(parent) {
                cache.insert(parent.to_owned(), submodule_paths(parent)?);
            }
            let inside = child.strip_prefix(parent).map_or_else(
                |_| String::new(),
                |inside| inside.to_string_lossy().into_owned(),
            );
            found.push(Found {
                dir: child.clone(),
                rel: path.clone(),
                parent: parent.to_owned(),
                registered: cache[parent].contains(&inside),
            });
            owner = child.clone();
        }
        descend(&child, &format!("{path}/"), &owner, cache, found)?;
    }
    Ok(())
}
