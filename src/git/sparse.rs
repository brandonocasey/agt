//! Sparse checkout, which libgit2 does not have: reapplying the patterns of
//! `info/sparse-checkout` to the index and the work tree, and turning the checkout off.
//! Cone and non-cone pattern files are read the same way: as gitignore patterns that select
//! the files to keep, the last matching pattern winning.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::index::{SKIP_WORKTREE, STAGE_MASK, copy_entry, refreshed_index};
use super::{GITLINK_MODE, Repo};
use crate::core::error::{Error, Result, ctx, fail};

impl Repo {
    /// Whether `core.sparseCheckout` is on.
    pub fn sparse_checkout_enabled(&self) -> Result<bool> {
        let repo = self.open_refs()?;
        is_enabled(&repo)
    }

    /// Applies the sparse-checkout patterns again (`git sparse-checkout reapply`): files they
    /// leave out are removed and marked skip-worktree unless they have local changes, files
    /// they take in come back.
    pub fn sparse_reapply(&self) -> Result<()> {
        let repo = self.open_writing()?;
        reapply(&repo)
    }

    /// Brings every file back and turns the sparse checkout off (`git sparse-checkout
    /// disable`). The patterns file stays, and the setting is written to the worktree's own
    /// config, as git writes it.
    pub fn sparse_disable(&self) -> Result<()> {
        let repo = self.open_writing()?;
        apply(&repo, |_| true)?;
        repo.config()?.set_bool("extensions.worktreeConfig", true)?;
        let mut own = git2::Config::open(&repo.path().join("config.worktree"))?;
        own.set_bool("core.sparseCheckout", false)?;
        own.set_bool("core.sparseCheckoutCone", false)?;
        own.set_bool("index.sparse", false)?;
        Ok(())
    }
}

/// Whether `core.sparseCheckout` is on in `repo`.
pub(super) fn is_enabled(repo: &git2::Repository) -> Result<bool> {
    Ok(repo
        .config()?
        .get_bool("core.sparseCheckout")
        .unwrap_or(false))
}

/// [`Repo::sparse_reapply`] on an open `repo`, whose index MUST be writable.
pub(super) fn reapply(repo: &git2::Repository) -> Result<()> {
    if !is_enabled(repo)? {
        return fail("must be in a sparse-checkout to reapply sparsity patterns");
    }
    let file = repo.path().join("info").join("sparse-checkout");
    let patterns = Patterns::read(&file)?;
    apply(repo, |path| patterns.selects(path))
}

/// Moves the index and the work tree to what `selected` says each path should be.
fn apply(repo: &git2::Repository, selected: impl Fn(&[u8]) -> bool) -> Result<()> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| Error("this operation must be run in a work tree".into()))?;
    let mut index = refreshed_index(repo)?;
    let entries: Vec<git2::IndexEntry> = index.iter().collect();
    let mut restore = Vec::new();
    let mut emptied = Vec::new();
    for entry in entries.iter().filter(|entry| entry.flags & STAGE_MASK == 0) {
        let file = workdir.join(OsStr::from_bytes(&entry.path));
        let skipped = entry.flags_extended & SKIP_WORKTREE != 0;
        let mut changed = copy_entry(entry);
        if selected(&entry.path) && skipped {
            changed.flags_extended &= !SKIP_WORKTREE;
            if fs::symlink_metadata(&file).is_err() {
                restore.push(file);
            }
        } else if !selected(&entry.path) && !skipped {
            if !leave_out(&file, entry)? {
                continue;
            }
            changed.flags_extended |= SKIP_WORKTREE;
            emptied.extend(file.parent().map(Path::to_owned));
        } else {
            continue;
        }
        index.add(&changed)?;
    }
    index.write()?;
    check_out(repo, &mut index, workdir, &restore)?;
    for dir in emptied {
        remove_empty_dirs(&dir, workdir);
    }
    Ok(())
}

/// Removes `file`, which the patterns leave out, unless it differs from the index; returns
/// whether the entry may now be marked skip-worktree.
fn leave_out(file: &Path, entry: &git2::IndexEntry) -> Result<bool> {
    let Ok(meta) = fs::symlink_metadata(file) else {
        return Ok(true);
    };
    if meta.is_dir() || entry.mode == GITLINK_MODE {
        return Ok(true);
    }
    let on_disk = if meta.is_symlink() {
        let target = fs::read_link(file).map_err(ctx(format!("read {}", file.display())))?;
        git2::Oid::hash_object(git2::ObjectType::Blob, target.as_os_str().as_bytes())?
    } else {
        git2::Oid::hash_file(git2::ObjectType::Blob, file)?
    };
    if on_disk != entry.id {
        return Ok(false);
    }
    fs::remove_file(file).map_err(ctx(format!("remove {}", file.display())))?;
    Ok(true)
}

fn check_out(
    repo: &git2::Repository,
    index: &mut git2::Index,
    workdir: &Path,
    files: &[PathBuf],
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let mut options = git2::build::CheckoutBuilder::new();
    options
        .force()
        .update_index(false)
        .disable_pathspec_match(true);
    for file in files {
        let relative = file.strip_prefix(workdir).unwrap_or(file);
        options.path(relative);
    }
    repo.checkout_index(Some(index), Some(&mut options))?;
    Ok(())
}

/// Removes `dir` and its parents below `workdir` for as long as they are empty.
fn remove_empty_dirs(dir: &Path, workdir: &Path) {
    let mut current = Some(dir);
    while let Some(dir) = current.filter(|dir| *dir != workdir && dir.starts_with(workdir)) {
        if fs::remove_dir(dir).is_err() {
            return;
        }
        current = dir.parent();
    }
}

/// The patterns of a sparse-checkout file.
struct Patterns(Vec<Pattern>);

struct Pattern {
    negated: bool,
    directory_only: bool,
    /// Matches the whole path, not just a file name at any depth.
    anchored: bool,
    segments: Vec<Vec<u8>>,
}

impl Patterns {
    fn read(file: &Path) -> Result<Self> {
        let text = match fs::read(file) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(ctx(format!("read {}", file.display()))(error)),
        };
        Ok(Self(
            text.split(|byte| *byte == b'\n')
                .filter_map(Pattern::parse)
                .collect(),
        ))
    }

    /// Whether the file at `path` is kept: the nearest directory or file that a pattern
    /// matches decides, the last matching pattern winning.
    fn selects(&self, path: &[u8]) -> bool {
        let components: Vec<&[u8]> = path.split(|byte| *byte == b'/').collect();
        let mut decision = false;
        for depth in 1..=components.len() {
            let is_directory = depth < components.len();
            if let Some(selected) = self.decide(&components[..depth], is_directory) {
                decision = selected;
            }
        }
        decision
    }

    fn decide(&self, path: &[&[u8]], is_directory: bool) -> Option<bool> {
        self.0
            .iter()
            .rev()
            .find(|pattern| pattern.matches(path, is_directory))
            .map(|pattern| !pattern.negated)
    }
}

impl Pattern {
    fn parse(line: &[u8]) -> Option<Self> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line = trim_unescaped_trailing_space(line);
        if line.is_empty() || line.starts_with(b"#") {
            return None;
        }
        let (negated, line) = match line.strip_prefix(b"!") {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let (directory_only, line) = match line.strip_suffix(b"/") {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let anchored = line.contains(&b'/');
        let line = line.strip_prefix(b"/").unwrap_or(line);
        Some(Self {
            negated,
            directory_only,
            anchored,
            segments: line
                .split(|byte| *byte == b'/')
                .map(<[u8]>::to_vec)
                .collect(),
        })
    }

    fn matches(&self, path: &[&[u8]], is_directory: bool) -> bool {
        if self.directory_only && !is_directory {
            return false;
        }
        if self.anchored {
            return match_segments(&self.segments, path);
        }
        path.last()
            .is_some_and(|name| wild(&self.segments[0], name))
    }
}

fn trim_unescaped_trailing_space(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && line[end - 1] == b' ' && !(end > 1 && line[end - 2] == b'\\') {
        end -= 1;
    }
    &line[..end]
}

/// Whether `path` components match the pattern `segments`, where `**` stands for any number
/// of directories.
fn match_segments(segments: &[Vec<u8>], path: &[&[u8]]) -> bool {
    let Some((first, rest)) = segments.split_first() else {
        return path.is_empty();
    };
    if first == b"**" {
        return (0..=path.len()).any(|skip| match_segments(rest, &path[skip..]));
    }
    path.split_first()
        .is_some_and(|(name, below)| wild(first, name) && match_segments(rest, below))
}

/// Shell-style match of one path component: `*`, `?`, `[...]` and `\` escapes.
fn wild(pattern: &[u8], text: &[u8]) -> bool {
    let Some((&first, rest)) = pattern.split_first() else {
        return text.is_empty();
    };
    match first {
        b'*' => (0..=text.len()).any(|skip| wild(rest, &text[skip..])),
        b'?' => !text.is_empty() && wild(rest, &text[1..]),
        b'[' => match (text.split_first(), class_end(pattern)) {
            (Some((&byte, below)), Some(end)) => {
                in_class(&pattern[1..end], byte) && wild(&pattern[end + 1..], below)
            }
            _ => false,
        },
        b'\\' if !rest.is_empty() => text.first() == Some(&rest[0]) && wild(&rest[1..], &text[1..]),
        literal => text.first() == Some(&literal) && wild(rest, &text[1..]),
    }
}

/// The index of the `]` that closes the class `pattern` opens, if it has one.
fn class_end(pattern: &[u8]) -> Option<usize> {
    let skip = if matches!(pattern.get(1), Some(b'!' | b'^')) {
        3
    } else {
        2
    };
    pattern
        .iter()
        .enumerate()
        .skip(skip)
        .find(|(_, byte)| **byte == b']')
        .map(|(at, _)| at)
}

fn in_class(class: &[u8], byte: u8) -> bool {
    let (negated, class) = match class.split_first() {
        Some((b'!' | b'^', rest)) => (true, rest),
        _ => (false, class),
    };
    let mut found = false;
    let mut at = 0;
    while at < class.len() {
        if class.get(at + 1) == Some(&b'-') && at + 2 < class.len() {
            found |= (class[at]..=class[at + 2]).contains(&byte);
            at += 3;
        } else {
            found |= class[at] == byte;
            at += 1;
        }
    }
    found != negated
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::super::fixtures::commit;
    use crate::core::testing::Sandbox;
    use crate::git::Repo;

    /// The files, the skip-worktree marks, the status and the sparse config of the work tree at
    /// `dir`, as the command line reports them.
    fn state(sandbox: &Sandbox, dir: &Path) -> String {
        let script = "git ls-files -t; find . -path ./.git -prune -o -type f -print | sort; \
                      git status --porcelain; \
                      for key in core.sparseCheckout core.sparseCheckoutCone index.sparse; do \
                      git config --get $key || echo \"$key unset\"; done";
        sandbox.shell_output(dir, script)
    }

    /// Two clones of one repository with the same sparse checkout, local changes inside the
    /// directories it is about to leave, and the patterns for what it moves to written in both.
    fn pair(sandbox: &Sandbox, cone: bool) -> (PathBuf, PathBuf) {
        let origin = sandbox.path().join("origin");
        fs::create_dir(&origin).unwrap();
        sandbox.git(&origin, &["init", "-q", "-b", "main"]);
        commit(
            sandbox,
            &origin,
            &[
                ("top.txt", "top\n"),
                ("a/a.txt", "a\n"),
                ("a/deep/x.txt", "x\n"),
                ("b/b.txt", "b\n"),
                ("c/d/d.txt", "d\n"),
                ("c/e/e.txt", "e\n"),
            ],
            "tree",
        );
        let (first, second) = if cone {
            (vec!["--cone", "a", "c/d"], vec!["--cone", "b", "c/e"])
        } else {
            (
                vec!["--no-cone", "/a/", "/c/d/"],
                vec!["--no-cone", "/b/", "/c/e/", "!/c/e/e.txt"],
            )
        };
        let template = sandbox.path().join("template");
        let make = |dir: &Path, args: &[&str]| {
            let mut command = vec!["sparse-checkout", "set"];
            command.extend(args);
            sandbox.git(dir, &command);
        };
        sandbox.git(
            sandbox.path(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                template.to_str().unwrap(),
            ],
        );
        make(&template, &second);
        let patterns = fs::read(template.join(".git/info/sparse-checkout")).unwrap();
        let mut clones = Vec::new();
        for name in ["ours", "theirs"] {
            let dir = sandbox.path().join(name);
            sandbox.git(
                sandbox.path(),
                &[
                    "clone",
                    "-q",
                    origin.to_str().unwrap(),
                    dir.to_str().unwrap(),
                ],
            );
            make(&dir, &first);
            fs::write(dir.join("a/a.txt"), "edited\n").unwrap();
            fs::write(dir.join("a/untracked.txt"), "u\n").unwrap();
            fs::write(dir.join(".git/info/sparse-checkout"), &patterns).unwrap();
            clones.push(dir);
        }
        (clones.remove(0), clones.remove(0))
    }

    #[test]
    fn reapply_and_disable_leave_the_work_tree_as_git_sparse_checkout_does() {
        for cone in [true, false] {
            let sandbox = Sandbox::new();
            let (ours, theirs) = pair(&sandbox, cone);

            Repo::at(&ours).sparse_reapply().unwrap();
            sandbox.git(&theirs, &["sparse-checkout", "reapply"]);
            let reapplied = state(&sandbox, &theirs);
            assert!(
                reapplied.contains("a/a.txt"),
                "the edited file stays: {reapplied}"
            );
            assert_eq!(state(&sandbox, &ours), reapplied, "reapply, cone={cone}");

            Repo::at(&ours).sparse_disable().unwrap();
            sandbox.git(&theirs, &["sparse-checkout", "disable"]);
            assert_eq!(
                state(&sandbox, &ours),
                state(&sandbox, &theirs),
                "disable, cone={cone}"
            );
        }
    }

    #[test]
    fn reapply_without_a_sparse_checkout_is_refused_as_git_refuses() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        commit(&sandbox, &dir, &[("a.txt", "a\n")], "one");
        let refused = Repo::at(&dir).sparse_reapply().unwrap_err().to_string();
        assert!(
            refused.contains("must be in a sparse-checkout"),
            "{refused}"
        );
        let output = sandbox
            .command("git")
            .args(["sparse-checkout", "reapply"])
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
}
