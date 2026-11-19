//! Creating and removing linked worktrees, moving a work tree to a snapshot, and submodules,
//! through libgit2. libgit2 runs no hooks and applies no filter drivers (such as Git LFS), so
//! none of these do.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::index::SKIP_WORKTREE;
use super::{Repo, commit_id, without_trailing_slash};
use crate::core::error::{Error, Result, ctx};

/// What a new worktree checks out.
pub enum Checkout<'a> {
    /// `commit`, with no branch.
    Detached(&'a str),
    /// The existing local branch.
    Branch(&'a str),
    /// A new local branch at `at`, which tracks nothing.
    NewBranch { name: &'a str, at: &'a str },
    /// A new local branch at the remote-tracking branch `upstream` (`origin/<name>`), which it
    /// tracks.
    Tracking { name: &'a str, upstream: &'a str },
}

/// Which submodule remotes an update may clone from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Transports {
    Any,
    /// Only a local path or a `file` URL: nothing is fetched over the network.
    FileOnly,
}

impl Repo {
    /// Adds a linked worktree at `path` with `what` checked out. A sparse checkout of this
    /// repository carries over to it, as `git worktree add` does.
    pub fn add_worktree(&self, path: &Path, what: &Checkout<'_>) -> Result<()> {
        let repo = self.open()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(ctx(format!("create {}", parent.display())))?;
        }
        // git accepts an existing empty directory, libgit2 does not.
        if fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none()) {
            fs::remove_dir(path).map_err(ctx(format!("prepare {}", path.display())))?;
        }
        let name = admin_name(&repo, path);
        match what {
            Checkout::Branch(branch) => add_on_branch(&repo, &name, path, branch)?,
            Checkout::NewBranch { name: branch, at } => {
                repo.branch(branch, &repo.find_commit(commit_id(&repo, at)?)?, false)?;
                add_on_branch(&repo, &name, path, branch)?;
            }
            Checkout::Tracking {
                name: branch,
                upstream,
            } => {
                let start = repo.find_branch(upstream, git2::BranchType::Remote)?;
                let commit = start.get().peel_to_commit()?;
                let mut created = repo.branch(branch, &commit, false)?;
                created.set_upstream(Some(upstream))?;
                add_on_branch(&repo, &name, path, branch)?;
            }
            Checkout::Detached(commit) => add_detached(&repo, &name, path, commit)?,
        }
        if self.sparse_checkout_enabled()? {
            let linked = Repo::at(path);
            copy_sparse_files(&repo, &git2::Repository::open(path)?)?;
            linked.sparse_reapply()?;
        }
        Ok(())
    }

    /// Removes the linked worktree at `path` with its files and its registration, whatever it
    /// holds (`git worktree remove --force`).
    pub fn remove_worktree(&self, path: &Path) -> Result<()> {
        let repo = self.open()?;
        let wanted = comparable(path);
        for name in repo.worktrees()?.iter().flatten() {
            let linked = repo.find_worktree(name)?;
            if comparable(Path::new(linked.path())) != wanted {
                continue;
            }
            let mut options = git2::WorktreePruneOptions::new();
            options.valid(true).locked(true).working_tree(true);
            linked.prune(Some(&mut options))?;
            return Ok(());
        }
        Err(Error(format!("'{}' is not a working tree", path.display())))
    }

    /// Makes the index match HEAD and leaves the files alone (`git reset -q`).
    pub fn reset_index_to_head(&self) -> Result<()> {
        let repo = self.open()?;
        let head = repo.head()?.peel(git2::ObjectType::Commit)?;
        repo.reset(&head, git2::ResetType::Mixed, None)?;
        Ok(())
    }

    /// Writes the `paths` from `commit` into the work tree and the index, replacing what is
    /// there (`git checkout <commit> -- <paths>`).
    pub fn checkout_paths(&self, commit: &str, paths: &[Vec<u8>]) -> Result<()> {
        let repo = self.open()?;
        let commit = repo.find_commit(commit_id(&repo, commit)?)?;
        let mut options = git2::build::CheckoutBuilder::new();
        options.force().disable_pathspec_match(true);
        for path in paths {
            options.path(path.as_slice());
        }
        repo.checkout_tree(commit.as_object(), Some(&mut options))?;
        Ok(())
    }

    /// Removes from the index the `deleted` paths that a sparse checkout marked skip-worktree
    /// (`git rm --cached --sparse`); returns how many.
    pub fn unstage_skipped(&self, deleted: &[Vec<u8>]) -> Result<usize> {
        let repo = self.open()?;
        let real = repo.path().join("index");
        let mut index = repo.index()?;
        if index.path() != Some(real.as_path()) {
            super::sparse_index::expand_in_place(&repo)?;
            repo.set_index(&mut git2::Index::open(&real)?)?;
            index = repo.index()?;
        }
        index.read(false)?;
        let mut removed = 0;
        for path in deleted {
            let at = Path::new(OsStr::from_bytes(path));
            let skipped = index
                .get_path(at, 0)
                .is_some_and(|entry| entry.flags_extended & SKIP_WORKTREE != 0);
            if skipped {
                index.remove_path(at)?;
                removed += 1;
            }
        }
        if removed > 0 {
            index.write()?;
        }
        Ok(removed)
    }

    /// Sets the boolean config key `name` in the repository's own config file.
    pub fn set_config_bool(&self, name: &str, value: bool) -> Result<()> {
        let repo = self.open()?;
        repo.config()?.set_bool(name, value)?;
        Ok(())
    }

    /// Fails when a config file of the repository cannot be read.
    pub fn check_config(&self) -> Result<()> {
        let repo = self.open()?;
        let config = repo.config()?;
        let mut entries = config.entries(None)?;
        while let Some(entry) = entries.next() {
            entry?;
        }
        Ok(())
    }

    /// Initializes and checks out the submodules of this work tree, below each other too;
    /// only the one at `path` when it is given.
    pub fn update_submodules(&self, path: Option<&str>, transports: Transports) -> Result<()> {
        let repo = self.open()?;
        update_in(&repo, path, transports)
    }
}

/// The name of the administrative directory for a worktree at `path`: its last component,
/// made unique the way git does, with a number added.
fn admin_name(repo: &git2::Repository, path: &Path) -> String {
    let base = path.file_name().map_or_else(
        || "worktree".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let taken = |name: &str| repo.commondir().join("worktrees").join(name).exists();
    let mut name = base.clone();
    let mut number = 0;
    while taken(&name) {
        number += 1;
        name = format!("{base}{number}");
    }
    name
}

fn add_on_branch(repo: &git2::Repository, name: &str, path: &Path, branch: &str) -> Result<()> {
    let found = repo.find_branch(branch, git2::BranchType::Local)?;
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(found.get()));
    repo.worktree(name, path, Some(&options))?;
    Ok(())
}

/// libgit2 can only check out a branch, so the worktree is made on a temporary branch at
/// `commit` that is dropped once HEAD is detached.
fn add_detached(repo: &git2::Repository, name: &str, path: &Path, commit: &str) -> Result<()> {
    let id = commit_id(repo, commit)?;
    let temporary = format!("agt-detach-{name}-{}", std::process::id());
    repo.branch(&temporary, &repo.find_commit(id)?, false)?;
    let added = add_on_branch(repo, name, path, &temporary);
    let detached = added.and_then(|()| {
        let linked = git2::Repository::open(path)?;
        linked.set_head_detached(id)?;
        Ok(())
    });
    if let Ok(mut branch) = repo.find_branch(&temporary, git2::BranchType::Local) {
        branch.delete()?;
    }
    detached
}

/// Gives the new worktree the sparse-checkout patterns and per-worktree config of `from`,
/// without the keys that name a bare repository or a work tree.
fn copy_sparse_files(from: &git2::Repository, to: &git2::Repository) -> Result<()> {
    let patterns = from.path().join("info").join("sparse-checkout");
    if patterns.is_file() {
        let target = to.path().join("info");
        fs::create_dir_all(&target).map_err(ctx(format!("create {}", target.display())))?;
        fs::copy(&patterns, target.join("sparse-checkout")).map_err(ctx("copy sparse-checkout"))?;
    }
    let own = from.path().join("config.worktree");
    if own.is_file() {
        let source = git2::Config::open(&own)?;
        let mut copy = git2::Config::open(&to.path().join("config.worktree"))?;
        let mut entries = source.entries(None)?;
        while let Some(entry) = entries.next() {
            let entry = entry?;
            let (Some(name), Some(value)) = (entry.name(), entry.value()) else {
                continue;
            };
            if name != "core.bare" && name != "core.worktree" {
                copy.set_str(name, value)?;
            }
        }
    }
    Ok(())
}

/// `path` as it can be compared with another: resolved when it exists, else its parent resolved.
fn comparable(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    match (
        path.parent().and_then(|dir| fs::canonicalize(dir).ok()),
        path.file_name(),
    ) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => without_trailing_slash(path),
    }
}

fn update_in(repo: &git2::Repository, only: Option<&str>, transports: Transports) -> Result<()> {
    for mut submodule in repo.submodules()? {
        let relative =
            String::from_utf8_lossy(submodule.path().as_os_str().as_bytes()).into_owned();
        if only.is_some_and(|wanted| wanted != relative) {
            continue;
        }
        submodule.init(false)?;
        let url = submodule.url().unwrap_or_default().to_owned();
        if transports == Transports::FileOnly && !is_local(&url) {
            return Err(Error(format!(
                "clone of '{url}' into submodule path '{}' failed",
                repo.workdir()
                    .unwrap_or(Path::new(""))
                    .join(&relative)
                    .display()
            )));
        }
        let config = repo.config()?;
        let mut options = git2::SubmoduleUpdateOptions::new();
        options.fetch(super::network::fetch_options(&config, &url));
        submodule.update(true, Some(&mut options))?;
        update_in(&submodule.open()?, None, transports)?;
    }
    Ok(())
}

/// Whether a submodule URL names a local path or a `file` URL.
fn is_local(url: &str) -> bool {
    url.starts_with('/')
        || url.starts_with("./")
        || url.starts_with("../")
        || url.starts_with("file://")
        || !url.contains("://") && !url.contains('@')
}
