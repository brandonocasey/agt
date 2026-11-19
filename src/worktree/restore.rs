use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use clap::Args;
use serde_json::json;

use super::{backup, info, snapshot};

use crate::core::error::{Result, ctx, fail};
use crate::core::output::Output;
use crate::git::{Checkout, GITLINK_MODE, Repo};

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
    // Git runs in the repository, so a relative `--path` must mean the caller's directory.
    let target = match cli.path {
        Some(path) => {
            std::path::absolute(&path).map_err(ctx(format!("resolve {}", path.display())))?
        }
        None => record.worktree.clone(),
    };
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
    // Only the registration of this very path is cleared; other missing worktrees keep theirs.
    let wanted = lexical_real(&target);
    if repo
        .worktrees()?
        .iter()
        .any(|entry| !entry.path.exists() && lexical_real(&entry.path) == wanted)
    {
        repo.remove_worktree(&target)?;
    }
    match &record.branch {
        None => {
            repo.add_worktree(&target, &Checkout::Detached(&record.head))?;
        }
        Some(branch) => {
            let tip = repo.branch_tip(branch)?;
            let elsewhere = repo.checkout_blocker(branch)?.is_some();
            match tip {
                None => {
                    let new = Checkout::NewBranch {
                        name: branch,
                        at: &record.head,
                    };
                    repo.add_worktree(&target, &new)?;
                }
                Some(_) if elsewhere => {
                    warnings.push(format!(
                        "branch {branch} is checked out in another worktree; restored detached at {}",
                        record.head
                    ));
                    repo.add_worktree(&target, &Checkout::Detached(&record.head))?;
                }
                Some(tip) if tip != record.head => {
                    warnings.push(format!(
                        "branch {branch} moved from {} to {tip} since the backup; restored detached at {}",
                        record.head, record.head
                    ));
                    repo.add_worktree(&target, &Checkout::Detached(&record.head))?;
                }
                Some(_) => {
                    repo.add_worktree(&target, &Checkout::Branch(branch))?;
                }
            }
        }
    }

    let target = info::real(&target)?;
    // `worktree add` copies a sparse main checkout's patterns. The snapshot goes onto a full
    // checkout, where every file it changes or deletes is on disk; the backup's own sparse
    // checkout comes back after it.
    let repo = Repo::at(&target);
    if sparse_enabled(&repo) {
        disable_inherited(&repo, &mut warnings);
    }
    let deleted = apply(&target, &record.snapshot)?;
    // Before the ignored files come back: a cone reapply deletes an out-of-cone directory
    // that holds only ignored files.
    let sparse = reapply_sparse(&dir.join(snapshot::SAVED_GIT_DIR), &target, &mut warnings);
    if sparse {
        restage_deletions(&repo, &deleted, &mut warnings);
    }
    restore_modes(&target, &record.file_modes)?;
    snapshot::copy_files(&dir.join("files"), &target, &record.copied, 0o755)?;
    Ok(Output::Json(json!({
        "sparse_checkout": sparse,
        "restored": target,
        "id": record.id,
        "branch": record.branch,
        "head": record.head,
        "copied_ignored": record.copied.len(),
        "warnings": warnings,
    })))
}

/// Puts the saved sparse-checkout patterns and per-worktree config back into the restored
/// worktree and reapplies them; true when it did. A failure is a warning naming the saved
/// files, not an error: the worktree is restored, as a full checkout.
fn reapply_sparse(
    saved: &std::path::Path,
    target: &std::path::Path,
    warnings: &mut Vec<String>,
) -> bool {
    if !snapshot::SAVED_GIT_FILES
        .iter()
        .any(|name| saved.join(name).is_file())
    {
        return false;
    }
    let repo = Repo::at(target);
    let Ok(git_dir) = repo.git_dir() else {
        return false;
    };
    let attempt = || -> Result<bool> {
        // Per-worktree config only counts when the repository reads it.
        repo.set_config_bool("extensions.worktreeConfig", true)?;
        for name in snapshot::SAVED_GIT_FILES {
            let from = saved.join(name);
            let to = git_dir.join(name);
            if !from.is_file() {
                // The backup is the record of these files. One it lacks, which `worktree add`
                // copied or `sparse-checkout disable` wrote, goes: the worktree then reads the
                // shared config again, as it did (a sparse checkout set there, the old way).
                if let Err(error) = fs::remove_file(&to)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(ctx(format!("remove {}", to.display()))(error));
                }
                continue;
            }
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent).map_err(ctx(format!("create {}", parent.display())))?;
            }
            fs::copy(&from, &to).map_err(ctx(format!("restore {}", to.display())))?;
        }
        // A config git cannot read is an error here, not an unset setting.
        repo.check_config()?;
        // `sparse-checkout disable` leaves the patterns file and turns the setting off.
        let sparse = repo.sparse_checkout_enabled()?;
        if sparse {
            repo.sparse_reapply()?;
        }
        Ok(sparse)
    };
    match attempt() {
        Ok(sparse) => sparse,
        Err(error) => {
            // A half-copied config would break every git command in the worktree.
            for name in snapshot::SAVED_GIT_FILES {
                let _ = fs::remove_file(git_dir.join(name));
            }
            warnings.push(format!(
                "could not reapply the sparse checkout ({}); the worktree is a full checkout, the saved patterns are in {}",
                error.0,
                saved.display()
            ));
            false
        }
    }
}

fn sparse_enabled(repo: &Repo) -> bool {
    repo.sparse_checkout_enabled().unwrap_or(false)
}

/// Turns off a sparse checkout that `worktree add` inherited from the main checkout.
fn disable_inherited(repo: &Repo, warnings: &mut Vec<String>) {
    if let Err(error) = repo.sparse_disable() {
        warnings.push(format!(
            "could not turn off the sparse checkout inherited from the main checkout ({})",
            error.0
        ));
    }
}

/// `path` with its parent directory resolved, so a path that no longer exists still compares
/// equal to the way git recorded it.
fn lexical_real(path: &std::path::Path) -> PathBuf {
    match (
        path.parent().and_then(|dir| fs::canonicalize(dir).ok()),
        path.file_name(),
    ) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => path.to_owned(),
    }
}

/// Stages again the snapshot's deletions that the sparse reapply marked skip-worktree. A
/// deletion outside the cone cannot stay unstaged in a sparse checkout: the reapply treats the
/// missing file as up to date, and the next commit would keep it.
fn restage_deletions(repo: &Repo, deleted: &[Vec<u8>], warnings: &mut Vec<String>) {
    let attempt = || -> Result<()> {
        // A deletion that is not marked skip-worktree is already unstaged.
        repo.unstage_skipped(deleted)?;
        Ok(())
    };
    if let Err(error) = attempt() {
        warnings.push(format!(
            "could not stage again the deletions outside the sparse checkout ({})",
            error.0
        ));
    }
}

/// Makes the working tree match `snapshot` and leaves every difference from HEAD unstaged.
/// Returns the paths it deleted.
fn apply(worktree: &std::path::Path, snapshot: &str) -> Result<Vec<Vec<u8>>> {
    let repo = Repo::at(worktree);
    let mut changed: Vec<Vec<u8>> = Vec::new();
    let mut deleted: Vec<Vec<u8>> = Vec::new();
    for change in repo.tree_changes("HEAD", snapshot)? {
        // Submodule commits cannot be checked out as files; the worktree records HEAD's.
        if change.old_mode == GITLINK_MODE || change.new_mode == GITLINK_MODE {
            continue;
        }
        if change.status == 'D' {
            let file = worktree.join(std::ffi::OsStr::from_bytes(&change.path));
            fs::remove_file(&file).map_err(ctx(format!("remove {}", file.display())))?;
            deleted.push(change.path);
            continue;
        }
        changed.push(change.path);
    }
    if !changed.is_empty() {
        repo.checkout_paths(snapshot, &changed)?;
    }
    repo.reset_index_to_head()?;
    Ok(deleted)
}

fn restore_modes(target: &std::path::Path, modes: &[backup::FileMode]) -> Result<()> {
    for saved in modes {
        let relative = std::path::Path::new(std::ffi::OsStr::from_bytes(&saved.path));
        if relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return fail("backup file mode has an invalid relative path");
        }
        let mut path = target.to_owned();
        let mut components = relative.components().peekable();
        while let Some(part) = components.next() {
            path.push(part);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(ctx(format!("stat {}", path.display()))(error)),
            };
            if components.peek().is_some() {
                if !metadata.is_dir() {
                    return fail(format!("cannot restore mode through {}", path.display()));
                }
            } else if metadata.is_file() {
                fs::set_permissions(&path, fs::Permissions::from_mode(saved.mode & 0o7777))
                    .map_err(ctx(format!("restore mode of {}", path.display())))?;
            }
        }
    }
    Ok(())
}
