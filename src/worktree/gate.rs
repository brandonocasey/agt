//! What removing a worktree would delete besides its files. `git worktree remove --force`
//! deletes the worktree's git directory with every submodule clone kept under it, and the
//! snapshot holds neither. Removal is refused unless everything it would delete is held
//! elsewhere: no repository under the worktree and no submodule clone may hold anything that
//! neither a remote-tracking branch nor the main checkout's clone has. Commits that only a
//! reflog holds are not protected.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::disposable::is_os_junk;
use super::{info, nested};
use crate::core::env;
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
    check_with(worktree, main, true)
}

/// [`check`] without the network: a tag that no local commit holds counts as unheld, so the
/// answer is never more permissive than [`check`]'s.
pub fn check_offline(worktree: &Path, main: &Path) -> Result<()> {
    check_with(worktree, main, false)
}

fn check_with(worktree: &Path, main: &Path, online: bool) -> Result<()> {
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
    let mut roots = vec![worktree.to_path_buf()];
    roots.extend(found.iter().map(|sub| sub.dir.clone()));
    unfilled_submodules(&roots)?;
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
    Ok(())
}

/// A gitlink directory that is not initialized holds files: git lists none of them and the
/// backup keeps none. The gitlinks are the 160000 entries of each repository in `roots`, so
/// submodules of submodules count and a `.gitmodules` entry over ordinary files does not.
fn unfilled_submodules(roots: &[PathBuf]) -> Result<()> {
    for root in roots {
        let listing = Repo::at(root).checked(&["ls-files", "--stage", "-z"], None, &[])?;
        for entry in listing.split(|byte| *byte == 0) {
            let entry = String::from_utf8_lossy(entry);
            let Some((meta, path)) = entry.split_once('\t') else {
                continue;
            };
            if !meta.starts_with("160000 ") {
                continue;
            }
            let dir = root.join(path);
            if dir.join(".git").exists() {
                continue;
            }
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .flatten()
                .map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
                    if dir { format!("{name}/") } else { name }
                })
                // Only a file the operating system recreates (a Finder `.DS_Store`) is no loss:
                // nothing here is ignored, so `build/` or `vendor/` may hold real work.
                .filter(|name| !is_os_junk(name))
                .map(|name| name.trim_end_matches('/').to_owned())
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
    let workdir = fs::canonicalize(clone.join(configured)).ok()?;
    // The path may hold other files by now (a vendored directory in place of the submodule,
    // say): then git there finds the superproject, and the clone has no files to check.
    let found = Repo::at(&workdir)
        .git(&["rev-parse", "--absolute-git-dir"])
        .ok()?;
    let same = fs::canonicalize(&found).ok()? == fs::canonicalize(clone).ok()?;
    same.then_some(workdir)
}

/// `git <args>` against the repository whose git directory is `gitdir`, even when its
/// `core.worktree` names a directory that is gone: the work tree is given on the command line.
fn plumbing(gitdir: &Path, args: &[&str]) -> Result<String> {
    crate::git::text(plumbing_bytes(gitdir, args)?)
}

fn plumbing_bytes(gitdir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let dir = gitdir.to_string_lossy();
    let mut all = vec!["--git-dir", &dir, "--work-tree", &dir];
    all.extend_from_slice(args);
    Repo::at(gitdir).checked(&all, None, &[])
}

/// A ref of a repository, with the commit it leads to (none for a tag of a blob or tree).
struct Tip {
    reference: String,
    object: String,
    commit: Option<String>,
}

/// Every ref of the repository at `gitdir`, with its commit.
fn tips(gitdir: &Path) -> Result<Vec<Tip>> {
    let listing = plumbing(
        gitdir,
        &[
            "for-each-ref",
            "--format=%(objecttype) %(*objecttype) %(objectname) %(*objectname) %(refname)",
        ],
    )?;
    Ok(listing
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            let [kind, peeled_kind, object, peeled, reference] = fields[..] else {
                return None;
            };
            let commit = match (kind, peeled_kind) {
                ("commit", _) => Some(object.to_owned()),
                ("tag", "commit") => Some(peeled.to_owned()),
                // A tag of a tag: rev-list peels it.
                ("tag", "tag") => Some(object.to_owned()),
                _ => None,
            };
            Some(Tip {
                reference: reference.to_owned(),
                object: object.to_owned(),
                commit,
            })
        })
        .collect())
}

/// The commits reachable from `roots` and from no remote-tracking branch, in one `rev-list`.
fn beyond_remotes(gitdir: &Path, roots: &[&str], remotes: &[String]) -> Result<Vec<String>> {
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = String::new();
    for root in roots {
        input.push_str(root);
        input.push('\n');
    }
    for remote in remotes {
        input.push('^');
        input.push_str(remote);
        input.push('\n');
    }
    let dir = gitdir.to_string_lossy();
    let output = Repo::at(gitdir).checked(
        &[
            "--git-dir",
            &dir,
            "--work-tree",
            &dir,
            "rev-list",
            "--stdin",
        ],
        Some(input.into_bytes()),
        &[],
    )?;
    Ok(String::from_utf8_lossy(&output)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// The commits of `commits` that none of the clones in `elsewhere` (git directories or work
/// trees) has, each looked up with one `cat-file --batch-check`.
fn missing_elsewhere(elsewhere: &[PathBuf], commits: Vec<String>) -> Vec<String> {
    let mut missing = commits;
    for clone in elsewhere.iter().filter(|clone| clone.is_dir()) {
        if missing.is_empty() {
            break;
        }
        let input = missing.join("\n") + "\n";
        let Ok(output) = Repo::at(clone).checked(
            &["cat-file", "--batch-check"],
            Some(input.into_bytes()),
            &[],
        ) else {
            continue;
        };
        let answers = String::from_utf8_lossy(&output).into_owned();
        let absent: HashSet<&str> = answers
            .lines()
            .filter(|line| line.ends_with(" missing"))
            .filter_map(|line| line.split(' ').next())
            .collect();
        missing.retain(|commit| absent.contains(commit.as_str()));
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
    let remotes = plumbing(gitdir, &["remote"])?;
    let remote = remotes
        .lines()
        .find(|name| *name == "origin")
        .or_else(|| remotes.lines().next())
        .ok_or_else(|| crate::core::error::Error("it has no remote".to_owned()))?;
    let dir = gitdir.to_string_lossy();
    let output = Repo::at(gitdir).git_within(
        &[
            "--git-dir",
            &dir,
            "--work-tree",
            &dir,
            "ls-remote",
            "--tags",
            remote,
        ],
        remote_timeout(),
    )?;
    Ok(String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(object, reference)| (reference.to_owned(), object.to_owned()))
        .collect())
}

/// Refuses when a ref of the repository at `gitdir`, or its HEAD, leads to a commit that no
/// remote-tracking branch, no clone in `elsewhere` and (for a tag) no remote has.
fn commits_held(name: &str, gitdir: &Path, elsewhere: &[PathBuf], online: bool) -> Result<()> {
    let all = tips(gitdir)?;
    if all.iter().any(|tip| tip.reference == "refs/stash") {
        return fail(format!("{name} has a stash; apply or drop it first"));
    }
    let remotes: Vec<String> = all
        .iter()
        .filter(|tip| tip.reference.starts_with("refs/remotes/"))
        .filter_map(|tip| tip.commit.clone())
        .collect();
    let head = plumbing(
        gitdir,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
    )
    .ok()
    .filter(|head| !head.is_empty());
    let mut roots: Vec<(String, String)> = all
        .iter()
        .filter(|tip| !tip.reference.starts_with("refs/remotes/"))
        .filter_map(|tip| Some((tip.reference.clone(), tip.commit.clone()?)))
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
    let tags: Vec<&Tip> = all
        .iter()
        .filter(|tip| tip.reference.starts_with("refs/tags/") && tip.commit.is_some())
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
                            *reference == tip.reference && *object == tip.object
                        })
                    })
                    .map(|tip| tip.reference.as_str())
                    .collect();
                kept.retain(|(reference, _)| !held.contains(reference.as_str()));
                // Whatever a held tag reaches is on the remote, HEAD at that commit included.
                not.extend(
                    tags.iter()
                        .filter(|tip| held.contains(tip.reference.as_str()))
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
    for marker in UNFINISHED {
        if gitdir.join(marker).exists() {
            return fail(format!(
                "{name} has a {} in progress; finish or abort it first",
                marker.trim_end_matches("_HEAD").to_lowercase()
            ));
        }
    }
    commits_held(name, gitdir, elsewhere, online)?;
    let worktrees = plumbing_bytes(gitdir, &["worktree", "list", "--porcelain", "-z"])?;
    if crate::git::parse_worktrees(&worktrees).len() > 1 {
        return fail(format!(
            "{name} has linked worktrees of its own; remove them first"
        ));
    }
    let Some(workdir) = workdir.filter(|dir| dir.is_dir()) else {
        return Ok(());
    };
    let work = Repo::at(workdir);
    let dirty = work.checked(
        &[
            "-c",
            "status.showUntrackedFiles=all",
            "status",
            "--porcelain",
            "-z",
            "--no-renames",
            "--untracked-files=all",
            // A submodule of this one gets its own pass, which filters its junk; here only
            // its commit counts.
            "--ignore-submodules=dirty",
        ],
        None,
        &[],
    )?;
    // An untracked file the operating system recreates (a Finder `.DS_Store`) is no work, as
    // at the top level; every other entry, tracked or not, counts.
    let real = dirty
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            let path = String::from_utf8_lossy(entry.get(3..).unwrap_or_default());
            !(entry.starts_with(b"?? ") && is_os_junk(path.rsplit('/').next().unwrap_or("")))
        });
    if real {
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
