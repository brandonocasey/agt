use std::path::Path;

use clap::Args;
use serde_json::{Value, json};

use super::idle::Cwds;
use super::info;
use super::{gate, hooks, nested, snapshot};
use crate::core::cwd;
use crate::core::error::{Result, fail};
use crate::core::output::Output;
use crate::git::{Repo, Worktree};

#[derive(Args)]
pub struct Cli {
    /// The linked worktree to remove.
    path: std::path::PathBuf,
    /// Back the worktree up and remove it. Without it nothing changes and the plan is printed.
    #[arg(long)]
    yes: bool,
    /// Skip the `[worktree.hooks] remove` commands.
    #[arg(long)]
    no_hooks: bool,
    /// Remove despite untracked files, or a HEAD on no branch outside an ignored path. The backup
    /// keeps neither.
    #[arg(long)]
    force: bool,
}

pub fn run(cli: Cli) -> Result<Output> {
    let path = match info::real(&cli.path) {
        Ok(path) => path,
        Err(_) => return fail(format!("{} is not a linked worktree", cli.path.display())),
    };
    let listing = Repo::at(&path)
        .worktrees()
        .or_else(|_| fail(format!("{} is not a linked worktree", path.display())))?;
    let main = info::real(&listing[0].path)?;
    let entry = refuse(&listing, &path, &main)?;
    snapshot::require_commit(entry)?;
    let mut cwds = Cwds::default();
    if let Some(why) = cwds.busy(&path) {
        return fail(format!("{} is in use: {why}", path.display()));
    }
    let warnings = cwds.own_in(&path);
    gate::check(&path, &main)?;

    let (modified, _) = info::dirty(&path)?;
    let unbacked = info::untracked(&path)?;
    let mode = if cli.force {
        Untracked::Drop
    } else {
        Untracked::Refuse
    };
    let unpinned = unpinned_head(&main, &path, entry, cli.force)?;
    if mode == Untracked::Refuse {
        refuse_untracked(&path, &unbacked)?;
    }
    let mut warnings = warnings;
    warnings.extend(unpinned);
    if !cli.yes {
        let (planned, untrusted) = if cli.no_hooks {
            (Vec::new(), None)
        } else {
            let resolved = hooks::resolve(&main)?;
            (
                resolved.commands(hooks::Kind::Remove).to_vec(),
                resolved.untrusted,
            )
        };
        let mut plan = json!({
            "plan": true,
            "path": path,
            "branch": entry.branch,
            "head": entry.head,
            "snapshot_files": modified,
            "untracked_lost": info::capped(unbacked),
            "ignored_lost": info::lost(&path)?,
            "hooks": {"remove": planned},
            "warnings": warnings,
            "next": if cli.force {
                "pass --yes --force to back up and remove; untracked files are not backed up"
            } else {
                "pass --yes to back up and remove"
            },
        });
        if let Some(untrusted) = untrusted {
            plan["hooks"]["untrusted"] = untrusted;
        }
        return Ok(Output::Json(plan));
    }

    let mut done = removal(&main, &path, entry, !cli.no_hooks, true, mode)?;
    done["warnings"] = json!(warnings);
    Ok(Output::Json(done))
}

/// What a removal does with untracked, non-ignored files.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Untracked {
    /// Fail while any exist: no backup keeps them.
    Refuse,
    /// Delete them with the worktree; the snapshot leaves them out.
    Drop,
}

/// Fails, naming the first few of `paths`, when there are untracked files no backup would keep.
fn refuse_untracked(path: &Path, paths: &[String]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let shown: Vec<&str> = paths.iter().take(5).map(String::as_str).collect();
    let more = paths.len() - shown.len();
    fail(format!(
        "{} has {} untracked files no backup keeps ({}{}); commit or delete them, or pass --force (cleanup: --dirty) to remove without them; nothing was backed up or removed",
        path.display(),
        paths.len(),
        shown.join(", "),
        if more > 0 {
            format!(", and {more} more")
        } else {
            String::new()
        }
    ))
}

/// A backup keeps files, not commits. A HEAD on no branch needs `--force` unless its path is
/// ignored; both warn.
fn unpinned_head(
    main: &Path,
    path: &Path,
    entry: &Worktree,
    force: bool,
) -> Result<Option<String>> {
    if gate::held_by_a_branch(&Repo::at(main), &entry.head)? {
        return Ok(None);
    }
    let ignored = match path.strip_prefix(main) {
        Ok(inside) => Repo::at(main).is_ignored(&inside.to_string_lossy())?,
        Err(_) => false,
    };
    if !force && !ignored {
        return fail(format!(
            "{} has HEAD {} on no branch, and a backup keeps files, not commits; create a branch there, or pass --force to remove without them; nothing was backed up or removed",
            path.display(),
            entry.head
        ));
    }
    Ok(Some(format!(
        "HEAD {} is on no branch; its commits are not backed up",
        entry.head
    )))
}

/// Runs the remove hooks when `run_hooks`, backs `entry` up, then removes its worktree; the
/// caller has checked that it may go. A failing hook is an error before anything is touched,
/// and so, unless `allow_lost`, is an ignored file no backup keeps.
pub(super) fn removal(
    main: &Path,
    path: &Path,
    entry: &Worktree,
    run_hooks: bool,
    allow_lost: bool,
    untracked: Untracked,
) -> Result<Value> {
    gate::check(path, main)?;
    let inner = nested::linked_inside(main, path)?;
    let mut hooks_report = None;
    if run_hooks {
        let (report, ok) = hooks::run(path, hooks::Kind::Remove)?;
        if !ok {
            let failed = report["remove"]
                .as_array()
                .and_then(|steps| steps.last())
                .cloned()
                .unwrap_or_default();
            return fail(format!(
                "remove hook `{}` failed; nothing was backed up or removed; see {}",
                failed["command"].as_str().unwrap_or_default(),
                failed["log"].as_str().unwrap_or_default()
            ));
        }
        hooks_report = Some(report).filter(|report| report.get("untrusted").is_some());
        // The hooks ran arbitrary commands; check again for work they may have left.
        gate::check(path, main)?;
    }
    let lost = info::lost(path)?;
    // A hook may have written one since the caller checked.
    if !allow_lost && !lost.is_empty() {
        return fail(format!(
            "dirty: {} ignored paths no backup keeps ({}); nothing was backed up or removed",
            lost.len(),
            lost.join(", ")
        ));
    }
    // A hook may have written an untracked file since the caller checked.
    let dropped = match untracked {
        Untracked::Refuse => {
            refuse_untracked(path, &info::untracked(path)?)?;
            Vec::new()
        }
        Untracked::Drop => info::untracked(path)?,
    };
    let taken = snapshot::take(main, entry)?;
    let shared = Repo::at(main);
    for other in &inner {
        shared.remove_worktree(&other.path)?;
    }
    shared.remove_worktree(path)?;
    let local = shared.refs_containing(&entry.head, "refs/heads/")?;
    let remote = shared.refs_containing(&entry.head, "refs/remotes/")?;
    let mut done = json!({
        "removed": path,
        "id": taken.record.id,
        "backup": taken.path,
        "snapshot_files": taken.changed,
        "ignored_lost": lost,
        "untracked_lost": info::capped(dropped),
        "head_on_local_branch": !local.is_empty(),
        "head_on_remote_branch": !remote.is_empty(),
        "local_branches": local,
        "remote_branches": remote,
    });
    if let Some(report) = hooks_report {
        done["hooks"] = report;
    }
    Ok(done)
}

/// The entry for `path`, or the reason it must not be removed.
pub(super) fn refuse<'a>(
    listing: &'a [Worktree],
    path: &Path,
    main: &Path,
) -> Result<&'a Worktree> {
    if path == main {
        return fail(format!("{} is the main checkout", path.display()));
    }
    let Some(entry) = listing
        .iter()
        .find(|entry| info::real(&entry.path).is_ok_and(|real| real == path))
    else {
        return fail(format!("{} is not a linked worktree", path.display()));
    };
    if entry.locked {
        return fail(format!("{} is locked; unlock it first", path.display()));
    }
    if cwd::held().iter().any(|held| held.starts_with(path)) {
        return fail(format!(
            "the current directory is inside {}; run from elsewhere",
            path.display()
        ));
    }
    Ok(entry)
}
