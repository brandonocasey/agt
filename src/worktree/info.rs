//! What git says about a repository's worktrees.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::disposable::is_disposable;
use crate::core::error::{Error, Result, ctx};
use crate::git::Repo;

pub struct Entry {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub locked: bool,
    pub bare: bool,
}

/// Every worktree of the repository; the first is the main checkout.
pub fn entries(repo: &Repo) -> Result<Vec<Entry>> {
    let listing = repo.git(&["worktree", "list", "--porcelain"])?;
    let mut found: Vec<Entry> = Vec::new();
    for line in listing.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            found.push(Entry {
                path: PathBuf::from(path),
                head: String::new(),
                branch: None,
                locked: false,
                bare: false,
            });
        } else if let Some(entry) = found.last_mut() {
            if let Some(head) = line.strip_prefix("HEAD ") {
                entry.head = head.to_owned();
            } else if let Some(branch) = line.strip_prefix("branch ") {
                entry.branch = Some(branch.strip_prefix("refs/heads/").unwrap_or(branch).into());
            } else if line == "locked" || line.starts_with("locked ") {
                entry.locked = true;
            } else if line == "bare" {
                entry.bare = true;
            }
        }
    }
    Ok(found)
}

/// The main checkout: the first entry of `git worktree list`.
pub fn main_checkout(repo: &Repo) -> Result<Entry> {
    entries(repo)?
        .into_iter()
        .next()
        .ok_or_else(|| Error("git worktree list printed nothing".into()))
}

/// The root of the work tree that contains `dir`.
pub fn toplevel(dir: &Path) -> Result<PathBuf> {
    let root = Repo::at(dir)
        .git(&["rev-parse", "--show-toplevel"])
        .map_err(|_| Error(format!("{} is not inside a git work tree", dir.display())))?;
    Ok(PathBuf::from(root))
}

/// The names the default branch may have, best first: the branch `origin/HEAD` names, `main`,
/// `master`.
pub fn default_names(repo: &Repo) -> Result<Vec<String>> {
    let mut names = Vec::new();
    if let Some(remote) = repo.git_opt(&[
        "symbolic-ref",
        "--quiet",
        "--short",
        "refs/remotes/origin/HEAD",
    ])? {
        names.push(remote.strip_prefix("origin/").unwrap_or(&remote).to_owned());
    }
    names.extend(["main".to_owned(), "master".to_owned()]);
    Ok(names)
}

/// The default branch's name and its ref: the first of [`default_names`] that exists, as a
/// local branch when it does and as `origin/<name>` otherwise.
pub fn default_branch(repo: &Repo) -> Result<Option<(String, String)>> {
    for name in default_names(repo)? {
        for reference in [
            format!("refs/heads/{name}"),
            format!("refs/remotes/origin/{name}"),
        ] {
            let spec = format!("{reference}^{{commit}}");
            if repo
                .git_opt(&["rev-parse", "--verify", "--quiet", &spec])?
                .is_some()
            {
                return Ok(Some((name, reference)));
            }
        }
    }
    Ok(None)
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
    for entry in listing.split(|byte| *byte == 0).filter(|e| !e.is_empty()) {
        let entry = String::from_utf8_lossy(entry).into_owned();
        walk(root, &entry, &mut found)?;
    }
    found.copy.sort();
    found.skipped.sort();
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
    let out = Repo::at(root).checked(
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
        None,
        &[],
    )?;
    Ok(out
        .split(|byte| *byte == 0)
        .filter(|e| e.len() > 3)
        .map(|entry| {
            (
                entry.starts_with(b"??"),
                String::from_utf8_lossy(&entry[3..]).into_owned(),
            )
        })
        .collect())
}

/// Modified tracked (staged or not, deleted) and untracked, non-ignored file counts.
pub fn dirty(root: &Path) -> Result<(usize, usize)> {
    let listed = status(root)?;
    let untracked = listed.iter().filter(|(untracked, _)| *untracked).count();
    Ok((listed.len() - untracked, untracked))
}

/// Modified tracked files and untracked files that are not disposable: what removing the
/// worktree would only keep in its snapshot.
pub fn unsaved(root: &Path) -> Result<usize> {
    Ok(status(root)?
        .iter()
        .filter(|(untracked, path)| !*untracked || !is_disposable(path))
        .count())
}

/// One worktree as `agt worktree list` prints it.
pub fn describe(repo: &Repo, entry: &Entry, default: Option<&(String, String)>) -> Result<Value> {
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
    if let Some((_, reference)) = default {
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

pub fn real(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).map_err(|error| Error(format!("{}: {error}", path.display())))
}
