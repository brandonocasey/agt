//! Nested repositories: a directory with a `.git` of its own that the worktree's repository
//! does not track as a submodule. A snapshot stores one only as a gitlink and removing the
//! worktree deletes its history, so none may be removed, unless git ignores it: a backup holds
//! nothing ignored. The same walk finds directories git could not empty.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::info::parents;
use crate::core::error::{Error, Result, ctx};
use crate::core::local;
use crate::git::{Repo, Worktree};

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

/// The repositories of `found`, sorted by path, that git does not ignore and that do not lie
/// inside one it ignores. `ignored` holds the worktree's ignored paths, a directory ending in
/// `/`: removal deletes a repository inside one with the worktree, and a backup never holds
/// anything ignored. A registered submodule stays, as `.gitmodules` may list one the index
/// no longer holds.
pub fn without_ignored(found: Vec<Found>, ignored: &[String]) -> Vec<Found> {
    let dirs: HashSet<&str> = ignored
        .iter()
        .filter(|path| path.ends_with('/'))
        .map(String::as_str)
        .collect();
    let is_ignored = |repo: &Found, dir: &str| {
        !repo.registered
            // A name that is not valid UTF-8 was read lossily and can equal another name.
            && !repo.rel.contains(char::REPLACEMENT_CHARACTER)
            && (dirs.contains(dir) || parents(dir).any(|above| dirs.contains(above)))
    };
    let mut dropped: Vec<String> = Vec::new();
    found
        .into_iter()
        .filter(|repo| {
            let dir = format!("{}/", repo.rel);
            let drop = dropped.iter().any(|above| dir.starts_with(above.as_str()))
                || is_ignored(repo, &dir);
            if drop {
                dropped.push(dir);
            }
            !drop
        })
        .collect()
}

/// The linked worktrees below `path`, deepest first. Only an ignored folder may hold one:
/// removing `path` deletes it, so git deregisters each first.
pub fn linked_inside(main: &Path, path: &Path) -> Result<Vec<Worktree>> {
    let mut inner: Vec<Worktree> = Repo::at(main)
        .worktrees()?
        .into_iter()
        .filter(|other| !other.prunable && other.path != path && other.path.starts_with(path))
        .collect();
    inner.sort_by_key(|other| std::cmp::Reverse(other.path.components().count()));
    Ok(inner)
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
    // Git stops at the first directory it cannot empty, after it deleted part of the worktree
    // and its registration, so such a directory refuses removal, with what to do about it.
    let stuck = |cause: String| {
        Error(format!(
            "cannot delete what is in {}: {cause}; git would stop partway through removing the \
             worktree, so run chmod -R u+rwX on it (or chown it back when another user owns \
             it) or delete it, then retry",
            dir.display()
        ))
    };
    let children = match fs::read_dir(dir) {
        Ok(children) => children,
        // Gone since its parent was listed: nothing in it to delete.
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            return Err(stuck(error.to_string()));
        }
        Err(error) => return Err(ctx(format!("read {}", dir.display()))(error)),
    };
    if !local::writable(dir) {
        return Err(stuck("it is not writable".into()));
    }
    for child in children {
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
