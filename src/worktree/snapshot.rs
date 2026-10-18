//! Backing a worktree up: a commit of its whole working tree pinned by a ref, and a copy of
//! its ignored files that are not disposable.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};

use super::backup::{self, REF_PREFIX, Record};
use super::info::{self, Entry};
use crate::core::clock;
use crate::core::error::{Result, ctx};
use crate::git::Repo;

pub struct Taken {
    pub record: Record,
    pub dir: PathBuf,
    /// Files that differ between the worktree's HEAD and the snapshot.
    pub changed: usize,
}

/// Snapshots `entry`, whose repository's main checkout is `main`. Nothing in the worktree
/// changes; its index is not touched.
pub fn take(main: &Path, entry: &Entry) -> Result<Taken> {
    let created = clock::now_secs()?;
    let stamp = clock::format(created);
    let stamp = format!(
        "{}{}{}-{}{}",
        &stamp[0..4],
        &stamp[5..7],
        &stamp[8..10],
        &stamp[11..13],
        &stamp[14..16]
    );
    let name = entry.path.file_name().unwrap_or_default().to_string_lossy();
    let repo_name = main.file_name().unwrap_or_default().to_string_lossy();
    let shared = Repo::at(main);
    let base = backup::root()?.join(repo_name.as_ref());
    let (id, dir) = (1..)
        .map(|attempt| match attempt {
            1 => format!("{stamp}-worktree-{name}"),
            n => format!("{stamp}-worktree-{name}-{n}"),
        })
        .map(|id| {
            let dir = base.join(&id);
            (id, dir)
        })
        .find(|(id, dir)| {
            let reference = format!("{REF_PREFIX}{id}");
            !dir.exists()
                && shared
                    .git_opt(&["rev-parse", "--verify", "--quiet", &reference])
                    .is_ok_and(|found| found.is_none())
        })
        .unwrap_or_default();
    make_dir(&dir.join("files"), 0o700)?;

    let tree = Repo::at(&entry.path).worktree_tree()?;
    let message = format!("agt worktree backup {id}");
    let snapshot = shared.git(&[
        "-c",
        "user.name=agt",
        "-c",
        "user.email=agt@localhost",
        "commit-tree",
        &tree,
        "-p",
        &entry.head,
        "-m",
        &message,
    ])?;
    let reference = format!("{REF_PREFIX}{id}");
    shared.git(&["update-ref", &reference, &snapshot])?;
    let changed = shared
        .checked(
            &[
                "diff-tree",
                "-r",
                "--no-renames",
                "--name-only",
                "-z",
                &entry.head,
                &snapshot,
            ],
            None,
            &[],
        )?
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .count();

    let ignored = info::ignored(&entry.path)?;
    copy_files(&entry.path, &dir.join("files"), &ignored.copy, 0o700)?;
    let record = Record {
        id,
        repository: main.to_owned(),
        worktree: entry.path.clone(),
        branch: entry.branch.clone(),
        head: entry.head.clone(),
        snapshot,
        reference,
        created: clock::format(created),
        copied: ignored.copy,
        skipped: ignored.skipped,
    };
    backup::write(&dir, &record)?;
    Ok(Taken {
        record,
        dir,
        changed,
    })
}

fn make_dir(dir: &Path, mode: u32) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(dir)
        .map_err(ctx(format!("create {}", dir.display())))
}

/// Copies each relative path from `from` to `to`, keeping file modes and symlinks. Missing
/// parent directories are made with `dir_mode`.
pub fn copy_files(from: &Path, to: &Path, rels: &[String], dir_mode: u32) -> Result<()> {
    for rel in rels {
        let (source, target) = (from.join(rel), to.join(rel));
        if let Some(parent) = target.parent() {
            make_dir(parent, dir_mode)?;
        }
        let kind = fs::symlink_metadata(&source)
            .map_err(ctx(format!("stat {}", source.display())))?
            .file_type();
        if kind.is_symlink() {
            let link = fs::read_link(&source).map_err(ctx(format!("read {}", source.display())))?;
            let _ = fs::remove_file(&target);
            symlink(&link, &target).map_err(ctx(format!("link {}", target.display())))?;
        } else {
            fs::copy(&source, &target).map_err(ctx(format!(
                "copy {} to {}",
                source.display(),
                target.display()
            )))?;
        }
    }
    Ok(())
}
