use std::path::Path;

use clap::Args;
use serde_json::{Value, json};

use super::idle::Cwds;
use super::info;
use super::{gate, hooks, snapshot};
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

    let (modified, untracked) = info::dirty(&path)?;
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
            "snapshot_files": modified + untracked,
            "ignored_lost": info::lost(&path)?,
            "hooks": {"remove": planned},
            "warnings": warnings,
            "next": "pass --yes to back up and remove",
        });
        if let Some(untrusted) = untrusted {
            plan["hooks"]["untrusted"] = untrusted;
        }
        return Ok(Output::Json(plan));
    }

    let mut done = removal(&main, &path, entry, !cli.no_hooks, true)?;
    done["warnings"] = json!(warnings);
    Ok(Output::Json(done))
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
) -> Result<Value> {
    gate::check(path, main)?;
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
    let taken = snapshot::take(main, entry)?;
    let shared = Repo::at(main);
    shared.git(&["worktree", "remove", "--force", &path.to_string_lossy()])?;
    let branches = |pattern: &str| -> Result<Vec<String>> {
        let found = shared.git(&[
            "for-each-ref",
            "--contains",
            &entry.head,
            "--format=%(refname:short)",
            pattern,
        ])?;
        Ok(found.lines().map(str::to_owned).collect())
    };
    let local = branches("refs/heads")?;
    let remote = branches("refs/remotes")?;
    let mut done = json!({
        "removed": path,
        "id": taken.record.id,
        "ref": taken.record.reference,
        "backup_dir": taken.dir,
        "snapshot_files": taken.changed,
        "ignored_lost": lost,
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
    if let Ok(cwd) = std::env::current_dir().and_then(|cwd| cwd.canonicalize())
        && cwd.starts_with(path)
    {
        return fail(format!(
            "the current directory is inside {}; run from elsewhere",
            path.display()
        ));
    }
    Ok(entry)
}
