use std::collections::HashSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::json;

use super::{archive, backup, info, snapshot};
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
    let (record, source) = backup::find(&cli.id)?;
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
    if backup::is_archive(&source) {
        restore_archive(&record, &source, &target)
    } else {
        restore_directory(&record, &source, &target)
    }
}

/// Where a restore puts the files: `path`, as a worktree of the repository when `attached`, or a
/// plain directory.
struct Placed {
    path: PathBuf,
    attached: bool,
}

fn restore_archive(record: &backup::Record, archive: &Path, target: &Path) -> Result<Output> {
    let mut warnings = Vec::new();
    let repo = Repo::at(&record.repository);
    let attached =
        record.repository.is_dir() && repo.object_exists(&format!("{}^{{commit}}", record.head))?;
    let mut made_branch = None;
    if attached {
        made_branch = add_worktree(&repo, record, target, &mut warnings)?;
    } else {
        fs::create_dir_all(target).map_err(ctx(format!("create {}", target.display())))?;
        warnings.push(format!(
            "HEAD {} is gone from {}; restored the files into a plain directory, not a worktree",
            record.head,
            record.repository.display()
        ));
    }
    let placed = Placed {
        path: target.to_owned(),
        attached,
    };
    match unpack_backup(record, archive, &placed, &mut warnings) {
        Ok((target, sparse)) => Ok(report(record, &target, sparse, warnings)),
        Err(error) => {
            // A restore that stops half way removes the worktree or directory it made, so a retry can start clean.
            if attached {
                let _ = repo.git(&["worktree", "remove", "--force", &target.to_string_lossy()]);
                if let Some(branch) = made_branch {
                    let _ = repo.git(&["branch", "-D", &branch]);
                }
            } else {
                let _ = fs::remove_dir_all(target);
            }
            Err(error)
        }
    }
}

/// Puts the archive's files into the placed worktree; returns its real path and whether its
/// sparse checkout came back.
fn unpack_backup(
    record: &backup::Record,
    archive: &Path,
    placed: &Placed,
    warnings: &mut Vec<String>,
) -> Result<(PathBuf, bool)> {
    let target = info::real(&placed.path)?;
    let repo = Repo::at(&target);
    if placed.attached && sparse_enabled(&repo) {
        disable_inherited(&repo, warnings);
    }
    for path in &record.deleted {
        let file = archive::inside(&target, path)?;
        remove_with_empty_parents(&target, &file)?;
    }
    let git_files = tempfile::tempdir().map_err(ctx("create a scratch directory"))?;
    archive::unpack(archive, &target, git_files.path())?;
    let sparse = placed.attached && reapply_sparse(git_files.path(), archive, &target, warnings);
    if sparse {
        restage_deletions(&repo, &record.deleted, warnings);
    }
    Ok((target, sparse))
}

/// The restore result: where the worktree is and what came back.
fn report(record: &backup::Record, target: &Path, sparse: bool, warnings: Vec<String>) -> Output {
    Output::Json(json!({
        "sparse_checkout": sparse,
        "restored": target,
        "id": record.id,
        "branch": record.branch,
        "head": record.head,
        "copied_ignored": record.copied.len(),
        "warnings": warnings,
    }))
}

fn restore_directory(record: &backup::Record, dir: &Path, target: &Path) -> Result<Output> {
    let repo = Repo::at(&record.repository);
    let snapshot = record.snapshot.as_deref().unwrap_or_default();
    if !repo.object_exists(&format!("{snapshot}^{{commit}}"))? {
        return fail(format!(
            "snapshot {snapshot} is gone from {}",
            record.repository.display()
        ));
    }
    let mut warnings = Vec::new();
    add_worktree(&repo, record, target, &mut warnings)?;

    let target = info::real(target)?;
    // `worktree add` copies a sparse main checkout's patterns. The snapshot goes onto a full
    // checkout. Every file it changes or deletes is on disk. The backup's own sparse checkout
    // comes back after it.
    let repo = Repo::at(&target);
    if sparse_enabled(&repo) {
        disable_inherited(&repo, &mut warnings);
    }
    let deleted = apply(&target, snapshot)?;
    // Before the ignored files come back: a cone reapply deletes an out-of-cone directory
    // that holds only ignored files.
    let saved = dir.join(snapshot::SAVED_GIT_DIR);
    let sparse = reapply_sparse(&saved, &saved, &target, &mut warnings);
    if sparse {
        restage_deletions(&repo, &deleted, &mut warnings);
    }
    restore_modes(&target, &record.file_modes)?;
    snapshot::copy_files(&dir.join("files"), &target, &record.copied, 0o755)?;
    Ok(report(record, &target, sparse, warnings))
}

/// Adds the worktree at `target` on the backup's branch. It detaches at the backup's HEAD if the
/// branch moved or is checked out elsewhere. Returns the branch it had to create.
fn add_worktree(
    repo: &Repo,
    record: &backup::Record,
    target: &Path,
    warnings: &mut Vec<String>,
) -> Result<Option<String>> {
    let mut made_branch = None;
    let target_text = target.to_string_lossy().into_owned();
    // Only the registration of this very path is cleared; other missing worktrees keep theirs.
    let wanted = lexical_real(target);
    if repo
        .worktrees()?
        .iter()
        .any(|entry| !entry.path.exists() && lexical_real(&entry.path) == wanted)
    {
        repo.git(&["worktree", "remove", "--force", &target_text])?;
    }
    match &record.branch {
        None => {
            repo.git(&["worktree", "add", "--detach", &target_text, &record.head])?;
        }
        Some(branch) => {
            let tip = repo.branch_tip(branch)?;
            let elsewhere = repo
                .worktrees()?
                .iter()
                .any(|entry| entry.branch.as_deref() == Some(branch));
            match tip {
                None => {
                    repo.git(&["worktree", "add", "-b", branch, &target_text, &record.head])?;
                    made_branch = Some(branch.clone());
                }
                Some(_) if elsewhere => {
                    warnings.push(format!(
                        "branch {branch} is checked out in another worktree; restored detached at {}",
                        record.head
                    ));
                    repo.git(&["worktree", "add", "--detach", &target_text, &record.head])?;
                }
                Some(tip) if tip != record.head => {
                    warnings.push(format!(
                        "branch {branch} moved from {} to {tip} since the backup; restored detached at {}",
                        record.head, record.head
                    ));
                    repo.git(&["worktree", "add", "--detach", &target_text, &record.head])?;
                }
                Some(_) => {
                    repo.git(&["worktree", "add", &target_text, branch])?;
                }
            }
        }
    }
    Ok(made_branch)
}

/// Deletes `file`, then each directory above it that is left empty, up to but not including `root`.
fn remove_with_empty_parents(root: &Path, file: &Path) -> Result<()> {
    match fs::remove_file(file) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ctx(format!("remove {}", file.display()))(error)),
    }
    for parent in file
        .ancestors()
        .skip(1)
        .take_while(|parent| *parent != root)
    {
        if fs::remove_dir(parent).is_err() {
            break;
        }
    }
    Ok(())
}

/// Restores the saved sparse-checkout patterns and config, then reapplies them. Returns true
/// when it did. A failure is a warning, not an error. It names `kept`, where the backup holds
/// the saved files. The worktree is then a full checkout.
fn reapply_sparse(
    saved: &std::path::Path,
    kept: &std::path::Path,
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
        repo.git(&["config", "extensions.worktreeConfig", "true"])?;
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
        repo.git(&["config", "--list"])?;
        // `sparse-checkout disable` leaves the patterns file and turns the setting off.
        let sparse = repo
            .git_opt(&["config", "--get", "--type=bool", "core.sparseCheckout"])?
            .is_some_and(|value| value == "true");
        if sparse {
            repo.git(&["sparse-checkout", "reapply"])?;
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
                kept.display()
            ));
            false
        }
    }
}

fn sparse_enabled(repo: &Repo) -> bool {
    matches!(
        repo.git_opt(&["config", "--get", "--type=bool", "core.sparseCheckout"]),
        Ok(Some(value)) if value == "true"
    )
}

/// Turns off a sparse checkout that `worktree add` inherited from the main checkout.
fn disable_inherited(repo: &Repo, warnings: &mut Vec<String>) {
    if let Err(error) = repo.git(&["sparse-checkout", "disable"]) {
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
        if deleted.is_empty() {
            return Ok(());
        }
        let wanted: HashSet<&[u8]> = deleted.iter().map(Vec::as_slice).collect();
        // `ls-files` expands a sparse index, so a collapsed directory still lists its files.
        let listing = repo.checked(&["ls-files", "-t", "-z"], None, &[])?;
        let mut skipped: Vec<u8> = Vec::new();
        for entry in listing.split(|byte| *byte == 0) {
            if let Some(path) = entry.strip_prefix(b"S ")
                && wanted.contains(path)
            {
                skipped.extend_from_slice(path);
                skipped.push(0);
            }
        }
        if skipped.is_empty() {
            return Ok(());
        }
        repo.checked(
            &[
                "--literal-pathspecs",
                "rm",
                "--cached",
                "--sparse",
                "--quiet",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
            Some(skipped),
            &[],
        )?;
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
    let mut changed: Vec<u8> = Vec::new();
    let mut deleted: Vec<Vec<u8>> = Vec::new();
    for change in snapshot::changes(&repo, "HEAD", snapshot)? {
        // Submodule commits cannot be checked out as files; the worktree records HEAD's.
        if change.gitlink {
            continue;
        }
        if change.deleted {
            let file = worktree.join(std::ffi::OsStr::from_bytes(&change.path));
            fs::remove_file(&file).map_err(ctx(format!("remove {}", file.display())))?;
            deleted.push(change.path);
            continue;
        }
        changed.extend_from_slice(&change.path);
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
    Ok(deleted)
}

fn restore_modes(target: &std::path::Path, modes: &[backup::FileMode]) -> Result<()> {
    for saved in modes {
        let path = archive::inside(target, &saved.path)?;
        if fs::symlink_metadata(&path).is_ok_and(|found| found.is_file()) {
            fs::set_permissions(&path, fs::Permissions::from_mode(saved.mode & 0o7777))
                .map_err(ctx(format!("restore mode of {}", path.display())))?;
        }
    }
    Ok(())
}
