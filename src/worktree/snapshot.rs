//! Backing a worktree up: a commit of its whole working tree pinned by a ref, and a copy of
//! its ignored files that are not disposable.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};

use super::backup::{self, REF_PREFIX, Record};
use super::info::{self, Entry};
use crate::core::clock;
use crate::core::error::{Result, ctx, fail};
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
    if entry.head.chars().all(|c| c == '0') {
        return fail(format!(
            "{} is on a branch with no commit yet; commit something first, a backup needs a commit to restore on",
            entry.path.display()
        ));
    }
    let name = ref_safe(&entry.path.file_name().unwrap_or_default().to_string_lossy());
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
                && !backup::id_taken(id)
                && shared
                    .git_opt(&["rev-parse", "--verify", "--quiet", &reference])
                    .is_ok_and(|found| found.is_none())
        })
        .unwrap_or_default();
    make_dir(&dir.join("files"), 0o700)?;
    let reference = format!("{REF_PREFIX}{id}");
    let made = build(main, entry, &shared, &dir, id, &reference, created);
    if made.is_err() {
        // A failed backup leaves neither the ref nor the directory behind.
        let _ = shared.git(&["update-ref", "-d", &reference]);
        let _ = fs::remove_dir_all(&dir);
    }
    made
}

fn build(
    main: &Path,
    entry: &Entry,
    shared: &Repo,
    dir: &Path,
    id: String,
    reference: &str,
    created: i64,
) -> Result<Taken> {
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
    shared.git(&["update-ref", reference, &snapshot])?;
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
        reference: reference.to_owned(),
        created: clock::format(created),
        copied: ignored.copy,
        skipped: ignored.skipped,
    };
    backup::write(dir, &record)?;
    Ok(Taken {
        record,
        dir: dir.to_owned(),
        changed,
    })
}

/// `name` as part of a ref: every run of characters git refuses in a ref name (space, `~`,
/// `^`, `:`, `?`, `*`, `[`, `\`, control characters, `..`, `@{`) becomes `-`, and a leading
/// `.`, a trailing `.` or a trailing `.lock` is dropped.
fn ref_safe(name: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    let mut previous = ' ';
    for ch in name.chars() {
        let bad =
            !(ch.is_alphanumeric() || "_-+=,@%".contains(ch) || (ch == '.' && previous != '.'));
        if bad {
            if !gap {
                out.push('-');
            }
            gap = true;
        } else {
            out.push(ch);
            gap = false;
        }
        previous = if bad { ' ' } else { ch };
    }
    let trimmed = out.trim_start_matches('.').trim_end_matches('.');
    let trimmed = trimmed.strip_suffix(".lock").unwrap_or(trimmed);
    let trimmed = trimmed.replace("@-", "-");
    if trimmed.is_empty() {
        "worktree".to_owned()
    } else {
        trimmed
    }
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

#[cfg(test)]
mod tests {
    use super::ref_safe;

    /// What `git check-ref-format` says of the backup ref for `name`.
    fn valid(name: &str) -> bool {
        std::process::Command::new("git")
            .args([
                "check-ref-format",
                &format!("refs/agt/backups/20260101-0000-worktree-{name}"),
            ])
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn a_worktree_name_becomes_a_valid_ref_part() {
        for name in [
            "my tree",
            "a~1",
            "a^b",
            "a:b",
            "a..b",
            "x.lock",
            "trail.",
            ".hidden",
            "a?b*c[d",
            "a\\b",
            "a@{b",
            "tab\there",
            "ünï cödé",
            "feat.v1.2",
            "",
        ] {
            let safe = ref_safe(name);
            assert!(valid(&safe), "{name:?} became {safe:?}");
        }
        assert_eq!(ref_safe("feat.v1.2"), "feat.v1.2");
        assert_eq!(ref_safe("my  tree~1"), "my-tree-1");
    }
}
