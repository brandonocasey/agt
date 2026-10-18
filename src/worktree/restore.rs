use std::fs;
use std::path::PathBuf;

use clap::Args;
use serde_json::json;

use super::{backup, info, snapshot};
use crate::core::error::{Result, ctx, fail};
use crate::core::output::Output;
use crate::git::Repo;

#[derive(Args)]
pub struct Cli {
    /// The backup id `agt worktree remove` printed.
    id: String,
    /// Recreate the worktree here instead of at its original path.
    #[arg(long)]
    path: Option<PathBuf>,
}

pub fn run(cli: Cli) -> Result<Output> {
    let (record, dir) = backup::find(&cli.id)?;
    let target = cli.path.unwrap_or_else(|| record.worktree.clone());
    if target.exists() {
        return fail(format!("{} already exists", target.display()));
    }
    let repo = Repo::at(&record.repository);
    if !repo.object_exists(&format!("{}^{{commit}}", record.snapshot))? {
        return fail(format!(
            "snapshot {} is gone from {}",
            record.snapshot,
            record.repository.display()
        ));
    }
    let mut warnings = Vec::new();
    repo.git(&["worktree", "prune"])?;
    let target_text = target.to_string_lossy().into_owned();
    match &record.branch {
        None => {
            repo.git(&["worktree", "add", "--detach", &target_text, &record.head])?;
        }
        Some(branch) => {
            let reference = format!("refs/heads/{branch}");
            let tip = repo.git_opt(&["rev-parse", "--verify", "--quiet", &reference])?;
            let elsewhere = info::entries(&repo)?
                .iter()
                .any(|entry| entry.branch.as_deref() == Some(branch));
            match tip {
                None => {
                    repo.git(&["worktree", "add", "-b", branch, &target_text, &record.head])?;
                }
                Some(_) if elsewhere => {
                    warnings.push(format!(
                        "branch {branch} is checked out in another worktree; restored detached at {}",
                        record.head
                    ));
                    repo.git(&["worktree", "add", "--detach", &target_text, &record.head])?;
                }
                Some(tip) => {
                    if tip != record.head {
                        warnings.push(format!(
                            "branch {branch} moved from {} to {tip} since the backup",
                            record.head
                        ));
                    }
                    repo.git(&["worktree", "add", &target_text, branch])?;
                }
            }
        }
    }

    let target = info::real(&target)?;
    apply(&target, &record.snapshot)?;
    snapshot::copy_files(&dir.join("files"), &target, &record.copied, 0o755)?;
    Ok(Output::Json(json!({
        "restored": target,
        "id": record.id,
        "branch": record.branch,
        "head": record.head,
        "copied_ignored": record.copied.len(),
        "warnings": warnings,
    })))
}

/// Makes the working tree match `snapshot` and leaves every difference from HEAD unstaged.
fn apply(worktree: &std::path::Path, snapshot: &str) -> Result<()> {
    let repo = Repo::at(worktree);
    let raw = repo.checked(
        &["diff-tree", "-r", "--no-renames", "-z", "HEAD", snapshot],
        None,
        &[],
    )?;
    let mut fields = raw.split(|byte| *byte == 0);
    let mut changed: Vec<u8> = Vec::new();
    while let Some(header) = fields.next().filter(|header| !header.is_empty()) {
        let Some(path) = fields.next() else { break };
        let header = String::from_utf8_lossy(header);
        let parts: Vec<&str> = header.trim_start_matches(':').split(' ').collect();
        // Submodule commits cannot be checked out as files; the worktree records HEAD's.
        if parts.iter().take(2).any(|mode| *mode == "160000") {
            continue;
        }
        if parts.get(4).is_some_and(|status| *status == "D") {
            let file = worktree.join(String::from_utf8_lossy(path).as_ref());
            fs::remove_file(&file).map_err(ctx(format!("remove {}", file.display())))?;
            continue;
        }
        changed.extend_from_slice(path);
        changed.push(0);
    }
    if !changed.is_empty() {
        repo.checked(
            &[
                "checkout",
                snapshot,
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
            Some(changed),
            &[],
        )?;
    }
    repo.git(&["reset", "-q"])?;
    Ok(())
}
