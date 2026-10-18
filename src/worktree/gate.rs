//! What removing a worktree would delete besides its files. `git worktree remove --force`
//! deletes the worktree's git directory with every submodule clone kept under it, and the
//! snapshot holds neither. Removal is refused unless everything it would delete is held
//! elsewhere: no repository under the worktree and no submodule clone may hold anything that
//! neither a remote-tracking branch nor the main checkout's clone has. Commits that only a
//! reflog holds are not protected.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::{info, nested};
use crate::core::error::{Result, fail};
use crate::git::Repo;

/// Files in a git directory that mark an unfinished rebase, merge, cherry-pick, revert or
/// bisect.
const UNFINISHED: [&str; 8] = [
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "sequencer",
    "BISECT_START",
];

/// Refuses, naming the repository and the reason, when removing the worktree at `worktree`
/// would delete work that exists nowhere else.
pub fn check(worktree: &Path, main: &Path) -> Result<()> {
    worktree_state(worktree)?;
    let found = nested::repositories(worktree)?;
    let strays: Vec<String> = found
        .iter()
        .filter(|found| !found.registered)
        .map(|found| found.rel.clone())
        .collect();
    if !strays.is_empty() {
        return fail(nested::reason(&strays));
    }
    let git_dir = PathBuf::from(Repo::at(worktree).git(&["rev-parse", "--absolute-git-dir"])?);
    let main_modules = PathBuf::from(Repo::at(main).git(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ])?)
    .join("modules");
    let worktree_modules = git_dir.join("modules");
    let mut seen = HashSet::new();
    for sub in found {
        let name = format!("submodule {}", sub.rel);
        let gitdir = PathBuf::from(Repo::at(&sub.dir).git(&["rev-parse", "--absolute-git-dir"])?);
        seen.insert(fs::canonicalize(&gitdir).unwrap_or_else(|_| gitdir.clone()));
        if let Some(recorded) = recorded_commit(&sub)?
            && Repo::at(&sub.dir).head().ok().as_deref() != Some(recorded.as_str())
        {
            return fail(format!(
                "{name} is not at the commit the worktree records; check it in or reset it first"
            ));
        }
        let mut elsewhere = vec![main.join(&sub.rel)];
        if let Ok(inside) = gitdir.strip_prefix(&worktree_modules) {
            elsewhere.push(main_modules.join(inside));
        }
        holds_nothing_unique(&name, &gitdir, Some(&sub.dir), &elsewhere)?;
    }
    for clone in clones(&worktree_modules) {
        let canonical = fs::canonicalize(&clone).unwrap_or_else(|_| clone.clone());
        if seen.contains(&canonical) {
            continue;
        }
        let name = format!("submodule clone {}", clone.display());
        let mut elsewhere = Vec::new();
        if let Ok(inside) = clone.strip_prefix(&worktree_modules) {
            elsewhere.push(main_modules.join(inside));
        }
        let workdir = workdir_of(&clone);
        holds_nothing_unique(&name, &clone, workdir.as_deref(), &elsewhere)?;
    }
    Ok(())
}

/// What the worktree's own git directory holds that the snapshot, a tree of the working files,
/// does not: per-worktree refs, an unfinished operation, a staged version that the working file
/// no longer has, and files in a submodule directory that is not initialized.
fn worktree_state(worktree: &Path) -> Result<()> {
    let repo = Repo::at(worktree);
    let git_dir = PathBuf::from(repo.git(&["rev-parse", "--absolute-git-dir"])?);
    for marker in UNFINISHED {
        if git_dir.join(marker).exists() {
            return fail(format!(
                "{} has a {} in progress; finish or abort it first",
                worktree.display(),
                marker.trim_end_matches("_HEAD").to_lowercase()
            ));
        }
    }
    let own = repo.git(&[
        "for-each-ref",
        "--format=%(refname)",
        "refs/worktree",
        "refs/bisect",
    ])?;
    if let Some(reference) = own.lines().next() {
        return fail(format!(
            "{} has the per-worktree ref {reference}, which the backup does not keep; delete it or move its commits to a branch first",
            worktree.display()
        ));
    }
    let status = repo.checked(
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--no-renames",
            "--untracked-files=no",
        ],
        None,
        &[],
    )?;
    for entry in status
        .split(|byte| *byte == 0)
        .filter(|entry| entry.len() > 3)
    {
        let (staged, unstaged) = (entry[0], entry[1]);
        if staged != b' ' && staged != b'?' && unstaged != b' ' && unstaged != b'?' {
            return fail(format!(
                "{} has both staged and unstaged changes: the backup keeps only the working file, so stage or commit them together first",
                String::from_utf8_lossy(&entry[3..])
            ));
        }
    }
    unfilled_submodules(worktree)
}

/// A submodule directory that is not initialized yet holds files: git lists none of them and
/// the backup keeps none.
fn unfilled_submodules(worktree: &Path) -> Result<()> {
    for path in nested::submodule_paths(worktree)? {
        let dir = worktree.join(&path);
        if dir.join(".git").exists() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        if !names.is_empty() {
            return fail(format!(
                "submodule {path} is not initialized but holds files the backup does not keep: {}; move them first",
                names.join(", ")
            ));
        }
    }
    Ok(())
}

/// The commit the repository holding the submodule records for it, if it records one.
fn recorded_commit(sub: &nested::Found) -> Result<Option<String>> {
    let inside = sub
        .dir
        .strip_prefix(&sub.parent)
        .map(|inside| inside.to_string_lossy().into_owned())
        .unwrap_or_default();
    let listing = Repo::at(&sub.parent).git(&["ls-files", "--stage", "--", &inside])?;
    Ok(listing.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some("160000")).then(|| fields.next().map(str::to_owned))?
    }))
}

/// Every clone below `modules`, at any depth: a directory with a `HEAD` and `objects`, and the
/// clones kept inside it in turn.
fn clones(modules: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(modules) else {
        return found;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths.into_iter().filter(|path| path.is_dir()) {
        if path.join("HEAD").is_file() && path.join("objects").is_dir() {
            found.extend(clones(&path.join("modules")));
            found.push(path);
        } else {
            found.extend(clones(&path));
        }
    }
    found
}

/// The work tree of a clone: its `core.worktree`, resolved against the clone.
fn workdir_of(clone: &Path) -> Option<PathBuf> {
    let configured = Repo::at(clone)
        .git_opt(&["config", "--file", "config", "core.worktree"])
        .ok()
        .flatten()?;
    fs::canonicalize(clone.join(configured)).ok()
}

/// `git <args>` against the repository whose git directory is `gitdir`, even when its
/// `core.worktree` names a directory that is gone: the work tree is given on the command line.
fn plumbing(gitdir: &Path, args: &[&str]) -> Result<String> {
    let dir = gitdir.to_string_lossy();
    let mut all = vec!["--git-dir", &dir, "--work-tree", &dir];
    all.extend_from_slice(args);
    Repo::at(gitdir).git(&all)
}

/// Whether `commit` is on a remote-tracking branch of the repository at `gitdir` or in one of
/// the clones in `elsewhere` (git directories or work trees).
fn held_elsewhere(gitdir: &Path, elsewhere: &[PathBuf], commit: &str) -> Result<bool> {
    if !plumbing(gitdir, &["branch", "-r", "--contains", commit])?.is_empty() {
        return Ok(true);
    }
    let spec = format!("{commit}^{{commit}}");
    for clone in elsewhere {
        if clone.is_dir() && Repo::at(clone).object_exists(&spec).unwrap_or(false) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Refuses when the repository at `gitdir` (work tree `workdir`, if it has one) holds anything
/// that nothing else holds.
fn holds_nothing_unique(
    name: &str,
    gitdir: &Path,
    workdir: Option<&Path>,
    elsewhere: &[PathBuf],
) -> Result<()> {
    for marker in UNFINISHED {
        if gitdir.join(marker).exists() {
            return fail(format!(
                "{name} has a {} in progress; finish or abort it first",
                marker.trim_end_matches("_HEAD").to_lowercase()
            ));
        }
    }
    let refs = plumbing(
        gitdir,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
    )?;
    for line in refs.lines() {
        let Some((tip, reference)) = line.split_once(' ') else {
            continue;
        };
        if reference.starts_with("refs/remotes/") {
            continue;
        }
        if reference == "refs/stash" {
            return fail(format!("{name} has a stash; apply or drop it first"));
        }
        if !held_elsewhere(gitdir, elsewhere, tip)? {
            let what = match reference.strip_prefix("refs/heads/") {
                Some(branch) => format!("the local branch {branch}"),
                None => format!("the ref {reference}"),
            };
            return fail(format!(
                "{name} has {what} that is on no remote-tracking branch and is not in the main checkout's clone; push it first"
            ));
        }
    }
    let head = plumbing(
        gitdir,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
    )
    .ok()
    .filter(|head| !head.is_empty());
    if let Some(head) = head
        && !held_elsewhere(gitdir, elsewhere, &head)?
    {
        return fail(format!(
            "{name} is on no remote-tracking branch and is not in the main checkout's clone; push it first"
        ));
    }
    let worktrees = plumbing(gitdir, &["worktree", "list", "--porcelain"])?;
    if worktrees
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count()
        > 1
    {
        return fail(format!(
            "{name} has linked worktrees of its own; remove them first"
        ));
    }
    let Some(workdir) = workdir.filter(|dir| dir.is_dir()) else {
        return Ok(());
    };
    let work = Repo::at(workdir);
    let dirty = work.git(&[
        "-c",
        "status.showUntrackedFiles=all",
        "status",
        "--porcelain",
        "--untracked-files=all",
        "--ignore-submodules=none",
    ])?;
    if !dirty.is_empty() {
        return fail(format!("{name} has uncommitted changes"));
    }
    let ignored = info::ignored(workdir)?.copy;
    if !ignored.is_empty() {
        return fail(format!(
            "{name} has ignored files that no backup keeps: {}; move them first",
            ignored.join(", ")
        ));
    }
    Ok(())
}
