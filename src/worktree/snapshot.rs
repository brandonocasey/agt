//! Backing a worktree up: a commit of its whole working tree pinned by a ref, and a copy of
//! its ignored files that are not disposable.

use std::fs::{self, DirBuilder};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use super::backup::{self, FileMode, REF_PREFIX, Record};
use super::info;
use crate::core::clock;
use crate::core::error::{Result, ctx, fail};
use crate::git::{Repo, Worktree, nul_fields};

pub struct Taken {
    pub record: Record,
    pub dir: PathBuf,
    /// Files that differ between the worktree's HEAD and the snapshot.
    pub changed: usize,
}

/// Snapshots `entry`, whose repository's main checkout is `main`. Nothing in the worktree
/// changes; its index is not touched.
pub fn take(main: &Path, entry: &Worktree) -> Result<Taken> {
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
    entry: &Worktree,
    shared: &Repo,
    dir: &Path,
    id: String,
    reference: &str,
    created: i64,
) -> Result<Taken> {
    let tree = Repo::at(&entry.path).worktree_tree_with_hidden_edits()?;
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
    let changed = shared.checked(
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
    )?;
    let changed = nul_fields(&changed).count();

    let ignored = info::ignored(&entry.path)?;
    copy_files(&entry.path, &dir.join("files"), &ignored.copy, 0o700)?;
    save_git_files(&entry.path, &dir.join(SAVED_GIT_DIR))?;
    let paths = shared.checked(&["ls-tree", "-r", "--name-only", "-z", &tree], None, &[])?;
    let mut file_modes = Vec::new();
    for path in nul_fields(&paths) {
        let source = entry.path.join(std::ffi::OsStr::from_bytes(path));
        let metadata = match fs::symlink_metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(ctx(format!("stat {}", source.display()))(error)),
        };
        if metadata.is_file() {
            file_modes.push(FileMode {
                path: path.to_vec(),
                mode: metadata.permissions().mode() & 0o7777,
            });
        }
    }
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
        file_modes,
    };
    backup::write(dir, &record)?;
    Ok(Taken {
        record,
        dir: dir.to_owned(),
        changed,
    })
}

/// Where a backup keeps the per-worktree git files that `git worktree remove` deletes.
pub const SAVED_GIT_DIR: &str = "worktree-git";

/// The per-worktree files of the worktree's git directory: its sparse-checkout patterns and
/// its own config (which holds the sparse-checkout settings).
pub const SAVED_GIT_FILES: [&str; 2] = ["info/sparse-checkout", "config.worktree"];

/// Copies the per-worktree git files that exist from the git directory of the worktree at
/// `worktree` into `to`.
fn save_git_files(worktree: &Path, to: &Path) -> Result<()> {
    let git_dir = Repo::at(worktree).git_dir()?;
    for name in SAVED_GIT_FILES {
        let source = git_dir.join(name);
        if !source.is_file() {
            continue;
        }
        let target = to.join(name);
        if let Some(parent) = target.parent() {
            make_dir(parent, 0o700)?;
        }
        fs::copy(&source, &target).map_err(ctx(format!(
            "copy {} to {}",
            source.display(),
            target.display()
        )))?;
    }
    Ok(())
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
    let mut trimmed = out.trim_start_matches('.');
    // Stripping one ending can expose the other (`x.lock.`, `x.lock.lock`), so go until neither is left.
    loop {
        let shorter = trimmed
            .trim_end_matches('.')
            .strip_suffix(".lock")
            .unwrap_or_else(|| trimmed.trim_end_matches('.'));
        if shorter.len() == trimmed.len() {
            break;
        }
        trimmed = shorter;
    }
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
    use crate::core::testing::Sandbox;

    /// What `git check-ref-format` says of the backup ref for `name`.
    fn valid(name: &str) -> bool {
        Sandbox::new()
            .command("git")
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
            "x.lock.lock",
            "x.lock.",
            "x..lock",
            "x.lock.lock.",
            ".lock",
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
