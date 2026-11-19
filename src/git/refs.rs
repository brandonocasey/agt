//! Refs and the commits they name: listing, filtering, moving and deleting, through libgit2.

use super::identity::{Role, signature};
use super::{Repo, commit_id, peeled_commit, subject_of};
use crate::core::error::{Error, Result, fail};

/// A ref, with the object it holds and the commit that object leads to.
/// A tag of a blob or tree has no commit.
pub struct RefTip {
    pub name: String,
    pub object: String,
    pub commit: Option<String>,
}

/// A local branch with what a pull request listing shows.
pub struct BranchRow {
    pub name: String,
    pub tip: String,
    pub author: String,
    pub subject: String,
}

/// Which local branches [`Repo::branch_rows`] lists, by their tips against `commit`.
pub enum BranchFilter<'a> {
    All,
    /// Branches whose tip is `commit` or one of its ancestors.
    Merged(&'a str),
    /// Branches whose tip is neither.
    NotMerged(&'a str),
    /// Branches that have `commit` in their history.
    Containing(&'a str),
}

impl Repo {
    /// Every ref under one of `prefixes` (all refs when empty), sorted by name.
    pub fn ref_tips(&self, prefixes: &[&str]) -> Result<Vec<RefTip>> {
        let repo = self.open_refs()?;
        ref_tips_in(&repo, prefixes)
    }

    /// The short names of the refs under `prefix` whose commit is, or has in its history,
    /// `commit` (`git for-each-ref --contains`).
    pub fn refs_containing(&self, commit: &str, prefix: &str) -> Result<Vec<String>> {
        let repo = self.open_refs()?;
        let wanted = commit_id(&repo, commit)?;
        let mut names = Vec::new();
        for tip in ref_tips_in(&repo, &[prefix])? {
            let Some(found) = tip.commit.as_deref() else {
                continue;
            };
            let found = git2::Oid::from_str(found)?;
            if found == wanted || repo.graph_descendant_of(found, wanted)? {
                names.push(short_ref_name(&tip.name).to_owned());
            }
        }
        Ok(names)
    }

    /// The local branches and their tips.
    pub fn branches(&self) -> Result<Vec<(String, String)>> {
        let repo = self.open_refs()?;
        branches_in(&repo)
    }

    /// The local branches `filter` picks, newest commit first.
    pub fn branch_rows(&self, filter: &BranchFilter<'_>) -> Result<Vec<BranchRow>> {
        let repo = self.open_refs()?;
        let mut rows = Vec::new();
        for (name, tip) in branches_in(&repo)? {
            let id = git2::Oid::from_str(&tip)?;
            let picked = match filter {
                BranchFilter::All => true,
                BranchFilter::Merged(commit) => is_reachable(&repo, id, commit_id(&repo, commit)?),
                BranchFilter::NotMerged(commit) => {
                    !is_reachable(&repo, id, commit_id(&repo, commit)?)
                }
                BranchFilter::Containing(commit) => {
                    is_reachable(&repo, commit_id(&repo, commit)?, id)
                }
            };
            if !picked {
                continue;
            }
            let commit = repo.find_commit(id)?;
            let author = String::from_utf8_lossy(commit.author().name_bytes()).into_owned();
            let when = commit.committer().when().seconds();
            rows.push((
                when,
                BranchRow {
                    name,
                    tip,
                    author,
                    subject: subject_of(commit.message_bytes()),
                },
            ));
        }
        rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        Ok(rows.into_iter().map(|(_, row)| row).collect())
    }

    /// The commits reachable from `roots` and from none of `hidden`, in no particular order.
    pub fn commits_beyond(&self, roots: &[&str], hidden: &[String]) -> Result<Vec<String>> {
        let repo = self.open_refs()?;
        let mut walk = repo.revwalk()?;
        for root in roots {
            walk.push(commit_id(&repo, root)?)?;
        }
        for other in hidden {
            walk.hide(commit_id(&repo, other)?)?;
        }
        let mut found = Vec::new();
        for id in walk {
            found.push(id?.to_string());
        }
        Ok(found)
    }

    /// The ids of `objects` that the repository does not have.
    pub fn missing_objects(&self, objects: &[String]) -> Result<Vec<String>> {
        let repo = self.open_refs()?;
        let odb = repo.odb()?;
        Ok(objects
            .iter()
            .filter(|object| !git2::Oid::from_str(object).is_ok_and(|id| odb.exists(id)))
            .cloned()
            .collect())
    }

    /// The committer time of `rev`, in seconds since the epoch.
    pub fn commit_time(&self, rev: &str) -> Result<i64> {
        let repo = self.open_refs()?;
        super::reject_bad_config(&repo)?;
        let id = commit_id(&repo, rev)?;
        Ok(repo.find_commit(id)?.committer().when().seconds())
    }

    /// The full message of every commit in `from..to`, newest first.
    pub fn messages(&self, from: &str, to: &str) -> Result<Vec<String>> {
        let repo = self.open_refs()?;
        let mut messages = Vec::new();
        for id in self.walk(&repo, Some(from), to)? {
            let commit = repo.find_commit(id?)?;
            messages.push(String::from_utf8_lossy(commit.message_bytes()).into_owned());
        }
        Ok(messages)
    }

    /// Points ref `name` at `commit`, creating it. `log` is the reflog message.
    pub fn set_ref(&self, name: &str, commit: &str, log: &str) -> Result<()> {
        let repo = self.open_refs()?;
        repo.reference(name, commit_id(&repo, commit)?, true, log)?;
        Ok(())
    }

    /// Moves ref `name` from `from` to `to`, and fails when it is no longer at `from`.
    pub fn move_ref(&self, name: &str, from: &str, to: &str, log: &str) -> Result<()> {
        let repo = self.open_refs()?;
        let (from, to) = (commit_id(&repo, from)?, commit_id(&repo, to)?);
        repo.reference_matching(name, to, true, from, log)?;
        Ok(())
    }

    /// Deletes ref `name`; a ref that is already gone is not an error.
    pub fn delete_ref(&self, name: &str) -> Result<()> {
        let repo = self.open_refs()?;
        match repo.find_reference(name) {
            Ok(mut reference) => Ok(reference.delete()?),
            Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Deletes local branch `name`: when `force` is false only if it is merged into HEAD or
    /// its upstream, as `git branch -d`.
    pub fn delete_branch(&self, name: &str, force: bool) -> Result<()> {
        {
            let repo = self.open_refs()?;
            local_branch(&repo, name)?;
        }
        if let Some(holder) = self.holders(name)?.first() {
            return fail(format!(
                "cannot delete branch '{name}' used by worktree at '{}'",
                holder.display()
            ));
        }
        let repo = self.open_refs()?;
        let mut branch = local_branch(&repo, name)?;
        if !force && !branch_is_merged(&repo, &branch)? {
            return fail(format!(
                "the branch '{name}' is not fully merged. If you are sure you want to delete it, run 'git branch -D {name}'"
            ));
        }
        branch.delete()?;
        Ok(())
    }

    /// Writes a commit of `tree` on top of `parent`, returning its id; no ref moves.
    /// The author and committer are `agt <agt@localhost>` unless `GIT_AUTHOR_*` or
    /// `GIT_COMMITTER_*` name another.
    pub fn commit_tree(&self, tree: &str, parent: &str, message: &str) -> Result<String> {
        let repo = self.open_refs()?;
        let tree = repo.find_tree(git2::Oid::from_str(tree)?)?;
        let parent = repo.find_commit(commit_id(&repo, parent)?)?;
        let author = signature(Role::Author)?;
        let committer = signature(Role::Committer)?;
        let message = format!("{}\n", message.trim_end_matches('\n'));
        let id = repo.commit(None, &author, &committer, &message, &tree, &[&parent])?;
        Ok(id.to_string())
    }

    /// Whether the ignore rules of the repository ignore `path`, a path below the work tree.
    pub fn is_ignored(&self, path: &str) -> Result<bool> {
        Ok(self.open()?.is_path_ignored(path)?)
    }

    /// Whether `name` is a valid full ref name.
    pub fn is_valid_ref_name(name: &str) -> bool {
        git2::Reference::is_valid_name(name)
    }
}

fn ref_tips_in(repo: &git2::Repository, prefixes: &[&str]) -> Result<Vec<RefTip>> {
    let mut tips = Vec::new();
    for reference in repo.references()? {
        let reference = reference?;
        let Some(name) = reference.name().map(str::to_owned) else {
            continue;
        };
        if !prefixes.is_empty() && !prefixes.iter().any(|prefix| name.starts_with(prefix)) {
            continue;
        }
        let Ok(resolved) = reference.resolve() else {
            continue;
        };
        let Some(object) = resolved.target() else {
            continue;
        };
        tips.push(RefTip {
            name,
            object: object.to_string(),
            commit: resolved
                .peel_to_commit()
                .ok()
                .map(|commit| commit.id().to_string()),
        });
    }
    tips.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(tips)
}

fn branches_in(repo: &git2::Repository) -> Result<Vec<(String, String)>> {
    Ok(ref_tips_in(repo, &["refs/heads/"])?
        .into_iter()
        .filter_map(|tip| {
            let name = tip.name.strip_prefix("refs/heads/")?.to_owned();
            Some((name, tip.object))
        })
        .collect())
}

fn local_branch<'r>(repo: &'r git2::Repository, name: &str) -> Result<git2::Branch<'r>> {
    repo.find_branch(name, git2::BranchType::Local)
        .map_err(|_| Error(format!("branch '{name}' not found")))
}

/// `ref` as `git for-each-ref --format=%(refname:short)` prints it.
fn short_ref_name(name: &str) -> &str {
    ["refs/heads/", "refs/remotes/", "refs/tags/"]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
        .unwrap_or(name)
}

/// Whether `ancestor` is `descendant` or in its history.
fn is_reachable(repo: &git2::Repository, ancestor: git2::Oid, descendant: git2::Oid) -> bool {
    ancestor == descendant
        || repo
            .graph_descendant_of(descendant, ancestor)
            .unwrap_or(false)
}

/// Whether the branch is merged into its upstream, or HEAD without one.
fn branch_is_merged(repo: &git2::Repository, branch: &git2::Branch<'_>) -> Result<bool> {
    let tip = branch
        .get()
        .peel_to_commit()
        .map_err(|error| Error(format!("read the branch tip: {}", error.message())))?
        .id();
    let target = match branch.upstream() {
        Ok(upstream) => upstream
            .get()
            .peel_to_commit()
            .ok()
            .map(|commit| commit.id()),
        Err(_) => peeled_commit(repo, "HEAD")?.and_then(|head| git2::Oid::from_str(&head).ok()),
    };
    Ok(target.is_some_and(|target| is_reachable(repo, tip, target)))
}
