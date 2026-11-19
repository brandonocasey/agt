//! What removing a worktree would delete besides its files. `git worktree remove --force`
//! deletes the worktree's git directory with every submodule clone kept under it, and the
//! snapshot holds neither. Removal is refused unless everything it would delete is held
//! elsewhere: no repository under the worktree that git does not ignore and no submodule
//! clone may hold anything that neither a remote-tracking branch nor the main checkout's
//! clone has. Commits that only a reflog holds are not protected.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::disposable::is_os_junk;
use super::info;
use super::nested;
use crate::core::env;
use crate::core::error::{Result, fail};
use crate::forge::auto::Offline;
use crate::git::{RefTip, Repo, SubmoduleDirt, config_file_value, unfinished_markers};

/// Refuses, naming the repository and the reason, when removing the worktree at `worktree`
/// would delete work that exists nowhere else. Under `--offline` it is [`check_offline`].
pub fn check(worktree: &Path, main: &Path) -> Result<()> {
    check_with(worktree, main, !Offline::current().is_set())
}

/// [`check`] without the network: a tag that no local commit holds counts as unheld, so the
/// answer is never more permissive than [`check`]'s.
pub fn check_offline(worktree: &Path, main: &Path) -> Result<()> {
    check_with(worktree, main, false)
}

fn check_with(worktree: &Path, main: &Path, online: bool) -> Result<()> {
    worktree_state(worktree)?;
    inner_worktrees_may_go(main, worktree)?;
    let mut found = nested::repositories(worktree)?;
    if !found.is_empty() {
        found = nested::without_ignored(found, &info::ignored(worktree)?);
    }
    let strays: Vec<String> = found
        .iter()
        .filter(|found| !found.registered)
        .map(|found| found.rel.clone())
        .collect();
    if !strays.is_empty() {
        return fail(nested::reason(&strays));
    }
    let git_dir = Repo::at(worktree).git_dir()?;
    let main_modules = PathBuf::from(Repo::at(main).common_dir()?).join("modules");
    let worktree_modules = git_dir.join("modules");
    let mut seen = HashSet::new();
    let mut roots = vec![worktree.to_path_buf()];
    roots.extend(found.iter().map(|sub| sub.dir.clone()));
    unfilled_submodules(&roots)?;
    for sub in found {
        let name = format!("submodule {}", sub.rel);
        let gitdir = Repo::at(&sub.dir).git_dir()?;
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
        holds_nothing_unique(&name, &gitdir, Some(&sub.dir), &elsewhere, online)?;
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
        holds_nothing_unique(&name, &clone, workdir.as_deref(), &elsewhere, online)?;
    }
    Ok(())
}

/// Whether a local or remote branch contains the commit `head`.
pub(super) fn held_by_a_branch(shared: &Repo, head: &str) -> Result<bool> {
    Ok(!shared.refs_containing(head, "refs/heads/")?.is_empty()
        || !shared.refs_containing(head, "refs/remotes/")?.is_empty())
}

/// Removal deletes a linked worktree below `worktree` without a backup, so refuses one that
/// is locked, has uncommitted changes, or sits on a commit that no branch holds.
fn inner_worktrees_may_go(main: &Path, worktree: &Path) -> Result<()> {
    let shared = Repo::at(main);
    for inner in nested::linked_inside(main, worktree)? {
        let path = inner.path.display();
        if inner.locked {
            return fail(format!(
                "it holds the locked worktree {path}; unlock it first"
            ));
        }
        if info::dirty(&inner.path)?.0 > 0 {
            return fail(format!(
                "it holds the worktree {path} with uncommitted changes; commit or discard them first"
            ));
        }
        let held =
            inner.branch.is_some() || inner.unborn() || held_by_a_branch(&shared, &inner.head)?;
        if !held {
            return fail(format!(
                "it holds the worktree {path} on a commit that no branch holds; branch it or push it first"
            ));
        }
    }
    Ok(())
}

/// What the worktree's own git directory holds that the snapshot, a tree of the working files,
/// does not: per-worktree refs, an unfinished operation, a staged version that the working file
/// no longer has, and files in a submodule directory that is not initialized.
fn worktree_state(worktree: &Path) -> Result<()> {
    let repo = Repo::at(worktree);
    if let Some(operation) = unfinished(&repo.git_dir()?) {
        return fail(format!(
            "{} has a {operation} in progress; finish or abort it first",
            worktree.display()
        ));
    }
    let own = repo.ref_tips(&["refs/worktree/", "refs/bisect/"])?;
    if let Some(RefTip {
        name: reference, ..
    }) = own.first()
    {
        return fail(format!(
            "{} has the per-worktree ref {reference}, which the backup does not keep; delete it or move its commits to a branch first",
            worktree.display()
        ));
    }
    for entry in repo.status(false, SubmoduleDirt::Report)? {
        let (staged, unstaged) = (entry.staged, entry.unstaged);
        if staged != b' ' && staged != b'?' && unstaged != b' ' && unstaged != b'?' {
            return fail(format!(
                "{} has both staged and unstaged changes: the backup keeps only the working file, so stage or commit them together first",
                entry.path
            ));
        }
    }
    Ok(())
}

/// The unfinished operation (rebase, merge, ...) the git directory records, if any.
fn unfinished(git_dir: &Path) -> Option<String> {
    unfinished_markers(git_dir)
        .first()
        .map(|marker| marker.trim_end_matches("_HEAD").to_lowercase())
}

/// A gitlink directory that is not initialized holds files: git lists none of them and the
/// backup keeps none. The gitlinks are the 160000 entries of each repository in `roots`, so
/// submodules of submodules count and a `.gitmodules` entry over ordinary files does not.
fn unfilled_submodules(roots: &[PathBuf]) -> Result<()> {
    for root in roots {
        for link in Repo::at(root).gitlinks(&[])? {
            let dir = root.join(&link.path);
            if dir.join(".git").exists() {
                continue;
            }
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
                    // Only a file the operating system recreates (a Finder `.DS_Store`) is no
                    // loss: nothing here is ignored, so `build/` or `vendor/` may hold real
                    // work.
                    (dir || !is_os_junk(&name)).then_some(name)
                })
                .collect();
            names.sort();
            if !names.is_empty() {
                let shown = dir
                    .strip_prefix(&roots[0])
                    .unwrap_or(&dir)
                    .to_string_lossy()
                    .into_owned();
                return fail(format!(
                    "submodule {shown} is not initialized but holds files the backup does not keep: {}; move them first",
                    names.join(", ")
                ));
            }
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
    let links = Repo::at(&sub.parent).gitlinks(&[&inside])?;
    Ok(links.into_iter().next().map(|link| link.commit))
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
    let configured = config_file_value(&clone.join("config"), "core.worktree")
        .ok()
        .flatten()?;
    let workdir = fs::canonicalize(clone.join(configured)).ok()?;
    // The path may hold other files by now (a vendored directory in place of the submodule,
    // say): then git there finds the superproject, and the clone has no files to check.
    let found = Repo::at(&workdir).git_dir().ok()?;
    let same = fs::canonicalize(&found).ok()? == fs::canonicalize(clone).ok()?;
    same.then_some(workdir)
}

/// The commits reachable from `roots` and from no remote-tracking branch.
fn beyond_remotes(gitdir: &Path, roots: &[&str], remotes: &[String]) -> Result<Vec<String>> {
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    Repo::at_git_dir(gitdir).commits_beyond(roots, remotes)
}

/// The commits of `commits` that none of the clones in `elsewhere` (git directories or work
/// trees) has.
fn missing_elsewhere(elsewhere: &[PathBuf], commits: Vec<String>) -> Vec<String> {
    let mut missing = commits;
    for clone in elsewhere.iter().filter(|clone| clone.is_dir()) {
        if missing.is_empty() {
            break;
        }
        let absent = Repo::at(clone)
            .missing_objects(&missing)
            .or_else(|_| Repo::at_git_dir(clone).missing_objects(&missing));
        let Ok(absent) = absent else {
            continue;
        };
        missing.retain(|commit| absent.contains(commit));
    }
    missing
}

/// Seconds a remote check may take: 10, or `AGT_LS_REMOTE_TIMEOUT`.
fn remote_timeout() -> u64 {
    env::number("AGT_LS_REMOTE_TIMEOUT").unwrap_or(10)
}

/// The tags the first remote of the repository advertises, as ref name and object id; an error
/// when there is no remote, it cannot be reached, or it takes too long.
fn advertised_tags(gitdir: &Path) -> Result<Vec<(String, String)>> {
    let repo = Repo::at_git_dir(gitdir);
    let remotes = repo.remote_names()?;
    let remote = remotes
        .iter()
        .find(|name| *name == "origin")
        .or_else(|| remotes.first())
        .ok_or_else(|| crate::core::error::Error("it has no remote".to_owned()))?;
    Ok(repo
        .ls_remote_within(remote, remote_timeout())?
        .into_iter()
        .filter(|advertised| advertised.name.starts_with("refs/tags/"))
        .map(|advertised| (advertised.name, advertised.id))
        .collect())
}

/// Refuses when a ref of the repository at `gitdir`, or its HEAD, leads to a commit that no
/// remote-tracking branch, no clone in `elsewhere` and (for a tag) no remote has.
fn commits_held(name: &str, gitdir: &Path, elsewhere: &[PathBuf], online: bool) -> Result<()> {
    let all = Repo::at_git_dir(gitdir).ref_tips(&[])?;
    if all.iter().any(|tip| tip.name == "refs/stash") {
        return fail(format!("{name} has a stash; apply or drop it first"));
    }
    let remotes: Vec<String> = all
        .iter()
        .filter(|tip| tip.name.starts_with("refs/remotes/"))
        .filter_map(|tip| tip.commit.clone())
        .collect();
    let head = Repo::at_git_dir(gitdir).head().ok();
    let mut roots: Vec<(String, String)> = all
        .iter()
        .filter(|tip| !tip.name.starts_with("refs/remotes/"))
        .filter_map(|tip| Some((tip.name.clone(), tip.commit.clone()?)))
        .collect();
    if let Some(head) = &head {
        roots.push(("HEAD".to_owned(), head.clone()));
    }
    let commits: Vec<&str> = roots.iter().map(|(_, commit)| commit.as_str()).collect();
    let unheld = missing_elsewhere(elsewhere, beyond_remotes(gitdir, &commits, &remotes)?);
    if unheld.is_empty() {
        return Ok(());
    }
    let unheld: HashSet<String> = unheld.into_iter().collect();
    // Held by the remote or by a remote-tracking branch: what no root needs to push.
    let mut not = remotes.clone();
    let leads_to_unheld = |roots: &[(String, String)], not: &[String]| -> Result<Option<String>> {
        let commits: Vec<&str> = roots.iter().map(|(_, commit)| commit.as_str()).collect();
        let reach: HashSet<String> = beyond_remotes(gitdir, &commits, not)?
            .into_iter()
            .filter(|commit| unheld.contains(commit))
            .collect();
        Ok(roots
            .iter()
            .find(|(_, commit)| reach.contains(commit))
            .map(|(reference, _)| reference.clone())
            // A root whose own commit is held but that still reaches an unheld one.
            .or_else(|| (!reach.is_empty()).then(|| roots[0].0.clone())))
    };
    // A tag that the remote advertises is held there, whatever the branches say.
    let mut failure = String::new();
    let tags: Vec<&RefTip> = all
        .iter()
        .filter(|tip| tip.name.starts_with("refs/tags/") && tip.commit.is_some())
        .collect();
    let mut kept = roots.clone();
    if tags.iter().any(|tip| {
        tip.commit
            .as_ref()
            .is_some_and(|commit| unheld.contains(commit))
    }) {
        let asked = if online {
            advertised_tags(gitdir)
        } else {
            fail("offline")
        };
        match asked {
            Ok(advertised) => {
                let held: HashSet<&str> = tags
                    .iter()
                    .filter(|tip| {
                        advertised.iter().any(|(reference, object)| {
                            *reference == tip.name && *object == tip.object
                        })
                    })
                    .map(|tip| tip.name.as_str())
                    .collect();
                kept.retain(|(reference, _)| !held.contains(reference.as_str()));
                // Whatever a held tag reaches is on the remote, HEAD at that commit included.
                not.extend(
                    tags.iter()
                        .filter(|tip| held.contains(tip.name.as_str()))
                        .filter_map(|tip| tip.commit.clone()),
                );
            }
            Err(error) => failure = format!(" (the remote could not tell: {})", error.0),
        }
    }
    let Some(reference) = leads_to_unheld(&kept, &not)? else {
        return Ok(());
    };
    if reference == "HEAD" {
        return fail(format!(
            "{name} is on no remote-tracking branch and is not in the main checkout's clone; push it first"
        ));
    }
    let what = match (
        reference.strip_prefix("refs/heads/"),
        reference.strip_prefix("refs/tags/"),
    ) {
        (Some(branch), _) => format!("the local branch {branch}"),
        (_, Some(tag)) => format!("the tag {tag}"),
        _ => format!("the ref {reference}"),
    };
    fail(format!(
        "{name} has {what} that is on no remote-tracking branch and is not in the main checkout's clone{failure}; push it first"
    ))
}

/// Refuses when the repository at `gitdir` (work tree `workdir`, if it has one) holds anything
/// that nothing else holds.
fn holds_nothing_unique(
    name: &str,
    gitdir: &Path,
    workdir: Option<&Path>,
    elsewhere: &[PathBuf],
    online: bool,
) -> Result<()> {
    if let Some(operation) = unfinished(gitdir) {
        return fail(format!(
            "{name} has a {operation} in progress; finish or abort it first"
        ));
    }
    commits_held(name, gitdir, elsewhere, online)?;
    if Repo::at_git_dir(gitdir).worktrees()?.len() > 1 {
        return fail(format!(
            "{name} has linked worktrees of its own; remove them first"
        ));
    }
    let Some(workdir) = workdir.filter(|dir| dir.is_dir()) else {
        return Ok(());
    };
    let work = Repo::at(workdir);
    // A submodule of this one gets its own pass, which filters its junk; here only its commit
    // counts.
    let dirty = work.status(true, SubmoduleDirt::Ignore)?;
    // An untracked file the operating system recreates (a Finder `.DS_Store`) is no work, as
    // at the top level; every other entry, tracked or not, counts.
    let real = dirty.iter().any(|entry| {
        !(entry.untracked() && is_os_junk(entry.path.rsplit('/').next().unwrap_or("")))
    });
    if real {
        return fail(format!("{name} has uncommitted changes"));
    }
    Ok(())
}
