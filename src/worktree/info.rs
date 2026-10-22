//! What git says about a repository's worktrees.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::disposable::is_disposable;
use super::nested;
use crate::core::agents::{Live, inside};
use crate::core::error::{Error, Result, ctx};
use crate::git::{Repo, Worktree, nul_fields};

/// The main checkout: the first entry of `git worktree list`.
pub fn main_checkout(repo: &Repo) -> Result<Worktree> {
    repo.worktrees()?
        .into_iter()
        .next()
        .ok_or_else(|| Error("git worktree list printed nothing".into()))
}

/// The root of the work tree that contains `dir`.
pub fn toplevel(dir: &Path) -> Result<PathBuf> {
    Repo::at(dir)
        .toplevel()
        .map_err(|_| Error(format!("{} is not inside a git work tree", dir.display())))
}

/// The ignored paths in the work tree at `root` and its submodules, sorted, with a directory
/// (ending in `/`) standing for everything in it. A backup keeps none of them.
pub fn ignored(root: &Path) -> Result<Vec<String>> {
    let found = listing(root)?;
    let dirs: HashSet<&str> = found
        .iter()
        .filter(|path| path.ends_with('/'))
        .map(String::as_str)
        .collect();
    Ok(found
        .iter()
        .filter(|path| !parents(path).any(|dir| dirs.contains(dir)))
        .cloned()
        .collect())
}

/// The ignored paths that removing the worktree at `root` deletes for good: those no build or
/// install recreates. A directory stands for everything in it only when none of it is
/// disposable.
pub fn lost(root: &Path) -> Result<Vec<String>> {
    let found = listing(root)?;
    let mut mixed = HashSet::new();
    for path in found.iter().filter(|path| is_disposable(path)) {
        mixed.extend(parents(path));
    }
    let whole: HashSet<&str> = found
        .iter()
        .filter(|path| path.ends_with('/') && !mixed.contains(path.as_str()))
        .map(String::as_str)
        .collect();
    Ok(found
        .iter()
        .filter(|path| {
            !is_disposable(path)
                && !mixed.contains(path.as_str())
                && !parents(path).any(|dir| whole.contains(dir))
        })
        .cloned()
        .collect())
}

/// Every ignored path under `root`, its submodules included, relative to `root` and sorted.
/// Git lists a directory whose files are all ignored and also every path in it.
fn listing(root: &Path) -> Result<Vec<String>> {
    let mut found = listed_in(root, "")?;
    for submodule in nested::repositories(root)?
        .into_iter()
        .filter(|found| found.registered)
    {
        found.extend(listed_in(&submodule.dir, &format!("{}/", submodule.rel))?);
    }
    found.sort();
    found.dedup();
    Ok(found)
}

fn listed_in(dir: &Path, prefix: &str) -> Result<Vec<String>> {
    let listing = Repo::at(dir).checked(
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
        ],
        None,
        &[],
    )?;
    Ok(nul_fields(&listing)
        .map(|entry| format!("{prefix}{}", String::from_utf8_lossy(entry)))
        .collect())
}

/// The directories above `path`, each ending in `/`.
fn parents(path: &str) -> impl Iterator<Item = &str> {
    path.trim_end_matches('/')
        .match_indices('/')
        .map(move |(at, _)| &path[..=at])
}

/// What `git status` lists: whether each path is untracked, and the path.
fn status(root: &Path) -> Result<Vec<(bool, String)>> {
    Ok(Repo::at(root)
        .status(true, &[])?
        .into_iter()
        .map(|entry| (entry.untracked(), entry.path))
        .collect())
}

/// Modified tracked (staged or not, deleted) and untracked, non-ignored file counts.
pub fn dirty(root: &Path) -> Result<(usize, usize)> {
    let listed = status(root)?;
    let untracked = listed.iter().filter(|(untracked, _)| *untracked).count();
    let hidden = Repo::at(root).hidden_edits()?.len();
    Ok((listed.len() - untracked + hidden, untracked))
}

/// Modified tracked files and untracked files that are not disposable: what removing the
/// worktree would only keep in its snapshot.
pub fn unsaved(root: &Path) -> Result<usize> {
    let shown = status(root)?
        .iter()
        .filter(|(untracked, path)| {
            !*untracked || !is_disposable(path) || root.join(path).join(".git").exists()
        })
        .count();
    Ok(shown + Repo::at(root).hidden_edits()?.len())
}

/// One worktree as `agt worktree list` prints it.
pub fn describe(
    repo: &Repo,
    entry: &Worktree,
    default: Option<&(String, String)>,
) -> Result<Value> {
    if !entry.path.exists() {
        return Ok(json!({
            "path": entry.path,
            "branch": entry.branch,
            "head": entry.head,
            "locked": entry.locked,
            "missing": true,
        }));
    }
    let (modified, untracked) = dirty(&entry.path)?;
    let ignored = lost(&entry.path)?.len();
    let (mut ahead, mut behind, mut merged) = (Value::Null, Value::Null, Value::Null);
    // A branch with no commit yet has no HEAD to compare.
    let unborn = entry.unborn();
    if let Some((_, reference)) = default.filter(|_| !unborn) {
        let counts = repo.git(&[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{reference}...{}", entry.head),
        ])?;
        let mut numbers = counts.split_whitespace().map(str::parse::<u64>);
        if let (Some(Ok(left)), Some(Ok(right))) = (numbers.next(), numbers.next()) {
            behind = json!(left);
            ahead = json!(right);
        }
        merged = json!(repo.is_ancestor(&entry.head, reference)?);
    }
    Ok(json!({
        "path": entry.path,
        "branch": entry.branch,
        "detached": entry.branch.is_none(),
        "unborn": unborn,
        "head": entry.head,
        "locked": entry.locked,
        "modified": modified,
        "untracked": untracked,
        "ignored": ignored,
        "default_branch": default.map(|(name, _)| name),
        "ahead": ahead,
        "behind": behind,
        "merged": merged,
    }))
}

/// The live Claude Code sessions whose working directory is inside the worktree.
pub fn agents_in(live: &[Live], path: &Path) -> Value {
    let root = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    let found: Vec<Value> = live
        .iter()
        .filter(|session| inside(&session.cwd, &root))
        .map(|session| {
            let text = |key: &str| session.record.get(key).and_then(Value::as_str);
            json!({
                "pid": session.pid,
                "name": text("name"),
                "status": text("status"),
                "session": text("sessionId"),
            })
        })
        .collect();
    json!(found)
}

/// The `path` argument of a worktree command, else the current directory.
pub fn dir_or_cwd(path: Option<PathBuf>) -> Result<PathBuf> {
    match path {
        Some(path) => Ok(path),
        None => std::env::current_dir().map_err(ctx("read the current directory")),
    }
}

pub fn real(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|error| Error(format!("{}: {error}", path.display())))
}
