//! Creating and removing linked worktrees, moving a work tree to a snapshot, and submodules.
//! They go through libgit2, which runs no hooks.
//! It applies no filter drivers, such as Git Large File Storage.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::deep::{check_out, is_too_deep};
use super::index::{SKIP_WORKTREE, refreshed_index};
use super::{Repo, commit_id, configured, sparse, without_trailing_slash};
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
        // `checkout_blocker` opens the repository itself, which a read-only `Repo` locks.
        if let Checkout::Branch(branch) = what {
            self.ensure_not_checked_out(branch)?;
        }
        let repo = self.open_refs()?;
        prepare_path(path)?;
        let head = new_head(&repo, what)?;
        let admin = repo
            .commondir()
            .join("worktrees")
            .join(admin_name(&repo, path));
        link_worktree(&admin, path, &head)?;
        fill_worktree(&repo, path).inspect_err(|_| discard_worktree(&admin, path))
    }

    /// Fails when a work tree has `branch` checked out, or is rebasing or bisecting it, which
    /// git refuses to check out a second time.
    fn ensure_not_checked_out(&self, branch: &str) -> Result<()> {
        match self.checkout_blocker(branch)? {
            Some(blocker) => Err(Error(format!(
                "branch {branch} is already checked out at {}",
                blocker.display()
            ))),
            None => Ok(()),
        }
    }

    /// Removes the linked worktree at `path` with its files and its registration, whatever it
    /// holds, but not when it is locked or its `.git` file is gone (`git worktree remove
    /// --force`). The files go first, so a failure leaves a registration that a second call
    /// removes.
    pub fn remove_worktree(&self, path: &Path) -> Result<()> {
        let repo = self.open_refs()?;
        let wanted = comparable(path);
        for name in repo.worktrees()?.iter().flatten() {
            let linked = repo.find_worktree(name)?;
            let work_tree = Path::new(linked.path());
            if comparable(work_tree) != wanted {
                continue;
            }
            if let git2::WorktreeLockStatus::Locked(reason) = linked.is_locked()? {
                return Err(Error(match reason {
                    Some(reason) => {
                        format!("cannot remove a locked working tree, lock reason: {reason}")
                    }
                    None => "cannot remove a locked working tree".to_owned(),
                }));
            }
            // Without its `.git` the directory may be an unrelated checkout that `Repo::at` would
            // resolve to, so git refuses and leaves the files. A missing directory is only pruned.
            // `remove_dir_all` replaces libgit2's prune, which stops 100 directory levels down.
            if fs::symlink_metadata(work_tree).is_ok() {
                let git_file = work_tree.join(".git");
                if fs::symlink_metadata(&git_file).is_err() {
                    return Err(Error(format!(
                        "validation failed, cannot remove working tree: '{}' does not exist",
                        git_file.display()
                    )));
                }
                fs::remove_dir_all(work_tree)
                    .map_err(ctx(format!("remove {}", work_tree.display())))?;
            }
            let mut options = git2::WorktreePruneOptions::new();
            options.valid(true);
            linked.prune(Some(&mut options))?;
            return Ok(());
        }
        Err(Error(format!("'{}' is not a working tree", path.display())))
    }

    /// Makes the index match HEAD and leaves the files alone (`git reset -q`).
    pub fn reset_index_to_head(&self) -> Result<()> {
        let repo = self.open_writing()?;
        let head = repo.head()?.peel(git2::ObjectType::Commit)?;
        repo.reset(&head, git2::ResetType::Mixed, None)?;
        Ok(())
    }

    /// Writes the `paths` from `commit` into the work tree and the index, replacing what is
    /// there (`git checkout <commit> -- <paths>`).
    pub fn checkout_paths(&self, commit: &str, paths: &[Vec<u8>]) -> Result<()> {
        let repo = self.open_writing()?;
        let commit = repo.find_commit(commit_id(&repo, commit)?)?;
        let mut options = git2::build::CheckoutBuilder::new();
        options.force().disable_pathspec_match(true);
        for path in paths {
            options.path(path.as_slice());
        }
        match repo.checkout_tree(commit.as_object(), Some(&mut options)) {
            Err(error) if is_too_deep(&error) => check_out(&repo, &commit.tree()?, paths),
            checked => Ok(checked?),
        }
    }

    /// Removes from the index the `deleted` paths that a sparse checkout marked skip-worktree
    /// (`git rm --cached --sparse`); returns how many.
    pub fn unstage_skipped(&self, deleted: &[Vec<u8>]) -> Result<usize> {
        let repo = self.open_writing()?;
        let mut index = refreshed_index(&repo)?;
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
        let repo = self.open_refs()?;
        repo.config()?.set_bool(name, value)?;
        Ok(())
    }

    /// Fails when a config file of the repository cannot be read.
    pub fn check_config(&self) -> Result<()> {
        let repo = self.open_refs()?;
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

/// Makes sure nothing is at `path` but room for a new work tree: its parent exists and an empty
/// directory is gone, as in git.
fn prepare_path(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(ctx(format!("create {}", parent.display())))?;
    }
    if fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none()) {
        fs::remove_dir(path).map_err(ctx(format!("prepare {}", path.display())))?;
    }
    if fs::symlink_metadata(path).is_ok() {
        return Err(Error(format!("'{}' already exists", path.display())));
    }
    Ok(())
}

/// The HEAD a new worktree gets for `what`, creating the local branch it names when it is new.
fn new_head(repo: &git2::Repository, what: &Checkout<'_>) -> Result<Head> {
    let branch_head = |branch: &git2::Branch<'_>| {
        Head::Branch(branch.get().name().unwrap_or_default().to_owned())
    };
    Ok(match what {
        Checkout::Branch(branch) => {
            branch_head(&repo.find_branch(branch, git2::BranchType::Local)?)
        }
        Checkout::NewBranch { name, at } => {
            let start = repo.find_commit(commit_id(repo, at)?)?;
            branch_head(&repo.branch(name, &start, false)?)
        }
        Checkout::Tracking { name, upstream } => {
            let start = repo.find_branch(upstream, git2::BranchType::Remote)?;
            let mut created = repo.branch(name, &start.get().peel_to_commit()?, false)?;
            created.set_upstream(Some(upstream))?;
            branch_head(&created)
        }
        Checkout::Detached(commit) => Head::Detached(commit_id(repo, commit)?),
    })
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

/// The `commondir` file of a linked worktree; a relative link survives moving the repository.
/// The administrative directory is two levels below the common git directory.
const COMMONDIR: &str = "../..\n";

/// What the HEAD of a new worktree holds.
enum Head {
    /// The full name of a local branch.
    Branch(String),
    Detached(git2::Oid),
}

/// Registers a linked worktree from its administrative directory `admin` and its `path`.
/// They hold only the links between them and a HEAD, with no file checked out.
/// A failure removes only the directories this call created.
fn link_worktree(admin: &Path, path: &Path, head: &Head) -> Result<()> {
    let create = |dir: &Path| fs::create_dir(dir).map_err(ctx(format!("create {}", dir.display())));
    if let Some(worktrees) = admin.parent() {
        fs::create_dir_all(worktrees).map_err(ctx(format!("create {}", worktrees.display())))?;
    }
    create(admin)?;
    if let Err(error) = create(path) {
        let _ = fs::remove_dir_all(admin);
        return Err(error);
    }
    write_links(admin, path, head).inspect_err(|_| discard_worktree(admin, path))
}

/// Removes the directories of a linked worktree that was never completed.
fn discard_worktree(admin: &Path, path: &Path) {
    let _ = fs::remove_dir_all(admin);
    let _ = fs::remove_dir_all(path);
}

/// Writes the links between `admin` and `path`, and the HEAD of the worktree.
fn write_links(admin: &Path, path: &Path, head: &Head) -> Result<()> {
    let real =
        |dir: &Path| fs::canonicalize(dir).map_err(ctx(format!("resolve {}", dir.display())));
    let (admin, work) = (real(admin)?, real(path)?);
    let head_text = match head {
        Head::Branch(refname) => format!("ref: {refname}\n"),
        Head::Detached(id) => format!("{id}\n"),
    };
    let files = [
        (admin.join("commondir"), COMMONDIR.to_owned()),
        (
            admin.join("gitdir"),
            format!("{}\n", work.join(".git").display()),
        ),
        (admin.join("HEAD"), head_text),
        (work.join(".git"), format!("gitdir: {}\n", admin.display())),
    ];
    for (file, text) in files {
        fs::write(&file, text).map_err(ctx(format!("write {}", file.display())))?;
    }
    Ok(())
}

/// Checks out HEAD in the worktree at `path`, only the files the sparse-checkout patterns of
/// `main` select when it has any.
fn fill_worktree(main: &git2::Repository, path: &Path) -> Result<()> {
    let linked = git2::Repository::open(path)?;
    if sparse::is_enabled(main)? {
        copy_sparse_files(main, &linked)?;
        return Repo::at(path).sparse_check_out_head();
    }
    linked.checkout_head(None)?;
    Ok(())
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
        let wanted = match only {
            Some(only) => only == relative,
            None => is_active(repo, &submodule)?,
        };
        if !wanted {
            continue;
        }
        update_submodule(repo, &mut submodule, &relative, transports)?;
    }
    Ok(())
}

/// Registers `submodule` at `relative`, then clones or checks it out and updates its own
/// submodules, unless its update setting is `none`.
fn update_submodule(
    repo: &git2::Repository,
    submodule: &mut git2::Submodule<'_>,
    relative: &str,
    transports: Transports,
) -> Result<()> {
    submodule.init(false)?;
    if update_is_none(repo, submodule)? {
        return Ok(());
    }
    let url = clone_url(repo, submodule)?;
    let refused = match transports {
        Transports::FileOnly => !is_file_transport(&url),
        Transports::Any => {
            is_file_transport(&url) && needs_transport(submodule) && !file_transport_allowed(repo)?
        }
    };
    if refused {
        let failed = format!(
            "clone of '{url}' into submodule path '{}' failed",
            repo.workdir()
                .unwrap_or(Path::new(""))
                .join(relative)
                .display()
        );
        return Err(Error(match transports {
            Transports::FileOnly => failed,
            Transports::Any => format!("transport 'file' not allowed\n{failed}"),
        }));
    }
    let config = repo.config()?;
    let mut options = git2::SubmoduleUpdateOptions::new();
    options.fetch(super::network::fetch_options(&config, &url)?);
    submodule
        .update(true, Some(&mut options))
        .map_err(super::network::explain)?;
    update_in(&submodule.open()?, None, transports)
}

/// Whether `git submodule update --init` takes `submodule` in. Only when `submodule.active` lists
/// pathspecs does it leave some out: those whose `submodule.<name>.active` is false, or, where
/// that is unset, whose path no pathspec matches.
fn is_active(repo: &git2::Repository, submodule: &git2::Submodule<'_>) -> Result<bool> {
    let config = repo.config()?;
    let mut patterns = Vec::new();
    let mut entries = config.multivar("submodule.active", None)?;
    while let Some(entry) = entries.next() {
        patterns.extend(entry?.value().map(str::to_owned));
    }
    if patterns.is_empty() {
        return Ok(true);
    }
    let name = submodule.name().unwrap_or_default();
    if let Ok(named) = config.get_bool(&format!("submodule.{name}.active")) {
        return Ok(named);
    }
    let pathspec = git2::Pathspec::new(patterns.iter().map(|pattern| match pattern.as_str() {
        "." => "*",
        other => other,
    }))?;
    Ok(pathspec.matches_path(submodule.path(), git2::PathspecFlags::DEFAULT))
}

/// Whether `submodule.<name>.update` is `none`, which the config sets over `.gitmodules`. libgit2
/// reads that key from `.gitmodules` only.
fn update_is_none(repo: &git2::Repository, submodule: &git2::Submodule<'_>) -> Result<bool> {
    let name = submodule.name().unwrap_or_default();
    Ok(
        match configured(repo, &format!("submodule.{name}.update"))? {
            Some(update) => update == "none",
            None => submodule.update_strategy() == git2::SubmoduleUpdate::None,
        },
    )
}

/// The URL an update clones `submodule` from: the one `init` wrote to the config, which resolves
/// a relative `.gitmodules` URL against the remote.
fn clone_url(repo: &git2::Repository, submodule: &git2::Submodule<'_>) -> Result<String> {
    let name = submodule.name().unwrap_or_default();
    let configured = repo.config()?.get_string(&format!("submodule.{name}.url"));
    Ok(configured
        .ok()
        .or_else(|| submodule.url().map(str::to_owned))
        .unwrap_or_default())
}

/// Whether updating `submodule` has to fetch: it is not cloned yet, or its clone lacks the
/// commit the index records.
fn needs_transport(submodule: &git2::Submodule<'_>) -> bool {
    let Ok(clone) = submodule.open() else {
        return true;
    };
    submodule
        .index_id()
        .is_some_and(|wanted| clone.find_commit(wanted).is_err())
}

/// Whether git would clone or fetch a submodule over the `file` transport. `git submodule` sets
/// `GIT_PROTOCOL_FROM_USER=0`, so the default of `protocol.file.allow` (`user`) refuses it: only
/// `GIT_ALLOW_PROTOCOL` naming `file`, or `protocol.file.allow` (else `protocol.allow`) set to
/// `always`, lets it through.
fn file_transport_allowed(repo: &git2::Repository) -> Result<bool> {
    if let Ok(list) = std::env::var("GIT_ALLOW_PROTOCOL") {
        return Ok(list.split(':').any(|protocol| protocol == "file"));
    }
    let setting = match configured(repo, "protocol.file.allow")? {
        Some(setting) => Some(setting),
        None => configured(repo, "protocol.allow")?,
    };
    Ok(setting.as_deref() == Some("always"))
}

/// Whether a submodule URL uses git's `file` transport: a local path or a `file://` URL.
fn is_file_transport(url: &str) -> bool {
    if url.starts_with("file://") {
        return true;
    }
    let colon = url.find(':');
    let slash = url.find('/');
    !url.contains("://") && (colon.is_none() || slash.is_some_and(|slash| Some(slash) < colon))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;

    use super::{Checkout, Head, link_worktree};
    use crate::core::testing::Sandbox;
    use crate::git::Repo;
    use crate::git::fixtures::commit;

    #[test]
    fn worktree_and_config_calls_leave_the_index_alone() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        commit(&sandbox, &dir, &[("a.txt", "a\n")], "one");
        sandbox.git(&dir, &["config", "index.sparse", "true"]);
        fs::write(dir.join(".git/index"), b"not an index").unwrap();
        let repo = Repo::at(&dir);
        let linked = sandbox.path().join("linked");

        repo.set_config_bool("extensions.worktreeConfig", true)
            .unwrap();
        repo.check_config().unwrap();
        repo.add_worktree(&linked, &Checkout::Detached("HEAD"))
            .unwrap();
        assert!(linked.join("a.txt").is_file());
        repo.remove_worktree(&linked).unwrap();
        assert!(!linked.exists());
    }

    #[test]
    fn a_branch_that_a_checkout_holds_is_refused_and_leaves_nothing_behind() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        commit(&sandbox, &dir, &[("a.txt", "a\n")], "one");
        let first = sandbox.path().join("first");
        let second = sandbox.path().join("second");
        sandbox.git(&dir, &["branch", "topic"]);
        let repo = Repo::at(&dir);

        repo.add_worktree(&first, &Checkout::Branch("topic"))
            .unwrap();
        let refused = repo.add_worktree(&second, &Checkout::Branch("topic"));

        assert!(refused.unwrap_err().0.contains("already checked out"));
        assert!(!second.exists());
        let oracle = sandbox
            .command("git")
            .current_dir(&dir)
            .args(["worktree", "add", "-q"])
            .arg(&second)
            .arg("topic")
            .output()
            .unwrap();
        assert!(!oracle.status.success());
        assert_eq!(repo.worktrees().unwrap().len(), 2);
    }

    /// A repository with a linked worktree `linked` on a new branch, and the worktree's path.
    fn with_linked(sandbox: &Sandbox) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = sandbox.repo();
        commit(sandbox, &dir, &[("a.txt", "a\n")], "one");
        let linked = sandbox.path().join("linked");
        Repo::at(&dir)
            .add_worktree(
                &linked,
                &Checkout::NewBranch {
                    name: "feat",
                    at: "HEAD",
                },
            )
            .unwrap();
        (dir, linked)
    }

    #[test]
    fn a_locked_worktree_is_not_removed_as_git_refuses() {
        let sandbox = Sandbox::with_repo();
        let (dir, linked) = with_linked(&sandbox);
        sandbox.git(
            &dir,
            &[
                "worktree",
                "lock",
                "--reason",
                "on a usb disk",
                linked.to_str().unwrap(),
            ],
        );
        let oracle = sandbox
            .command("git")
            .args(["worktree", "remove", "--force", linked.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(!oracle.status.success(), "git removed a locked worktree");

        let refused = Repo::at(&dir).remove_worktree(&linked).unwrap_err();

        assert!(refused.0.contains("locked"), "{refused}");
        assert!(refused.0.contains("on a usb disk"), "{refused}");
        assert!(linked.join("a.txt").is_file());
        assert_eq!(Repo::at(&dir).worktrees().unwrap().len(), 2);
    }

    #[test]
    fn a_worktree_nested_deeper_than_libgit2_deletes_is_removed_whole() {
        let sandbox = Sandbox::with_repo();
        let (dir, linked) = with_linked(&sandbox);
        let deep = (0..150).fold(linked.clone(), |path, _| path.join("d"));
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("leaf.txt"), "leaf\n").unwrap();

        Repo::at(&dir).remove_worktree(&linked).unwrap();

        assert!(!linked.exists());
        assert_eq!(Repo::at(&dir).worktrees().unwrap().len(), 1);
    }

    #[test]
    fn checking_out_a_path_nested_deeper_than_libgit2_walks_replaces_the_file() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        let deep = (1..=110)
            .map(|level| format!("d{level}"))
            .collect::<Vec<_>>()
            .join("/");
        let (file, script, link) = (
            format!("{deep}/a.sh"),
            format!("{deep}/b.txt"),
            format!("{deep}/link"),
        );
        commit(
            &sandbox,
            &dir,
            &[(&file, "#!/bin/sh\n"), (&script, "b\n")],
            "one",
        );
        std::os::unix::fs::symlink("b.txt", dir.join(&link)).unwrap();
        sandbox.git(&dir, &["add", "-A"]);
        sandbox.git(&dir, &["update-index", "--chmod=+x", &file]);
        sandbox.git(&dir, &["commit", "-q", "-m", "link"]);
        fs::write(dir.join(&file), "changed\n").unwrap();
        fs::set_permissions(dir.join(&file), fs::Permissions::from_mode(0o644)).unwrap();
        fs::remove_file(dir.join(&script)).unwrap();
        fs::remove_file(dir.join(&link)).unwrap();
        fs::write(dir.join(&link), "not a link\n").unwrap();
        let paths = [file.clone(), script.clone(), link.clone()].map(String::into_bytes);

        Repo::at(&dir).checkout_paths("HEAD", &paths).unwrap();

        assert_eq!(fs::read_to_string(dir.join(&file)).unwrap(), "#!/bin/sh\n");
        assert_ne!(fs::metadata(dir.join(&file)).unwrap().mode() & 0o111, 0);
        assert_eq!(fs::read_to_string(dir.join(&script)).unwrap(), "b\n");
        assert_eq!(fs::read_link(dir.join(&link)).unwrap(), Path::new("b.txt"));
        assert_eq!(sandbox.git_output(&dir, &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_worktree_that_lost_its_git_file_is_refused_and_kept() {
        let sandbox = Sandbox::with_repo();
        let (dir, linked) = with_linked(&sandbox);
        fs::remove_file(linked.join(".git")).unwrap();
        fs::write(linked.join("kept.txt"), "x\n").unwrap();
        let oracle = sandbox
            .command("git")
            .args(["worktree", "remove", "--force", linked.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(!oracle.status.success(), "git removed it");

        let refused = Repo::at(&dir).remove_worktree(&linked).unwrap_err();

        assert!(refused.0.contains("does not exist"), "{refused}");
        assert_eq!(fs::read_to_string(linked.join("kept.txt")).unwrap(), "x\n");
        assert_eq!(Repo::at(&dir).worktrees().unwrap().len(), 2);
        assert_eq!(
            sandbox
                .git_output(&dir, &["worktree", "list"])
                .lines()
                .count(),
            2
        );
    }

    #[test]
    fn a_failed_link_removes_only_the_directories_it_created() {
        let sandbox = Sandbox::with_repo();
        let admin = sandbox.path().join("repo/.git/worktrees/feat");
        let taken = sandbox.path().join("taken");
        let head = Head::Branch("refs/heads/feat".to_owned());
        fs::create_dir_all(&taken).unwrap();
        fs::write(taken.join("work.txt"), "winner\n").unwrap();

        let error = link_worktree(&admin, &taken, &head).unwrap_err();

        assert!(error.0.contains("create"), "{error}");
        assert_eq!(
            fs::read_to_string(taken.join("work.txt")).unwrap(),
            "winner\n"
        );
        assert!(
            !admin.exists(),
            "this call's admin directory was left behind"
        );

        fs::create_dir_all(&admin).unwrap();
        fs::write(admin.join("HEAD"), "ref: refs/heads/winner\n").unwrap();
        let fresh = sandbox.path().join("fresh");

        link_worktree(&admin, &fresh, &head).unwrap_err();

        assert!(
            admin.join("HEAD").is_file(),
            "another call's admin directory was removed"
        );
        assert!(!fresh.exists());
    }
}
