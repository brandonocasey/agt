//! Moves a local branch forward to a descendant commit. The work tree that has the branch runs
//! `git merge --ff-only`. With none, a ref update moves it. Local changes and ignored files stay
//! untouched. Nothing is stashed, no work tree is added and no branch is switched. Git refuses a
//! branch being rebased or bisected.

use std::path::PathBuf;

use super::{GITLINK, Repo};
use crate::core::error::Result;
use crate::core::shell::quote;

/// What `fast_forward` did.
pub enum Update {
    /// The branch is at the new commit. `path` is the work tree that merged it, none when git moved
    /// the ref alone.
    Moved {
        path: Option<PathBuf>,
    },
    Refused(Refused),
}

/// Why the branch stayed put.
pub struct Refused {
    pub reason: String,
    /// The work tree that has the branch checked out, when there is one.
    pub path: Option<PathBuf>,
    /// The command to run once what git refused is resolved.
    pub retry: Option<String>,
}

impl Refused {
    fn because(reason: impl Into<String>, path: Option<PathBuf>) -> Self {
        Self {
            reason: reason.into(),
            path,
            retry: None,
        }
    }
}

/// Where a fast-forward would happen.
enum Place {
    /// No work tree has the branch checked out: git moves the ref alone.
    Ref,
    Worktree(PathBuf),
}

/// What stops moving local branch `name` to `to`, which MUST descend from `from`.
/// `None` when nothing stops it that can be seen beforehand.
/// Git may still refuse for a branch being rebased or bisected.
pub fn check(repo: &Repo, name: &str, from: &str, to: &str) -> Result<Option<Refused>> {
    Ok(place(repo, name, from, to)?.err())
}

/// Moves local branch `name` from `from` to `to`, which MUST descend from `from`. A refusal is
/// not an error: the branch is as it was.
pub fn fast_forward(repo: &Repo, name: &str, from: &str, to: &str) -> Result<Update> {
    Ok(match place(repo, name, from, to)? {
        Err(refused) => Update::Refused(refused),
        Ok(Place::Ref) => {
            let spec = format!("{to}:refs/heads/{name}");
            let fetch = [
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-write-fetch-head",
                "--no-recurse-submodules",
                ".",
                &spec,
            ];
            run(repo, &fetch, None)?
        }
        Ok(Place::Worktree(path)) => {
            let merge = [
                "merge",
                "--ff-only",
                "--no-overwrite-ignore",
                "--no-autostash",
                "--quiet",
                to,
            ];
            run(&Repo::at(&path), &merge, Some(path.clone()))?
        }
    })
}

fn place(
    repo: &Repo,
    name: &str,
    from: &str,
    to: &str,
) -> Result<std::result::Result<Place, Refused>> {
    let worktrees = repo.worktrees()?;
    if worktrees.first().is_some_and(|main| main.bare) {
        return Ok(Err(Refused::because(
            "git reports the main repository as bare; automatic default-branch updates are disabled because an existing checkout cannot be ruled out; check core.bare if this repository has a working checkout",
            None,
        )));
    }
    let holders: Vec<_> = worktrees
        .iter()
        .filter(|entry| entry.branch.as_deref() == Some(name))
        .collect();
    let holder = match holders.as_slice() {
        [] => return Ok(Ok(Place::Ref)),
        [holder] => holder,
        _ => {
            let paths: Vec<String> = holders
                .iter()
                .map(|holder| holder.path.display().to_string())
                .collect();
            return Ok(Err(Refused::because(
                format!(
                    "it is checked out in {} work trees ({}), and a merge in one would leave the others behind",
                    paths.len(),
                    paths.join(", ")
                ),
                None,
            )));
        }
    };
    let path = Some(holder.path.clone());
    if !holder.path.exists() {
        return Ok(Err(Refused::because(
            format!(
                "it is checked out in {}, which no longer exists; `git worktree prune` drops that entry",
                holder.path.display()
            ),
            path,
        )));
    }
    if moves_submodule(repo, from, to)? {
        return Ok(Err(Refused::because(
            format!(
                "a submodule moves, and a merge in {} would leave it at its old commit",
                holder.path.display()
            ),
            path,
        )));
    }
    Ok(Ok(Place::Worktree(holder.path.clone())))
}

/// Whether a submodule pointer differs between `from` and `to`.
/// A merge leaves a moved submodule at its old commit, which dirties the checkout.
pub fn moves_submodule(repo: &Repo, from: &str, to: &str) -> Result<bool> {
    let raw = repo.git(&["diff", "--raw", "--ignore-submodules=none", from, to])?;
    Ok(raw.lines().any(|line| {
        line.split_whitespace()
            .take(2)
            .any(|mode| mode.trim_start_matches(':') == GITLINK)
    }))
}

/// Runs the update. Git may refuse over local changes a merge would overwrite.
/// The reason is git's message, and `retry` the command to run once that is resolved.
fn run(repo: &Repo, command: &[&str], path: Option<PathBuf>) -> Result<Update> {
    let (code, error) = repo.probe(command)?;
    if code == Some(0) {
        return Ok(Update::Moved { path });
    }
    let words: Vec<String> = command.iter().map(|word| quote(word)).collect();
    let retry = format!(
        "git -C {} {}",
        quote(&repo.root().display().to_string()),
        words.join(" ")
    );
    let message: Vec<&str> = error
        .lines()
        .map(|line| {
            let line = line.trim();
            ["error: ", "fatal: "]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
                .unwrap_or(line)
        })
        .filter(|line| !line.is_empty())
        .collect();
    // `--no-overwrite-ignore` makes git count an ignored file as untracked. The message is reworded
    // to name both.
    let message = message.join(" ").replace(
        "untracked working tree files",
        "untracked or ignored working tree files",
    );
    Ok(Update::Refused(Refused {
        reason: format!("git {} refused: {message}", command[0]),
        path,
        retry: Some(retry),
    }))
}
