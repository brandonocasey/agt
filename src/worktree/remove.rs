use std::path::Path;

use clap::Args;
use serde_json::json;

use super::info::{self, Entry};
use super::{snapshot, submodules};
use crate::core::agents;
use crate::core::error::{Result, fail};
use crate::core::output::Output;
use crate::git::Repo;

#[derive(Args)]
pub struct Cli {
    /// The linked worktree to remove.
    path: std::path::PathBuf,
    /// Back the worktree up and remove it. Without it nothing changes and the plan is printed.
    #[arg(long)]
    yes: bool,
}

pub fn run(cli: Cli) -> Result<Output> {
    let path = match info::real(&cli.path) {
        Ok(path) => path,
        Err(_) => return fail(format!("{} is not a linked worktree", cli.path.display())),
    };
    let listing = info::entries(&Repo::at(&path))
        .or_else(|_| fail(format!("{} is not a linked worktree", path.display())))?;
    let main = info::real(&listing[0].path)?;
    let entry = refuse(&listing, &path, &main)?;
    submodules::check(&path, &main)?;

    let (modified, untracked) = info::dirty(&path)?;
    if !cli.yes {
        let ignored = info::ignored(&path)?;
        return Ok(Output::Json(json!({
            "plan": true,
            "path": path,
            "branch": entry.branch,
            "head": entry.head,
            "snapshot_files": modified + untracked,
            "copied_ignored": ignored.copy,
            "skipped_disposable": ignored.skipped,
            "next": "pass --yes to back up and remove",
        })));
    }

    let taken = snapshot::take(&main, entry)?;
    let shared = Repo::at(&main);
    shared.git(&["worktree", "remove", "--force", &path.to_string_lossy()])?;
    shared.git(&["worktree", "prune"])?;
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
    Ok(Output::Json(json!({
        "removed": path,
        "id": taken.record.id,
        "ref": taken.record.reference,
        "backup_dir": taken.dir,
        "snapshot_files": taken.changed,
        "copied_ignored": taken.record.copied.len(),
        "head_on_local_branch": !local.is_empty(),
        "head_on_remote_branch": !remote.is_empty(),
        "local_branches": local,
        "remote_branches": remote,
    })))
}

/// The entry for `path`, or the reason it must not be removed.
fn refuse<'a>(listing: &'a [Entry], path: &Path, main: &Path) -> Result<&'a Entry> {
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
    if let Some(live) = agents::live_in(path) {
        return fail(format!(
            "a Claude Code session (pid {}) is running in {}",
            live.pid,
            live.cwd.display()
        ));
    }
    Ok(entry)
}
