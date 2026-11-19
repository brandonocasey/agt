//! Backing a worktree up: an archive of its tracked files, working-tree edits included, and
//! its sparse-checkout files. Ignored and untracked files stay out of it.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use super::archive::{self, Writer};
use super::backup::{self, Record};
use super::info;
use crate::core::clock;
use crate::core::error::{Result, ctx, fail};
use crate::core::local::create_dirs;
use crate::git::{GITLINK, Repo, Worktree, nul_fields};

pub struct Taken {
    pub record: Record,
    pub path: PathBuf,
    /// Files that differ between the worktree's HEAD and its working tree.
    pub changed: usize,
}

/// One path that differs between two trees.
pub(super) struct Change {
    pub path: Vec<u8>,
    pub deleted: bool,
    pub gitlink: bool,
    /// The mode and object id the path has in the second tree.
    pub mode: String,
    pub oid: String,
}

/// The paths that differ between the trees `from` and `to`.
pub(super) fn changes(repo: &Repo, from: &str, to: &str) -> Result<Vec<Change>> {
    let raw = repo.checked(
        &["diff-tree", "-r", "--no-renames", "-z", from, to],
        None,
        &[],
    )?;
    let mut fields = raw.split(|byte| *byte == 0);
    let mut found = Vec::new();
    while let Some(header) = fields.next().filter(|header| !header.is_empty()) {
        let Some(path) = fields.next() else { break };
        let header = String::from_utf8_lossy(header);
        let parts: Vec<&str> = header.trim_start_matches(':').split(' ').collect();
        let part = |index: usize| parts.get(index).copied().unwrap_or_default().to_owned();
        found.push(Change {
            path: path.to_vec(),
            deleted: parts.get(4) == Some(&"D"),
            gitlink: parts.iter().take(2).any(|mode| *mode == GITLINK),
            mode: part(1),
            oid: part(3),
        });
    }
    Ok(found)
}

/// Snapshots `entry`, whose repository's main checkout is `main`. Nothing in the worktree
/// changes; its index is not touched.
/// Fails when `entry` has no commit, which a backup needs to restore on.
pub(super) fn require_commit(entry: &Worktree) -> Result<()> {
    if entry.unborn() {
        return fail(format!(
            "{} is on a branch with no commit yet; commit something first, a backup needs a commit to restore on",
            entry.path.display()
        ));
    }
    Ok(())
}

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
    require_commit(entry)?;
    let name = ref_safe(&entry.path.file_name().unwrap_or_default().to_string_lossy());
    let repo_name = main.file_name().unwrap_or_default().to_string_lossy();
    let base = backup::root()?.join(repo_name.as_ref());
    let (id, path) = (1..)
        .map(|attempt| match attempt {
            1 => format!("{stamp}-worktree-{name}"),
            n => format!("{stamp}-worktree-{name}-{n}"),
        })
        .map(|id| {
            let path = base.join(format!("{id}{}", archive::EXT));
            (id, path)
        })
        .find(|(id, path)| !path.exists() && !backup::id_taken(id))
        .unwrap_or_default();
    create_dirs(&base, 0o700)?;
    build(main, entry, &path, id, created)
}

fn build(main: &Path, entry: &Worktree, path: &Path, id: String, created: i64) -> Result<Taken> {
    let shared = &Repo::at(main);
    let tree = Repo::at(&entry.path).worktree_tree_with_hidden_edits()?;
    let changes = changes(shared, &entry.head, &tree)?;
    let record = Record {
        id,
        repository: main.to_owned(),
        worktree: entry.path.clone(),
        branch: entry.branch.clone(),
        head: entry.head.clone(),
        snapshot: None,
        reference: None,
        created: clock::format(created),
        copied: Vec::new(),
        skipped: info::ignored(&entry.path)?,
        file_modes: Vec::new(),
        deleted: changes
            .iter()
            .filter(|change| change.deleted)
            .map(|change| change.path.clone())
            .collect(),
    };
    let mut writer = Writer::create(path, &record)?;
    if let Err(error) = fill(&mut writer, shared, entry, &tree, &changes) {
        writer.discard();
        return Err(error);
    }
    writer.finish()?;
    Ok(Taken {
        record,
        path: path.to_owned(),
        changed: changes.len(),
    })
}

/// Adds the files of `tree` to `writer`. A file of `tree` the worktree does not hold, as in a
/// sparse checkout, is added from its object when it differs from HEAD, and left to HEAD when
/// it does not.
fn fill(
    writer: &mut Writer,
    shared: &Repo,
    entry: &Worktree,
    tree: &str,
    changes: &[Change],
) -> Result<()> {
    save_git_files(&entry.path, writer)?;
    let edited: HashMap<&[u8], &Change> = changes
        .iter()
        .filter(|change| !change.deleted && !change.gitlink)
        .map(|change| (change.path.as_slice(), change))
        .collect();
    let paths = shared.checked(&["ls-tree", "-r", "--name-only", "-z", tree], None, &[])?;
    for path in nul_fields(&paths) {
        let source = entry.path.join(OsStr::from_bytes(path));
        match fs::symlink_metadata(&source) {
            Ok(found) if found.is_file() || found.file_type().is_symlink() => {
                writer.file(path, &source)?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if let Some(change) = edited.get(path) {
                    add_object(writer, shared, change)?;
                }
            }
            Err(error) => return Err(ctx(format!("stat {}", source.display()))(error)),
        }
    }
    Ok(())
}

fn add_object(writer: &mut Writer, shared: &Repo, change: &Change) -> Result<()> {
    let contents = shared.checked(&["cat-file", "blob", &change.oid], None, &[])?;
    let mode = if change.mode == "100755" {
        0o755
    } else {
        0o644
    };
    writer.blob(&change.path, mode, change.mode == "120000", &contents)
}

/// Where a directory backup keeps the per-worktree git files that `git worktree remove` deletes.
pub const SAVED_GIT_DIR: &str = "worktree-git";

/// The per-worktree files of the worktree's git directory: its sparse-checkout patterns and
/// its own config (which holds the sparse-checkout settings).
pub const SAVED_GIT_FILES: [&str; 2] = ["info/sparse-checkout", "config.worktree"];

/// Adds the per-worktree git files that exist in the git directory of the worktree at
/// `worktree` to `writer`.
fn save_git_files(worktree: &Path, writer: &mut Writer) -> Result<()> {
    let git_dir = Repo::at(worktree).git_dir()?;
    for name in SAVED_GIT_FILES {
        let source = git_dir.join(name);
        if source.is_file() {
            writer.git_file(name, &source)?;
        }
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

/// Copies each relative path from `from` to `to`, keeping file modes and symlinks. Missing
/// parent directories are made with `dir_mode`.
pub fn copy_files(from: &Path, to: &Path, rels: &[String], dir_mode: u32) -> Result<()> {
    for rel in rels {
        let (source, target) = (from.join(rel), to.join(rel));
        if let Some(parent) = target.parent() {
            create_dirs(parent, dir_mode)?;
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
