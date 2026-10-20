//! What git says about a repository's worktrees.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::disposable::is_disposable;
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

/// Ignored files, split into those a backup copies and the disposable paths it skips.
pub struct Ignored {
    pub copy: Vec<String>,
    pub skipped: Vec<String>,
}

pub fn ignored(root: &Path) -> Result<Ignored> {
    let repo = Repo::at(root);
    let listing = repo.checked(
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
    let mut found = Ignored {
        copy: Vec::new(),
        skipped: Vec::new(),
    };
    for entry in nul_fields(&listing) {
        let entry = String::from_utf8_lossy(entry).into_owned();
        walk(root, &entry, &mut found)?;
    }
    found.copy.sort();
    found.copy.dedup();
    found.skipped.sort();
    found.skipped.dedup();
    Ok(found)
}

/// Adds `rel` (a file, or a directory ending in `/`) to `found`, descending into directories
/// that are not disposable.
fn walk(root: &Path, rel: &str, found: &mut Ignored) -> Result<()> {
    if is_disposable(rel) {
        found.skipped.push(rel.to_owned());
        return Ok(());
    }
    if !rel.ends_with('/') {
        found.copy.push(rel.to_owned());
        return Ok(());
    }
    let dir = root.join(rel);
    for child in fs::read_dir(&dir).map_err(ctx(format!("read {}", dir.display())))? {
        let child = child.map_err(ctx(format!("read {}", dir.display())))?;
        let kind = child
            .file_type()
            .map_err(ctx(format!("stat {}", child.path().display())))?;
        let name = child.file_name().to_string_lossy().into_owned();
        if kind.is_dir() {
            walk(root, &format!("{rel}{name}/"), found)?;
        } else if kind.is_file() || kind.is_symlink() {
            walk(root, &format!("{rel}{name}"), found)?;
        }
    }
    Ok(())
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
    let ignored = ignored(&entry.path)?.copy.len();
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
