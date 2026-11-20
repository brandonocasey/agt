//! Refs and the commits they name: listing, filtering, moving and deleting, through libgit2.

use std::collections::HashSet;

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
        let mut tips = Vec::new();
        for tip in ref_tips_in(&repo, &[prefix])? {
            if let Some(found) = tip.commit.as_deref() {
                tips.push((tip.name, git2::Oid::from_str(found)?));
            }
        }
        let ids: Vec<git2::Oid> = tips.iter().map(|(_, id)| *id).collect();
        let containing = commits_containing(&repo, wanted, &ids)?;
        Ok(tips
            .iter()
            .filter(|(_, id)| containing.contains(id))
            .map(|(name, _)| short_ref_name(&repo, name))
            .collect())
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

    /// Deletes local branch `name`, as `git branch -d` does (`git branch -D` when `force`). A
    /// work tree that has it checked out, or is rebasing or bisecting it, refuses either.
    /// Without `force` it must also be merged into its upstream, or into HEAD when it has none.
    pub fn delete_branch(&self, name: &str, force: bool) -> Result<()> {
        {
            let repo = self.open_refs()?;
            local_branch(&repo, name)?;
        }
        if let Some(holder) = self.checkout_blocker(name)? {
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

/// `wanted` and the commits among those reachable from `tips` that have it in their history,
/// found in one walk that leaves out the history of `wanted`.
fn commits_containing(
    repo: &git2::Repository,
    wanted: git2::Oid,
    tips: &[git2::Oid],
) -> Result<HashSet<git2::Oid>> {
    let mut found = HashSet::from([wanted]);
    if tips.is_empty() {
        return Ok(found);
    }
    let mut walk = repo.revwalk()?;
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
    for tip in tips {
        walk.push(*tip)?;
    }
    walk.hide(wanted)?;
    for id in walk {
        let id = id?;
        if repo
            .find_commit(id)?
            .parent_ids()
            .any(|parent| found.contains(&parent))
        {
            found.insert(id);
        }
    }
    Ok(found)
}

/// The ways `git rev-parse` reads a short name, as a prefix and a suffix around it.
const SHORT_NAME_RULES: [(&str, &str); 6] = [
    ("", ""),
    ("refs/", ""),
    ("refs/tags/", ""),
    ("refs/heads/", ""),
    ("refs/remotes/", ""),
    ("refs/remotes/", "/HEAD"),
];

/// `name` as `git for-each-ref --format=%(refname:short)` prints it: the shortest form that no
/// other ref also answers to, else the full name.
fn short_ref_name(repo: &git2::Repository, name: &str) -> String {
    for (rule, (prefix, suffix)) in SHORT_NAME_RULES.iter().enumerate().skip(1).rev() {
        let Some(short) = name
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(suffix))
            .filter(|short| !short.is_empty() && (suffix.is_empty() || !short.contains('/')))
        else {
            continue;
        };
        let ambiguous = SHORT_NAME_RULES
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != rule)
            .any(|(_, (prefix, suffix))| {
                repo.find_reference(&format!("{prefix}{short}{suffix}"))
                    .is_ok()
            });
        if !ambiguous {
            return short.to_owned();
        }
    }
    name.to_owned()
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::super::fixtures::{commit, rev};
    use crate::core::testing::Sandbox;
    use crate::git::Repo;

    /// What `git for-each-ref --contains` prints for `commit` under `prefix`.
    fn oracle(sandbox: &Sandbox, dir: &Path, commit: &str, prefix: &str) -> Vec<String> {
        let output = sandbox.git_output(
            dir,
            &[
                "for-each-ref",
                "--contains",
                commit,
                "--format=%(refname:short)",
                prefix,
            ],
        );
        output.lines().map(str::to_owned).collect()
    }

    #[test]
    fn refs_containing_names_what_git_for_each_ref_contains_does() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        commit(&sandbox, &dir, &[("a.txt", "1\n")], "c1");
        commit(&sandbox, &dir, &[("a.txt", "2\n")], "c2");
        sandbox.git(&dir, &["branch", "side"]);
        commit(&sandbox, &dir, &[("b.txt", "3\n")], "c3");
        sandbox.git(&dir, &["switch", "-q", "side"]);
        commit(&sandbox, &dir, &[("c.txt", "s\n")], "s1");
        sandbox.git(&dir, &["switch", "-q", "-"]);
        sandbox.git(&dir, &["merge", "-q", "--no-ff", "side", "-m", "merge"]);
        sandbox.git(&dir, &["switch", "-q", "--orphan", "lone"]);
        commit(&sandbox, &dir, &[("z.txt", "z\n")], "lone");
        sandbox.git(&dir, &["switch", "-q", "-"]);
        // Names that two kinds of ref share, so a short name alone would be ambiguous.
        sandbox.git(&dir, &["branch", "dup", "HEAD~1"]);
        sandbox.git(&dir, &["tag", "dup", "HEAD~2"]);
        sandbox.git(&dir, &["tag", "-a", "-m", "release", "v1", "HEAD~1"]);
        sandbox.git(&dir, &["branch", "origin", "HEAD"]);
        sandbox.git(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        sandbox.git(&dir, &["update-ref", "refs/remotes/origin/side", "side"]);
        sandbox.git(&dir, &["update-ref", "refs/remotes/origin/dup", "dup"]);
        sandbox.git(
            &dir,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        let repo = Repo::at(&dir);

        let commits = sandbox.git_output(&dir, &["rev-list", "--all"]);
        assert!(commits.lines().count() >= 6, "{commits}");
        for commit in commits.lines() {
            for prefix in ["refs/heads/", "refs/remotes/", "refs/tags/"] {
                let mut ours = repo.refs_containing(commit, prefix).unwrap();
                let mut expected = oracle(&sandbox, &dir, commit, prefix);
                ours.sort();
                expected.sort();
                assert_eq!(ours, expected, "{commit} under {prefix}");
            }
        }
        let tip = rev(&sandbox, &dir, "HEAD");
        let remotes = repo.refs_containing(&tip, "refs/remotes/").unwrap();
        assert!(remotes.contains(&"origin/HEAD".to_owned()), "{remotes:?}");
        let heads = repo.refs_containing(&tip, "refs/heads/").unwrap();
        assert!(heads.contains(&"heads/origin".to_owned()), "{heads:?}");
        let old = repo.refs_containing("HEAD~1", "refs/heads/").unwrap();
        assert!(old.contains(&"heads/dup".to_owned()), "{old:?}");
    }

    #[test]
    fn a_branch_that_a_worktree_is_bisecting_is_not_deleted() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        for name in ["one", "two", "three", "four"] {
            commit(&sandbox, &dir, &[("a.txt", name)], name);
        }
        sandbox.git(&dir, &["branch", "topic"]);
        let linked = sandbox.path().join("bisecting");
        sandbox.git(
            &dir,
            &["worktree", "add", "-q", linked.to_str().unwrap(), "topic"],
        );
        sandbox.git(&linked, &["bisect", "start", "topic", "topic~3"]);
        let repo = Repo::at(&dir);
        let oracle = sandbox
            .command("git")
            .args(["branch", "-D", "topic"])
            .output()
            .unwrap();
        assert!(!oracle.status.success(), "git deleted a bisected branch");

        for force in [false, true] {
            let refused = repo.delete_branch("topic", force).unwrap_err().to_string();
            assert!(refused.contains("used by worktree"), "{refused}");
        }
        assert!(
            repo.branches()
                .unwrap()
                .iter()
                .any(|(name, _)| name == "topic")
        );

        sandbox.git(&linked, &["bisect", "reset"]);
        sandbox.git(&linked, &["switch", "-q", "--detach"]);
        repo.delete_branch("topic", true).unwrap();
    }
}
