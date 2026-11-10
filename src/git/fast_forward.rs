//! Moves a local branch forward to a commit that descends from it, without touching anything
//! else: `git merge --ff-only` in the work tree that has the branch checked out, which keeps
//! local changes and refuses to overwrite them, ignored files included, else a ref update that git
//! refuses for a branch being rebased or bisected. Never stashes, adds a work tree or switches a
//! checkout's branch.

use std::path::PathBuf;

use super::{GITLINK, Repo};
use crate::core::error::Result;
use crate::core::shell::quote;

/// What `fast_forward` did.
pub enum Update {
    /// The branch is at the new commit; `path` is the work tree that merged it, none when git
    /// moved the ref alone.
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
    fn because(reason: impl Into<String>, path: Option<PathBuf>) -> Update {
        Update::Refused(Self {
            reason: reason.into(),
            path,
            retry: None,
        })
    }
}

/// Moves local branch `name` from `from` to `to`, which MUST descend from `from`. A refusal is
/// not an error: the branch is as it was.
pub fn fast_forward(repo: &Repo, name: &str, from: &str, to: &str) -> Result<Update> {
    let worktrees = repo.worktrees()?;
    if worktrees.first().is_some_and(|main| main.bare) {
        return Ok(Refused::because(
            "git reports the main repository as bare; automatic default-branch updates are disabled because an existing checkout cannot be ruled out; check core.bare if this repository has a working checkout",
            None,
        ));
    }
    let holders: Vec<_> = worktrees
        .iter()
        .filter(|entry| entry.branch.as_deref() == Some(name))
        .collect();
    let [holder] = holders.as_slice() else {
        if holders.is_empty() {
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
            return run(repo, &fetch, None);
        }
        let paths: Vec<String> = holders
            .iter()
            .map(|holder| holder.path.display().to_string())
            .collect();
        return Ok(Refused::because(
            format!(
                "it is checked out in {} work trees ({}), and a merge in one would leave the others behind",
                paths.len(),
                paths.join(", ")
            ),
            None,
        ));
    };
    let path = Some(holder.path.clone());
    if !holder.path.exists() {
        return Ok(Refused::because(
            format!(
                "it is checked out in {}, which no longer exists; `git worktree prune` drops that entry",
                holder.path.display()
            ),
            path,
        ));
    }
    // A merge leaves a moved submodule checked out at its old commit, so the checkout turns dirty.
    let raw = repo.git(&["diff", "--raw", "--ignore-submodules=none", from, to])?;
    let gitlink = raw.lines().any(|line| {
        line.split_whitespace()
            .take(2)
            .any(|mode| mode.trim_start_matches(':') == GITLINK)
    });
    if gitlink {
        return Ok(Refused::because(
            format!(
                "a submodule moves, and a merge in {} would leave it at its old commit",
                holder.path.display()
            ),
            path,
        ));
    }
    run(
        &Repo::at(&holder.path),
        &[
            "merge",
            "--ff-only",
            "--no-overwrite-ignore",
            "--no-autostash",
            "--quiet",
            to,
        ],
        path,
    )
}

/// Runs the update; when git refuses, as it does over local changes a merge would overwrite, the
/// reason is git's message and `retry` the command to run once that is resolved.
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
    Ok(Update::Refused(Refused {
        reason: format!("git {} refused: {}", command[0], message.join(" ")),
        path,
        retry: Some(retry),
    }))
}
