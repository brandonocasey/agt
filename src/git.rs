use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::ops::Deref;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

pub mod base;
mod env_config;
pub mod fast_forward;
mod files;
#[cfg(test)]
mod fixtures;
mod history;
mod hooks;
mod identity;
mod index;
mod known_hosts;
mod linked;
mod merge;
mod network;
pub use network::disable_credential_prompts;
mod patch_id;
mod rebase;
mod refs;
mod remotes;
mod sparse;
mod sparse_index;
mod worktrees;

use crate::core::error::{Error, Result, ctx, fail};

pub use index::SubmoduleDirt;
pub use linked::{Checkout, Transports};
pub use merge::Busy;
pub use rebase::Rebased;
pub use refs::{BranchFilter, RefTip};

pub struct CommitInfo {
    pub sha: String,
    pub tree: String,
    pub subject: String,
    pub patch_id: Option<String>,
}

pub struct LogEntry {
    pub sha: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub subject: String,
}

/// One entry of `git status --porcelain`: the index and work tree columns, and the path.
pub struct StatusEntry {
    pub staged: u8,
    pub unstaged: u8,
    pub path: String,
}

impl StatusEntry {
    pub fn untracked(&self) -> bool {
        self.staged == b'?'
    }
}

/// A submodule recorded in the index: the commit it is at, and its path.
pub struct Gitlink {
    pub commit: String,
    pub path: String,
}

/// One entry of `git worktree list`.
pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub locked: bool,
    pub bare: bool,
    pub prunable: bool,
}

impl Worktree {
    /// Whether its branch has no commit yet, so `head` is all zeros.
    pub fn unborn(&self) -> bool {
        self.head.chars().all(|c| c == '0')
    }
}

/// Which repository a [`Repo`] opens from its path.
#[derive(Clone, Copy)]
enum Kind {
    /// A directory in a work tree, found the way git finds it: by looking upward.
    WorkTree,
    /// A git directory taken as it is, whatever its `core.worktree` says.
    GitDir,
}

pub struct Repo {
    root: PathBuf,
    kind: Kind,
    main_checkout: OnceLock<PathBuf>,
    // Nothing in one agt process moves the common dir, so it is read from git once per `Repo`;
    // a failed read is not kept.
    common_dir: OnceLock<String>,
    // Answers about refs, HEAD and config, kept when [`Repo::remembering_refs`] asked for it.
    refs: Option<Mutex<HashMap<String, Option<String>>>>,
    // The one open repository of a [`Repo::read_only`] one, whose object writes stay in memory.
    held: Option<Mutex<git2::Repository>>,
}

/// An open repository: a new one, or the lock on the held one of a read-only [`Repo`].
enum Handle<'a> {
    Fresh(git2::Repository),
    Held(MutexGuard<'a, git2::Repository>),
}

impl Deref for Handle<'_> {
    type Target = git2::Repository;

    fn deref(&self) -> &git2::Repository {
        match self {
            Handle::Fresh(repo) => repo,
            Handle::Held(repo) => repo,
        }
    }
}

impl Repo {
    /// The repository whose work tree is `root`, which MUST be a directory git accepts; it need
    /// not be the current directory.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self::of(root.into(), Kind::WorkTree)
    }

    /// The repository whose git directory is `git_dir`, even when its `core.worktree` names a
    /// directory that is gone. It has no work tree, so only refs, objects and config can be read.
    pub fn at_git_dir(git_dir: impl Into<PathBuf>) -> Self {
        Self::of(git_dir.into(), Kind::GitDir)
    }

    fn of(root: PathBuf, kind: Kind) -> Self {
        Self {
            root,
            kind,
            main_checkout: OnceLock::new(),
            common_dir: OnceLock::new(),
            refs: None,
            held: None,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn open_raw(&self) -> std::result::Result<git2::Repository, git2::Error> {
        match self.kind {
            Kind::WorkTree => git2::Repository::open_ext(
                &self.root,
                git2::RepositoryOpenFlags::FROM_ENV,
                std::iter::empty::<&OsStr>(),
            ),
            Kind::GitDir => git2::Repository::open_bare(&self.root),
        }
    }

    fn open_new(&self) -> Result<git2::Repository> {
        let repo = self.open_raw().map_err(|error| {
            Error(format!(
                "open the git repository at {}: {}",
                self.root.display(),
                crate::core::error::libgit2_message(&error)
            ))
        })?;
        Ok(repo)
    }

    /// The repository for a call that reads or changes only refs, objects, config or remotes:
    /// its index is not looked at.
    fn open_refs(&self) -> Result<Handle<'_>> {
        match &self.held {
            Some(held) => Ok(Handle::Held(
                held.lock().unwrap_or_else(PoisonError::into_inner),
            )),
            None => self.open_new().map(Handle::Fresh),
        }
    }

    /// The repository for a call that may read the index, which is expanded in memory when it
    /// is a sparse one.
    fn open(&self) -> Result<Handle<'_>> {
        let handle = self.open_refs()?;
        sparse_index::expand_if_sparse(&handle)?;
        Ok(handle)
    }

    /// The repository for a call that makes libgit2 change the index. A sparse index file is
    /// rewritten as a full one first, which a read-only `Repo` never does.
    fn open_writing(&self) -> Result<Handle<'_>> {
        if self.held.is_some() {
            return self.open();
        }
        let handle = self.open_refs()?;
        sparse_index::expand_in_place(&handle)?;
        Ok(handle)
    }

    /// The absolute git directory of this work tree (a linked worktree's own, not the common one).
    pub fn git_dir(&self) -> Result<PathBuf> {
        Ok(without_trailing_slash(self.open_refs()?.path()))
    }

    /// The root of the work tree that contains this directory.
    pub fn toplevel(&self) -> Result<PathBuf> {
        self.find_toplevel()?
            .ok_or_else(|| Error("this operation must be run in a work tree".into()))
    }

    pub fn discover() -> Result<Self> {
        let top = Self::at(".")
            .find_toplevel()?
            .ok_or_else(|| Error("not inside a git work tree".into()))?;
        Ok(Self::at(top))
    }

    /// The work tree root that contains this directory, or `None` outside any work tree, in a
    /// bare repository or inside a git directory. Other failures return an error.
    pub fn find_toplevel(&self) -> Result<Option<PathBuf>> {
        let repo = match self.open_raw() {
            Ok(repo) => repo,
            Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(workdir) = repo.workdir() else {
            return Ok(None);
        };
        // `git rev-parse --show-toplevel` has no work tree to name inside the git directory.
        let inside_git_dir = fs::canonicalize(&self.root)
            .is_ok_and(|root| root.starts_with(without_trailing_slash(repo.path())));
        Ok((!inside_git_dir).then(|| without_trailing_slash(workdir)))
    }

    /// Makes this `Repo` remember answers about refs, HEAD and config; failed reads are not kept.
    /// While it lives, callers MUST NOT move a ref, HEAD or config through any handle.
    #[must_use]
    pub fn remembering_refs(mut self) -> Self {
        self.refs = Some(Mutex::new(HashMap::new()));
        self
    }

    /// Keeps object writes in memory while existing objects stay readable, so a read-only
    /// command never modifies the repository.
    pub fn read_only(mut self) -> Result<Self> {
        let repo = self.open_new()?;
        repo.odb()?.add_new_mempack_backend(MEMORY_PRIORITY)?;
        self.held = Some(Mutex::new(repo));
        Ok(self)
    }

    /// The answer to `key`, from the first read of it when this `Repo` remembers answers and
    /// that read was settled. A read that cannot open the repository is not settled.
    fn fact(
        &self,
        key: &str,
        read: impl FnOnce(&git2::Repository) -> Result<Option<String>>,
    ) -> Result<Option<String>> {
        self.remembered(key, || match self.open_refs() {
            Ok(repo) => read(&repo).map(|value| (value, true)),
            Err(_) => Ok((None, false)),
        })
    }

    /// Returns the first kept answer, or else `read`'s. An answer is kept when this `Repo`
    /// remembers refs and it is settled.
    fn remembered(
        &self,
        key: &str,
        read: impl FnOnce() -> Result<(Option<String>, bool)>,
    ) -> Result<Option<String>> {
        let Some(refs) = &self.refs else {
            return Ok(read()?.0);
        };
        let known = refs.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(value) = known.get(key) {
            return Ok(value.clone());
        }
        drop(known);
        let (value, settled) = read()?;
        if settled {
            refs.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key.to_owned(), value.clone());
        }
        Ok(value)
    }

    pub fn head(&self) -> Result<String> {
        self.fact("head", |repo| peeled_commit(repo, "HEAD"))?
            .ok_or_else(|| Error("repository has no commits".into()))
    }

    pub fn commit(&self, rev: &str) -> Result<String> {
        let repo = self.open_refs()?;
        peeled_commit(&repo, rev)?.ok_or_else(|| Error(format!("unknown commit: {rev}")))
    }

    /// The first line of the message of `commit`.
    pub fn subject(&self, commit: &str) -> Result<String> {
        let repo = self.open_refs()?;
        reject_bad_config(&repo)?;
        let found = peeled_object(&repo, commit, git2::ObjectType::Commit)?
            .ok_or_else(|| Error(format!("unknown commit: {commit}")))?;
        Ok(subject_of(
            found.as_commit().map_or(&[][..], |c| c.message_bytes()),
        ))
    }

    pub fn parent(&self, commit: &str) -> Result<Option<String>> {
        let repo = self.open_refs()?;
        let Some(found) = peeled_object(&repo, commit, git2::ObjectType::Commit)? else {
            return Ok(None);
        };
        let parent = found.as_commit().and_then(|c| c.parent_id(0).ok());
        Ok(parent.map(|id| id.to_string()))
    }

    pub fn tree_of(&self, commit: &str) -> Result<String> {
        let repo = self.open_refs()?;
        peeled_object(&repo, commit, git2::ObjectType::Tree)?
            .map(|tree| tree.id().to_string())
            .ok_or_else(|| Error(format!("unknown tree of {commit}")))
    }

    pub fn current_branch(&self) -> Result<Option<String>> {
        let full = self.fact("head-branch", |repo| symbolic_target(repo, "HEAD"))?;
        Ok(full.map(|full| local_branch_name(&full).to_owned()))
    }

    /// `<unix time> <sha>` of the oldest reflog entry of a local branch, which stays the same
    /// across rebases but changes when the branch is deleted and created again. Reflogs of
    /// branches live in the common dir, so linked worktrees agree. `None` without a reflog.
    pub fn branch_birth(&self, branch: &str) -> Result<Option<String>> {
        let reference = format!("refs/heads/{branch}");
        self.fact(&format!("birth {reference}"), |repo| {
            let Ok(log) = repo.reflog(&reference) else {
                return Ok(None);
            };
            let Some(oldest) = log.len().checked_sub(1).and_then(|last| log.get(last)) else {
                return Ok(None);
            };
            let time = oldest.committer().when().seconds();
            Ok(Some(format!("{time} {}", oldest.id_new())))
        })
    }

    /// The commit `reference` (a full ref name) leads to, or `None` when it does not exist.
    pub fn ref_commit(&self, reference: &str) -> Result<Option<String>> {
        self.fact(&format!("ref {reference}"), |repo| {
            peeled_commit(repo, reference)
        })
    }

    /// The commit local branch `name` is at, or `None` when there is no such branch.
    pub fn branch_tip(&self, name: &str) -> Result<Option<String>> {
        self.ref_commit(&format!("refs/heads/{name}"))
    }

    /// The full name of the ref that the symbolic ref `name` (`HEAD`, `refs/remotes/origin/HEAD`)
    /// names, or `None` when `name` is missing or not symbolic.
    fn symbolic_ref(&self, name: &str) -> Result<Option<String>> {
        self.fact(&format!("symref {name}"), |repo| {
            symbolic_target(repo, name)
        })
    }

    /// Returns the full ref of the default branch: `--default-branch`, then `origin/HEAD`, then
    /// [`Self::default_branch_ref`].
    pub fn default_branch(&self, explicit: Option<&str>) -> Result<String> {
        self.find_default_branch(explicit)?.map_or_else(
            || fail("cannot find the default branch; pass --default-branch <ref>"),
            Ok,
        )
    }

    /// [`Self::default_branch`], with `None` for a repository where none is found, so a caller
    /// can say how to name one itself.
    pub fn find_default_branch(&self, explicit: Option<&str>) -> Result<Option<String>> {
        if let Some(name) = explicit {
            let local = format!("refs/heads/{name}");
            if self.ref_commit(&local)?.is_some() {
                return Ok(Some(local));
            }
            self.commit(name)?;
            return Ok(Some(name.to_owned()));
        }
        if let Some(remote) = self.symbolic_ref("refs/remotes/origin/HEAD")? {
            return Ok(Some(remote));
        }
        Ok(self.default_branch_ref()?.map(|(_, reference)| reference))
    }

    /// The value of the config key `name`, `None` when unset or empty.
    pub fn config_value(&self, name: &str) -> Result<Option<String>> {
        self.fact(&format!("config {name}"), |repo| configured(repo, name))
    }

    /// The names the default branch may have, best first: the branch `origin/HEAD` names,
    /// `main`, `master`, then `init.defaultBranch`.
    pub fn default_branch_names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        if let Some(remote) = self.symbolic_ref("refs/remotes/origin/HEAD")? {
            let short = remote.strip_prefix("refs/remotes/").unwrap_or(&remote);
            names.push(short.strip_prefix("origin/").unwrap_or(short).to_owned());
        }
        names.extend(["main".to_owned(), "master".to_owned()]);
        names.extend(self.config_value("init.defaultBranch")?);
        let mut seen = HashSet::new();
        names.retain(|name| seen.insert(name.clone()));
        Ok(names)
    }

    /// The default branch's name and ref: the first of [`Self::default_branch_names`] that
    /// exists, as a local branch when it does and as `origin/<name>` otherwise.
    pub fn default_branch_ref(&self) -> Result<Option<(String, String)>> {
        for name in self.default_branch_names()? {
            for reference in [
                format!("refs/heads/{name}"),
                format!("refs/remotes/origin/{name}"),
            ] {
                if self.ref_commit(&reference)?.is_some() {
                    return Ok(Some((name, reference)));
                }
            }
        }
        Ok(None)
    }

    /// The first of [`Self::default_branch_names`] that exists as a local branch, from local refs
    /// only.
    pub fn local_default_branch(&self) -> Result<Option<String>> {
        for name in self.default_branch_names()? {
            if self.branch_tip(&name)?.is_some() {
                return Ok(Some(name));
            }
        }
        Ok(None)
    }

    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        self.fact(&format!("merge-base {a}\0{b}"), |repo| {
            let (Some(a), Some(b)) = (resolved(repo, a), resolved(repo, b)) else {
                return Ok(None);
            };
            Ok(repo.merge_base(a, b).ok().map(|base| base.to_string()))
        })
    }

    pub fn root_commit(&self, head: &str) -> Result<String> {
        let key = format!("roots {head}");
        let roots = self
            .remembered(&key, || {
                let repo = self.open_refs()?;
                let mut walk = repo.revwalk()?;
                let start = peeled_commit(&repo, head)?
                    .ok_or_else(|| Error(format!("unknown commit: {head}")))?;
                walk.push(git2::Oid::from_str(&start)?)?;
                let mut roots = Vec::new();
                for id in walk {
                    let id = id?;
                    if repo.find_commit(id)?.parent_count() == 0 {
                        roots.push(id.to_string());
                    }
                }
                Ok((Some(roots.join("\n")), true))
            })?
            .unwrap_or_default();
        let mut roots: Vec<&str> = roots.lines().collect();
        roots.sort_unstable();
        roots
            .first()
            .map(|root| (*root).to_owned())
            .ok_or_else(|| Error(format!("no root commit for {head}")))
    }

    /// Missing objects count as "not an ancestor", since stored commits may have been collected.
    pub fn is_ancestor(&self, ancestor: &str, commit: &str) -> Result<bool> {
        let repo = self.open_refs()?;
        let (Some(ancestor), Some(commit)) = (resolved(&repo, ancestor), resolved(&repo, commit))
        else {
            return Ok(false);
        };
        Ok(ancestor == commit || repo.graph_descendant_of(commit, ancestor).unwrap_or(false))
    }

    /// Commits in `to` that `from` lacks (`git rev-list --count from..to`).
    pub fn distance(&self, from: &str, to: &str) -> Result<u64> {
        let repo = self.open_refs()?;
        let mut walk = self.walk(&repo, Some(from), to)?;
        let mut count = 0;
        for id in &mut walk {
            id?;
            count += 1;
        }
        Ok(count)
    }

    pub fn object_exists(&self, spec: &str) -> Result<bool> {
        Ok(self.open_refs()?.revparse_single(spec).is_ok())
    }

    /// A revision walk over `from..to` (all of `to` when `from` is `None`).
    fn walk<'r>(
        &self,
        repo: &'r git2::Repository,
        from: Option<&str>,
        to: &str,
    ) -> Result<git2::Revwalk<'r>> {
        let mut walk = repo.revwalk()?;
        walk.push(commit_id(repo, to)?)?;
        if let Some(from) = from {
            walk.hide(commit_id(repo, from)?)?;
        }
        Ok(walk)
    }

    /// The commit of each of `revs`, keyed by rev. A rev that names no commit, is empty or holds
    /// a newline is absent.
    pub fn commit_ids<'a>(&self, revs: &[&'a str]) -> Result<HashMap<&'a str, String>> {
        let repo = self.open_refs()?;
        let mut found = HashMap::new();
        for rev in revs
            .iter()
            .filter(|rev| !rev.is_empty() && !rev.contains('\n'))
        {
            if let Some(id) = peeled_commit(&repo, rev)? {
                found.insert(*rev, id);
            }
        }
        Ok(found)
    }

    /// The `candidates` that [`Self::is_ancestor`] says are ancestors of `head`. A name that is
    /// no commit is not an ancestor.
    pub fn ancestors_of<'a>(&self, head: &str, candidates: &[&'a str]) -> Result<HashSet<&'a str>> {
        let mut revs = candidates.to_vec();
        revs.push(head);
        let ids = self.commit_ids(&revs)?;
        let Some(head_id) = ids.get(head) else {
            return Ok(HashSet::new());
        };
        let repo = self.open_refs()?;
        // What a candidate reaches that the head does not: a candidate in that list is not an
        // ancestor of the head.
        let mut walk = repo.revwalk()?;
        let mut pushed = false;
        for id in candidates.iter().filter_map(|candidate| ids.get(candidate)) {
            walk.push(git2::Oid::from_str(id)?)?;
            pushed = true;
        }
        if !pushed {
            return Ok(HashSet::new());
        }
        walk.hide(git2::Oid::from_str(head_id)?)?;
        let outside: HashSet<String> = walk
            .map(|id| id.map(|id| id.to_string()))
            .collect::<std::result::Result<_, _>>()?;
        Ok(candidates
            .iter()
            .copied()
            .filter(|candidate| {
                ids.get(candidate)
                    .is_some_and(|id| !outside.contains(id.as_str()))
            })
            .collect())
    }

    /// The `revs`, in order, that are not an ancestor of another of them. Revs naming one commit
    /// keep the first; revs naming no commit are dropped.
    pub fn independent<'a>(&self, revs: &[&'a str]) -> Result<Vec<&'a str>> {
        let ids = self.commit_ids(revs)?;
        let mut first: Vec<(&'a str, git2::Oid)> = Vec::new();
        for rev in revs {
            if let Some(id) = ids.get(rev) {
                let id = git2::Oid::from_str(id)?;
                if first.iter().all(|(_, seen)| *seen != id) {
                    first.push((rev, id));
                }
            }
        }
        let repo = self.open_refs()?;
        let mut tips = Vec::new();
        for (rev, id) in &first {
            let covered = first.iter().any(|(_, other)| {
                other != id && repo.graph_descendant_of(*other, *id).unwrap_or(false)
            });
            if !covered {
                tips.push(*rev);
            }
        }
        Ok(tips)
    }

    pub fn user_name(&self) -> Result<Option<String>> {
        let repo = self.open_refs()?;
        configured(&repo, "user.name")
    }

    /// The absolute git common dir, which names a `local/` repository.
    pub fn common_dir(&self) -> Result<String> {
        if let Some(dir) = self.common_dir.get() {
            return Ok(dir.clone());
        }
        let dir = without_trailing_slash(self.open_refs()?.commondir())
            .to_string_lossy()
            .into_owned();
        Ok(self.common_dir.get_or_init(|| dir).clone())
    }

    pub fn is_bare(&self) -> Result<bool> {
        let repo = self.open_refs()?;
        Ok(env_config::is_bare(&repo))
    }

    pub fn main_checkout_root(&self) -> Result<PathBuf> {
        if let Some(root) = self.main_checkout.get() {
            return Ok(root.clone());
        }
        if self.is_bare()? {
            return fail("repository-local state requires a non-bare main checkout");
        }
        let first = self
            .worktrees()?
            .into_iter()
            .next()
            .ok_or_else(|| Error("git did not report its main checkout".into()))?;
        let root = fs::canonicalize(first.path).map_err(ctx("main checkout is unavailable"))?;
        let main = Self::at(&root);
        if main.is_bare()?
            || fs::canonicalize(main.common_dir()?).map_err(ctx("resolve main git directory"))?
                != fs::canonicalize(self.common_dir()?).map_err(ctx("resolve git directory"))?
        {
            return fail("main checkout does not belong to this repository");
        }
        let _ = self.main_checkout.set(root.clone());
        Ok(root)
    }

    pub fn repo_key(&self) -> Result<String> {
        if let Some(url) = self.origin_url()?
            && let Some(key) = remote_key(&url)
        {
            return Ok(key);
        }
        let common = self.common_dir()?;
        let path = Path::new(&common);
        let named = if path.file_name().is_some_and(|name| name == ".git") {
            path.parent()
        } else {
            Some(path)
        };
        let name = named
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().trim_end_matches(".git").to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "repo".to_owned());
        let hash = hash_text(&common)?;
        let short = crate::core::text::prefix(&hash, 12);
        Ok(format!("local/{name}-{short}"))
    }
}

/// Fails on a `log.showSignature` that is not a boolean, as `git log` does however it is
/// overridden. The reads that stood for `git log` call it; the other reads never did.
fn reject_bad_config(repo: &git2::Repository) -> Result<()> {
    const KEY: &str = "log.showsignature";
    let config = repo.config()?;
    let Ok(entry) = config.get_entry(KEY) else {
        return Ok(());
    };
    match (entry.value(), config.get_bool(KEY)) {
        (Some(value), Err(_)) => fail(format!("bad boolean config value '{value}' for '{KEY}'")),
        _ => Ok(()),
    }
}

/// The path configured at `key` in the user's global git config (`GIT_CONFIG_GLOBAL`, else the
/// home or XDG config), with `~` expanded; `None` when it is unset.
pub fn global_config_path(key: &str) -> Option<PathBuf> {
    let mut config = git2::Config::new().ok()?;
    let global = std::env::var_os("GIT_CONFIG_GLOBAL")
        .map(PathBuf::from)
        .or_else(|| git2::Config::find_global().ok());
    for (file, level) in [
        (git2::Config::find_xdg().ok(), git2::ConfigLevel::XDG),
        (global, git2::ConfigLevel::Global),
    ] {
        if let Some(file) = file.filter(|file| file.is_file()) {
            config.add_file(&file, level, false).ok()?;
        }
    }
    config.get_path(key).ok()
}

/// The value of `key` in the config file `file`, `None` when it is unset.
pub fn config_file_value(file: &Path, key: &str) -> Result<Option<String>> {
    let config = git2::Config::open(file)?;
    Ok(config.get_string(key).ok())
}

/// The entries of the config file `file` whose names match the regular expression `names`, as
/// name and value.
pub fn config_file_entries(file: &Path, names: &str) -> Result<Vec<(String, String)>> {
    let config = git2::Config::open(file)?;
    let mut found = Vec::new();
    let mut entries = config.entries(Some(names))?;
    while let Some(entry) = entries.next() {
        let entry = entry?;
        if let (Some(name), Some(value)) = (entry.name(), entry.value()) {
            found.push((name.to_owned(), value.to_owned()));
        }
    }
    Ok(found)
}

/// How early the in-memory object store of a read-only repository is asked to take a write.
const MEMORY_PRIORITY: i32 = 1000;

/// A stable id for `value`: the id git gives a blob of it, which stays the same across Rust
/// versions, unlike std hashers. It writes no object.
pub fn hash_text(value: &str) -> Result<String> {
    Ok(git2::Oid::hash_object(git2::ObjectType::Blob, value.as_bytes())?.to_string())
}

/// The object `spec` names, peeled to `kind`; `None` when it does not exist or cannot be one.
fn peeled_object<'r>(
    repo: &'r git2::Repository,
    spec: &str,
    kind: git2::ObjectType,
) -> Result<Option<git2::Object<'r>>> {
    let Ok(found) = repo.revparse_single(spec) else {
        return Ok(None);
    };
    Ok(found.peel(kind).ok())
}

/// The commit `spec` names, peeled through tags; `None` when it names none.
fn peeled_commit(repo: &git2::Repository, spec: &str) -> Result<Option<String>> {
    Ok(peeled_object(repo, spec, git2::ObjectType::Commit)?.map(|commit| commit.id().to_string()))
}

/// The commit `spec` names, or `None`; for callers that only compare commits.
fn resolved(repo: &git2::Repository, spec: &str) -> Option<git2::Oid> {
    peeled_object(repo, spec, git2::ObjectType::Commit)
        .ok()
        .flatten()
        .map(|commit| commit.id())
}

/// The value of the config key `name` for `repo`, the environment's `GIT_CONFIG_*` first;
/// `None` when unset or empty.
fn configured(repo: &git2::Repository, name: &str) -> Result<Option<String>> {
    if let Some(value) = env_config::value(name) {
        return Ok(Some(value).filter(|value| !value.is_empty()));
    }
    let config = repo.config()?;
    Ok(config
        .get_string(name)
        .ok()
        .filter(|value| !value.is_empty()))
}

fn commit_id(repo: &git2::Repository, spec: &str) -> Result<git2::Oid> {
    resolved(repo, spec).ok_or_else(|| Error(format!("unknown commit: {spec}")))
}

/// What the symbolic ref `name` points at; `None` when `name` is missing or holds an id.
fn symbolic_target(repo: &git2::Repository, name: &str) -> Result<Option<String>> {
    Ok(repo
        .find_reference(name)
        .ok()
        .and_then(|found| found.symbolic_target().map(str::to_owned)))
}

/// `path` with `.` and `..` folded away and no symbolic links followed.
fn folded(path: &Path) -> PathBuf {
    let mut folded = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                folded.pop();
            }
            Component::CurDir => {}
            other => folded.push(other),
        }
    }
    folded
}

/// `path` as libgit2 prints a directory, with no trailing slash.
fn without_trailing_slash(path: &Path) -> PathBuf {
    let text = path.as_os_str().as_bytes();
    let trimmed = text.strip_suffix(b"/").filter(|rest| !rest.is_empty());
    PathBuf::from(OsStr::from_bytes(trimmed.unwrap_or(text)))
}

/// The first line of a commit message as `git log --format=%s` prints it: the first paragraph
/// joined on one line.
fn subject_of(message: &[u8]) -> String {
    let message = String::from_utf8_lossy(message);
    message
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .take_while(|line| !line.trim().is_empty())
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Files in a git directory that mark an unfinished rebase, merge, cherry-pick, revert or
/// bisect.
const UNFINISHED: [&str; 8] = [
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "sequencer",
    "BISECT_START",
];

/// The markers of unfinished operations that `git_dir` holds, in the order of [`UNFINISHED`].
pub fn unfinished_markers(git_dir: &Path) -> Vec<&'static str> {
    UNFINISHED
        .into_iter()
        .filter(|marker| git_dir.join(marker).exists())
        .collect()
}

/// Git's stderr as one line, without `error:` and `fatal:` marks and without hints.
/// Progress text that a carriage return overwrites on a terminal is left out too.
pub fn error_line(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(|line| {
            let line = line
                .rsplit_once('\r')
                .map_or(line, |(_, shown)| shown)
                .trim();
            ["error: ", "fatal: "]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
                .unwrap_or(line)
        })
        // Git's hints name git commands, such as `git merge --no-ff`, that a caller here forbids.
        .filter(|line| !line.is_empty() && !line.starts_with("hint:"))
        .collect();
    lines.join(" ")
}

/// The short name of a full branch ref: a local branch or one of `origin`.
pub fn branch_name(reference: &str) -> &str {
    reference
        .strip_prefix("refs/remotes/origin/")
        .or_else(|| reference.strip_prefix("refs/heads/"))
        .unwrap_or(reference)
}

/// The name of a local branch ref; anything else is returned as it is.
pub fn local_branch_name(reference: &str) -> &str {
    reference.strip_prefix("refs/heads/").unwrap_or(reference)
}

/// A git remote whose URL names a repository on a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteLocation {
    pub name: String,
    /// Lowercase.
    pub host: String,
    /// `owner/repo`, or a GitLab project path.
    pub repo: String,
}

/// Splits a remote URL into its lowercase host and its `owner/repo` path (GitLab subgroups kept).
pub fn split_remote(url: &str) -> Option<(String, String)> {
    let (authority, path) = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?,
        None => {
            let (authority, path) = url.split_once(':')?;
            if authority.contains('/') {
                return None;
            }
            (authority, path)
        }
    };
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host.split(':').next().unwrap_or_default();
    if host.is_empty() {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if segments.len() < 2 {
        return None;
    }
    Some((host.to_ascii_lowercase(), segments.join("/")))
}

fn remote_key(url: &str) -> Option<String> {
    split_remote(url).map(|(host, path)| format!("{host}/{path}"))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{Repo, branch_name, remote_key};
    use crate::core::testing::Sandbox;

    /// The repository of `sandbox`; the sandbox hides the developer's global git config from
    /// libgit2.
    fn sandboxed(sandbox: &Sandbox) -> Repo {
        sandboxed_at(sandbox, &sandbox.repo())
    }

    /// Like [`sandboxed`], for the repository at `repo_root`.
    fn sandboxed_at(_sandbox: &Sandbox, repo_root: &Path) -> Repo {
        Repo::at(repo_root)
    }

    #[test]
    fn origin_location_falls_back_to_the_written_url_when_the_expanded_one_names_no_host() {
        let sandbox = Sandbox::new();
        let root = sandbox.repo();
        let repo = sandboxed(&sandbox);
        sandbox.git(&root, &["init", "-q"]);
        assert_eq!(repo.origin_location().unwrap(), None);
        sandbox.git(
            &root,
            &[
                "remote",
                "add",
                "origin",
                "https://GitHub.com/owner/repo.git",
            ],
        );
        let expected = Some(("github.com".to_owned(), "owner/repo".to_owned()));
        assert_eq!(repo.origin_location().unwrap(), expected);
        sandbox.git(
            &root,
            &[
                "config",
                "url./srv/mirror/.insteadOf",
                "https://GitHub.com/",
            ],
        );
        assert_eq!(
            repo.origin_url().unwrap().as_deref(),
            Some("/srv/mirror/owner/repo.git")
        );
        assert_eq!(repo.origin_location().unwrap(), expected);
    }

    #[test]
    fn remote_locations_list_every_remote_that_names_a_repository() {
        let sandbox = Sandbox::new();
        let root = sandbox.repo();
        let repo = sandboxed(&sandbox);
        sandbox.git(&root, &["init", "-q"]);
        assert!(repo.remote_locations().unwrap().is_empty());
        for (name, url) in [
            ("zeta", "git@gitlab.com:team/zeta.git"),
            ("upstream", "https://github.com/up/stream.git"),
            ("local", "/srv/mirror"),
            ("origin", "https://GitHub.com/me/fork.git"),
        ] {
            sandbox.git(&root, &["remote", "add", name, url]);
        }
        let found: Vec<(String, String, String)> = repo
            .remote_locations()
            .unwrap()
            .into_iter()
            .map(|remote| (remote.name, remote.host, remote.repo))
            .collect();
        let expect = |name: &str, host: &str, repo: &str| {
            (name.to_owned(), host.to_owned(), repo.to_owned())
        };
        assert_eq!(
            found,
            [
                expect("origin", "github.com", "me/fork"),
                expect("upstream", "github.com", "up/stream"),
                expect("zeta", "gitlab.com", "team/zeta"),
            ]
        );
    }

    #[test]
    fn current_branch_is_not_confused_by_a_tag_of_the_same_name() {
        let sandbox = Sandbox::new();
        let root = sandbox.repo();
        let repo = sandboxed(&sandbox);
        sandbox.git(&root, &["init", "-q", "-b", "topic"]);
        sandbox.git(&root, &["commit", "-q", "--allow-empty", "-m", "first"]);
        assert_eq!(repo.current_branch().unwrap().as_deref(), Some("topic"));
        sandbox.git(&root, &["tag", "topic"]);
        assert_eq!(repo.current_branch().unwrap().as_deref(), Some("topic"));
        sandbox.git(&root, &["checkout", "-q", "--detach"]);
        assert_eq!(repo.current_branch().unwrap(), None);
    }

    #[test]
    fn a_remembering_repo_does_not_keep_a_read_that_failed() {
        let sandbox = Sandbox::new();
        let root = sandbox.repo();
        let repo = sandboxed(&sandbox).remembering_refs();
        // Outside a repository git exits 128, which says nothing about the branch.
        assert_eq!(repo.current_branch().unwrap(), None);
        assert!(repo.head().is_err());
        sandbox.git(&root, &["init", "-q", "-b", "topic"]);
        sandbox.git(&root, &["commit", "-q", "--allow-empty", "-m", "first"]);
        assert_eq!(repo.current_branch().unwrap().as_deref(), Some("topic"));
        assert!(repo.head().is_ok());
    }

    #[test]
    fn the_default_branch_honours_init_default_branch_everywhere() {
        let sandbox = Sandbox::new();
        let root = sandbox.repo();
        let repo = sandboxed(&sandbox);
        sandbox.git(&root, &["init", "-q", "-b", "trunk"]);
        sandbox.git(&root, &["commit", "-q", "--allow-empty", "-m", "first"]);
        assert_eq!(repo.default_branch_ref().unwrap(), None);
        assert_eq!(repo.find_default_branch(None).unwrap(), None);
        sandbox.git(&root, &["config", "init.defaultBranch", "trunk"]);
        assert_eq!(
            repo.default_branch_ref().unwrap(),
            Some(("trunk".to_owned(), "refs/heads/trunk".to_owned()))
        );
        assert_eq!(
            repo.find_default_branch(None).unwrap().as_deref(),
            Some("refs/heads/trunk")
        );
        assert_eq!(
            repo.default_branch_names().unwrap(),
            ["main", "master", "trunk"]
        );
    }

    #[test]
    fn the_repository_layout_is_read_from_git_once_per_repo() {
        let sandbox = Sandbox::with_repo();
        let root = sandbox.repo();
        let repo = sandboxed(&sandbox);
        let common_dir = repo.common_dir().unwrap();
        assert_eq!(
            fs::canonicalize(&common_dir).unwrap(),
            fs::canonicalize(root.join(".git")).unwrap()
        );
        // With the repository gone, only the kept first answer can return.
        fs::rename(root.join(".git"), root.join("moved")).unwrap();
        assert_eq!(repo.common_dir().unwrap(), common_dir);
        assert!(sandboxed(&sandbox).common_dir().is_err());
    }

    #[test]
    fn each_repo_keeps_the_layout_of_its_own_root() {
        let sandbox = Sandbox::with_repo();
        let other = sandbox.path().join("other");
        fs::create_dir(&other).unwrap();
        sandbox.git(&other, &["init", "-q", "--bare"]);
        let first = sandboxed(&sandbox);
        let second = sandboxed_at(&sandbox, &other);
        for _ in 0..2 {
            let refusal = second.main_checkout_root().unwrap_err();
            assert!(refusal.to_string().contains("non-bare"), "{refusal}");
            assert_eq!(
                fs::canonicalize(first.common_dir().unwrap()).unwrap(),
                fs::canonicalize(sandbox.repo().join(".git")).unwrap()
            );
            assert_eq!(
                fs::canonicalize(second.common_dir().unwrap()).unwrap(),
                fs::canonicalize(&other).unwrap()
            );
        }
    }

    #[test]
    fn a_failed_layout_read_is_not_kept() {
        let sandbox = Sandbox::new();
        let root = sandbox.repo();
        let repo = sandboxed(&sandbox);
        assert!(repo.common_dir().is_err());
        sandbox.git(&root, &["init", "-q"]);
        assert_eq!(
            fs::canonicalize(repo.common_dir().unwrap()).unwrap(),
            fs::canonicalize(root.join(".git")).unwrap()
        );
    }

    /// A repo with `main` (c0-c2), `side` off c0 (d1, d2) and tag `v1` on c1. Also returns revs
    /// to ask about, including a blob and an absent object.
    fn history(sandbox: &Sandbox) -> (Repo, Vec<String>) {
        let root = sandbox.repo();
        let repo = sandboxed(sandbox);
        sandbox.git(&root, &["init", "-q", "-b", "main"]);
        let empty = |message: &str| {
            sandbox.git(&root, &["commit", "-q", "--allow-empty", "-m", message]);
            repo.commit("HEAD").unwrap()
        };
        let c0 = empty("c0");
        sandbox.git(&root, &["switch", "-q", "-c", "side"]);
        let d1 = empty("d1");
        let d2 = empty("d2");
        sandbox.git(&root, &["switch", "-q", "main"]);
        let c1 = empty("c1");
        sandbox.git(&root, &["tag", "-a", "-m", "release", "v1"]);
        let c2 = empty("c2");
        fs::write(root.join("blob.txt"), "blob\n").unwrap();
        let blob = git2::Repository::open(&root)
            .unwrap()
            .blob_path(&root.join("blob.txt"))
            .unwrap()
            .to_string();
        let absent = "deadbeef".repeat(5);
        let revs = [
            c0,
            d1,
            d2,
            c1.clone(),
            c1,
            c2,
            "v1".into(),
            "main~1".into(),
            "HEAD".into(),
            blob,
            absent,
            String::new(),
        ];
        (repo, revs.into())
    }

    #[test]
    fn commit_ids_agree_with_object_exists_for_each_rev() {
        let sandbox = Sandbox::new();
        let (repo, revs) = history(&sandbox);
        let asked: Vec<&str> = revs.iter().map(String::as_str).collect();
        let ids = repo.commit_ids(&asked).unwrap();
        for rev in &asked {
            let exists =
                !rev.is_empty() && repo.object_exists(&format!("{rev}^{{commit}}")).unwrap();
            assert_eq!(ids.contains_key(rev), exists, "{rev:?}");
            if exists {
                assert_eq!(ids[rev], repo.commit(rev).unwrap(), "{rev}");
            }
        }
        assert!(
            ids.len() >= 8,
            "the commits, the tag and the relative names resolve"
        );
        assert!(repo.commit_ids(&[]).unwrap().is_empty());
    }

    #[test]
    fn ancestors_of_agrees_with_is_ancestor_for_each_head_and_candidate() {
        let sandbox = Sandbox::new();
        let (repo, revs) = history(&sandbox);
        let asked: Vec<&str> = revs.iter().map(String::as_str).collect();
        let absent = "deadbeef".repeat(5);
        for head in ["main", "side", revs[0].as_str(), "v1", absent.as_str()] {
            let batch = repo.ancestors_of(head, &asked).unwrap();
            for candidate in &asked {
                assert_eq!(
                    batch.contains(candidate),
                    repo.is_ancestor(candidate, head).unwrap(),
                    "{candidate:?} below {head}"
                );
            }
        }
        assert!(repo.ancestors_of("main", &[]).unwrap().is_empty());
    }

    #[test]
    fn independent_keeps_what_no_other_rev_descends_from() {
        let sandbox = Sandbox::new();
        let (repo, revs) = history(&sandbox);
        let asked: Vec<&str> = revs.iter().map(String::as_str).collect();
        let ids = repo.commit_ids(&asked).unwrap();
        let mut seen = Vec::new();
        let mut expected = Vec::new();
        for rev in &asked {
            let Some(id) = ids.get(rev) else { continue };
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            let covered = asked.iter().any(|other| {
                ids.get(other).is_some_and(|other_id| other_id != id)
                    && repo.is_ancestor(rev, other).unwrap()
            });
            if !covered {
                expected.push(*rev);
            }
        }
        assert_eq!(repo.independent(&asked).unwrap(), expected);
        assert_eq!(expected.len(), 2, "the tips of main and side: {expected:?}");
        assert!(
            repo.independent(&["", &"deadbeef".repeat(5)])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parses_remote_urls() {
        let cases = [
            (
                "git@github.com:owner/repo.git",
                Some("github.com/owner/repo"),
            ),
            (
                "https://GitHub.com/owner/repo",
                Some("github.com/owner/repo"),
            ),
            (
                "ssh://git@gitlab.example.com:2222/group/sub/repo.git",
                Some("gitlab.example.com/group/sub/repo"),
            ),
            ("https://user:token@host.io/o/r.git/", Some("host.io/o/r")),
            ("/srv/git/repo.git", None),
            ("file:///srv/git/repo.git", None),
        ];
        for (url, expected) in cases {
            assert_eq!(remote_key(url).as_deref(), expected, "{url}");
        }
    }

    #[test]
    fn worktrees_list_the_main_checkout_first_then_linked_ones_with_awkward_paths() {
        let sandbox = Sandbox::with_repo();
        let root = sandbox.repo();
        sandbox.git(&root, &["commit", "-q", "--allow-empty", "-m", "first"]);
        let repo = sandboxed(&sandbox);
        let weird = sandbox.path().join("new\nline");
        let locked = sandbox.path().join("locked");
        let gone = sandbox.path().join("gone");
        sandbox.git(
            &root,
            &["worktree", "add", "-q", "--detach", weird.to_str().unwrap()],
        );
        sandbox.git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "keep",
                locked.to_str().unwrap(),
            ],
        );
        sandbox.git(
            &root,
            &[
                "worktree",
                "lock",
                "--reason",
                "why",
                locked.to_str().unwrap(),
            ],
        );
        sandbox.git(
            &root,
            &["worktree", "add", "-q", "--detach", gone.to_str().unwrap()],
        );
        fs::remove_dir_all(&gone).unwrap();
        let found = repo.worktrees().unwrap();
        let by_path = |path: &Path| {
            found
                .iter()
                .find(|entry| entry.path == fs::canonicalize(path).unwrap_or(path.to_owned()))
                .unwrap_or_else(|| panic!("{} is not listed", path.display()))
        };
        assert_eq!(found.len(), 4);
        assert_eq!(found[0].path, fs::canonicalize(&root).unwrap());
        assert_eq!(found[0].branch.as_deref(), Some("main"));
        assert_eq!(found[0].head, repo.head().unwrap());
        assert!(by_path(&weird).branch.is_none() && !by_path(&weird).locked);
        assert_eq!(by_path(&locked).branch.as_deref(), Some("keep"));
        assert!(by_path(&locked).locked && !by_path(&locked).prunable);
        assert!(
            found
                .iter()
                .any(|entry| entry.path.ends_with("gone") && entry.prunable)
        );
    }

    #[test]
    fn branch_name_strips_local_and_origin_prefixes() {
        assert_eq!(branch_name("refs/remotes/origin/main"), "main");
        assert_eq!(branch_name("refs/heads/a/b"), "a/b");
        assert_eq!(branch_name("v1"), "v1");
    }
}
