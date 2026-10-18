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
            });
        } else if let Some(entry) = found.last_mut() {
            if let Some(head) = line.strip_prefix("HEAD ") {
                entry.head = head.to_owned();
            } else if let Some(branch) = line.strip_prefix("branch ") {
                entry.branch = Some(branch.strip_prefix("refs/heads/").unwrap_or(branch).into());
            } else if line == "locked" || line.starts_with("locked ") {
                entry.locked = true;
            }
        }
    }
    Ok(found)
}

/// The default branch's name and its ref: the branch `origin/HEAD` names, else `main`, else
/// `master`, as a local branch when it exists and as `origin/<name>` otherwise.
pub fn default_branch(repo: &Repo) -> Result<Option<(String, String)>> {
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
    for name in names {
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

/// Modified tracked (staged or not, deleted) and untracked, non-ignored file counts.
pub fn dirty(root: &Path) -> Result<(usize, usize)> {
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
    let (mut modified, mut untracked) = (0, 0);
    for entry in out.split(|byte| *byte == 0).filter(|e| e.len() > 3) {
        if entry.starts_with(b"??") {
            untracked += 1;
        } else {
            modified += 1;
        }
    }
    Ok((modified, untracked))
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
