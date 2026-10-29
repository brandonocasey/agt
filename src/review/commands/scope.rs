use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use crate::core::clock;
use crate::core::error::{Error, Result, fail};
use crate::core::text::prefix;
use crate::git::{CommitInfo, Repo};
use crate::store::model::{Author, CommitPatch, Target, TargetKind, VERSION};
use crate::store::{Namespace, Store};

use super::{Ctx, Scope, Selection, pass_patch_ids, passes};

impl Ctx {
    /// With no `--target` or `--range`, a command naming a run works on the target that holds
    /// it, so callers need not repeat a range or key. The current branch's own target wins.
    pub(super) fn select_run_target(&mut self, run: &str) -> Result<()> {
        if self.selection.target.is_some() || self.selection.range.is_some() {
            return Ok(());
        }
        let has_run = |target: &Target| {
            target.runs.iter().any(|known| known.id == run)
                || target.qa_runs.iter().any(|known| known.id == run)
        };
        let scope = self.scope();
        if let Ok(scope) = &scope
            && scope.existing.as_ref().is_some_and(has_run)
        {
            return Ok(());
        }
        // Only a range, commit or PR/MR target: another branch's target would evaluate that
        // branch's tip, not the checked-out fix.
        let holders: Vec<String> = self
            .store
            .targets(&self.dir)?
            .into_iter()
            .filter(|target| has_run(target))
            .filter(|target| target.kind != TargetKind::Branch || target.review.is_some())
            .map(|target| target.key)
            .collect();
        match (holders.as_slice(), scope) {
            ([key], _) => self.selection.target = Some(key.clone()),
            (_, Ok(_)) => {}
            ([], Err(why)) => {
                return fail(format!(
                    "run {run} is in no range or PR/MR target here, and {why}"
                ));
            }
            (keys, Err(_)) => {
                return fail(format!(
                    "run {run} is in several targets ({}); pass --target <key>",
                    keys.join(", ")
                ));
            }
        }
        Ok(())
    }

    pub(super) fn new(
        selection: Selection,
        read_only: bool,
        scratch_git: bool,
        preview: bool,
    ) -> Result<Self> {
        let mut repo = Repo::discover()?;
        if scratch_git {
            repo = repo.read_only()?;
        }
        let store = Store::open_repo(Namespace::Reviews, &repo, preview)?;
        let repo_key = repo.repo_key()?;
        let dir = store.repo_dir(&repo_key)?;
        if !read_only && repo_key.starts_with("local/") {
            // `prune` checks this path, and how long ago agt last ran here, to tell a repository
            // that is gone.
            store.record_repo(&dir, &repo.common_dir()?, clock::now_secs()?)?;
        }
        Ok(Self {
            repo,
            read_only,
            store,
            repo_key,
            dir,
            selection,
        })
    }

    /// Author default: the forge login cached by `sync`, else git `user.name`, which the next
    /// sync replaces with the forge login.
    pub(super) fn default_author(&self) -> Result<Option<Author>> {
        if let Some(account) = self.store.load_account(&self.dir)? {
            return Ok(Some(Author::Human {
                login: account.login,
                git_name: false,
            }));
        }
        Ok(self.repo.user_name()?.map(|login| Author::Human {
            login,
            git_name: true,
        }))
    }

    pub(super) fn scope(&self) -> Result<Scope> {
        if let Some(key) = &self.selection.target {
            return self.scope_for_key(key);
        }
        if let Some(range) = &self.selection.range {
            return self.scope_for_range(range);
        }
        self.scope_for_branch()
    }

    pub(super) fn scope_for_key(&self, key: &str) -> Result<Scope> {
        let target = self
            .store
            .load(&self.dir, key)?
            .ok_or_else(|| Error(format!("target {key} does not exist")))?;
        let mut headless = false;
        let (base, head) = match target.kind {
            TargetKind::Branch => {
                let branch = target.branch.clone().unwrap_or_default();
                let found = match self.branch_head(&target, &branch)? {
                    Ok(head) => Some(head),
                    // Reading commands still work: anchors map to the last reviewed head, or
                    // nowhere when nothing was reviewed.
                    Err(_) if self.read_only => passes(&target)
                        .max_by(|a, b| a.started_at.cmp(&b.started_at))
                        .map(|pass| pass.head.clone()),
                    Err(why) => {
                        return fail(format!(
                            "branch {branch} of target {key} is not here and {why}"
                        ));
                    }
                };
                match found {
                    Some(head) => (
                        Some(
                            self.branch_base(&branch, &head, target.default_branch.as_deref())?
                                .0,
                        ),
                        head,
                    ),
                    None => {
                        headless = true;
                        (target.base.clone(), String::new())
                    }
                }
            }
            TargetKind::Range | TargetKind::Commit => {
                let head = target
                    .head
                    .clone()
                    .ok_or_else(|| Error(format!("target {key} has no head")))?;
                (target.base.clone(), head)
            }
        };
        Ok(Scope {
            key: key.to_owned(),
            kind: target.kind,
            branch: target.branch.clone(),
            birth: target.branch_birth.clone(),
            base,
            head,
            head_known: !headless,
            default_branch: self
                .selection
                .default_branch
                .clone()
                .or_else(|| target.default_branch.clone()),
            existing: Some(target),
        })
    }

    pub(super) fn scope_for_range(&self, range: &str) -> Result<Scope> {
        let (kind, key, base, head) = match range.split_once("..") {
            Some((_, to)) if to.starts_with('.') => {
                return fail("symmetric ranges (a...b) are not supported");
            }
            Some((from, to)) => {
                let from = self.repo.commit(from)?;
                let to = self.repo.commit(to)?;
                (
                    TargetKind::Range,
                    format!("range-{from}-{to}"),
                    Some(from),
                    to,
                )
            }
            None => {
                let commit = self.repo.commit(range)?;
                let parent = self.repo.parent(&commit)?;
                (
                    TargetKind::Commit,
                    format!("commit-{commit}"),
                    parent,
                    commit,
                )
            }
        };
        let existing = self.store.load(&self.dir, &key)?;
        Ok(Scope {
            key,
            kind,
            branch: None,
            birth: None,
            base,
            head,
            head_known: true,
            default_branch: None,
            existing,
        })
    }

    /// Finds the branch's target: first one with the same reflog birth, then the exact key, then
    /// an older target this branch continues; otherwise a new target.
    pub(super) fn scope_for_branch(&self) -> Result<Scope> {
        let branch = self.repo.current_branch()?.ok_or_else(|| {
            Error("HEAD is detached; pass --range <rev> or --target <key>".into())
        })?;
        let head = self.repo.head()?;
        let birth = self.repo.branch_birth(&branch)?;
        let mut candidates: Vec<Target> = self
            .store
            .targets(&self.dir)?
            .into_iter()
            .filter(|target| {
                target.kind == TargetKind::Branch && target.branch.as_deref() == Some(&branch)
            })
            .collect();
        candidates.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let same_birth = birth.as_ref().and_then(|birth| {
            candidates
                .iter()
                .position(|target| target.branch_birth.as_ref() == Some(birth))
        });
        // The default branch a target of this branch was started with applies to later commands.
        let default_branch = self
            .selection
            .default_branch
            .clone()
            .or_else(|| same_birth.and_then(|index| candidates[index].default_branch.clone()));
        let (base, key_base) = self.branch_base(&branch, &head, default_branch.as_deref())?;
        let mut key = format!("branch-{}-{key_base}", self.branch_key(&branch)?);
        if let Some((time, sha)) = birth.as_ref().and_then(|birth| birth.split_once(' ')) {
            let short = prefix(sha, 12);
            let _ = write!(key, "-{time}-{short}");
        }
        let existing = match same_birth {
            Some(index) => Some(candidates.swap_remove(index)),
            None => match candidates.iter().position(|target| target.key == key) {
                Some(index) => Some(candidates.swap_remove(index)),
                None => self.adopt(candidates, &base, &head)?,
            },
        };
        Ok(Scope {
            key: existing.as_ref().map_or(key, |target| target.key.clone()),
            kind: TargetKind::Branch,
            branch: Some(branch),
            birth,
            base: Some(base),
            head,
            head_known: true,
            default_branch,
            existing,
        })
    }

    /// The encoded branch name, shortened with a hash of the full name so the target key stays
    /// under the store's length limit.
    pub(super) fn branch_key(&self, branch: &str) -> Result<String> {
        const KEEP: usize = 87;
        let encoded = crate::forge::percent_encode(branch, b"");
        if encoded.len() <= KEEP + 13 {
            return Ok(encoded);
        }
        let mut cut = KEEP;
        // MUST NOT split a `%XX` escape.
        while encoded[..cut].ends_with('%') || encoded[..cut - 1].ends_with('%') {
            cut -= 1;
        }
        let hash = self.repo.hash_text(branch)?;
        Ok(format!("{}-{}", &encoded[..cut], prefix(&hash, 12)))
    }

    /// The head of a branch target: `HEAD` when on its branch, or when detached on top of the
    /// local branch or the synced PR/MR head (a review worktree with fix commits, also after a
    /// force-push); else the local branch, else the PR/MR head. An unrelated `HEAD` MUST NOT
    /// stand in. The error says why there is none.
    pub(super) fn branch_head(
        &self,
        target: &Target,
        branch: &str,
    ) -> Result<Result<String, String>> {
        let current = self.repo.current_branch()?;
        let head = self.repo.head()?;
        if current.as_deref() == Some(branch) {
            return Ok(Ok(head));
        }
        let local = self.repo.commit(&format!("refs/heads/{branch}")).ok();
        let pr_head = target.review.as_ref().map(|review| &review.head_sha);
        let pr_local = match pr_head {
            Some(sha) if self.repo.object_exists(&format!("{sha}^{{commit}}"))? => Some(sha),
            _ => None,
        };
        if current.is_none() {
            for known in local.iter().chain(pr_local) {
                if self.repo.is_ancestor(known, &head)? {
                    return Ok(Ok(head));
                }
            }
        }
        Ok(match (local, pr_local, pr_head) {
            (Some(local), _, _) => Ok(local),
            (None, Some(sha), _) => Ok(sha.clone()),
            (None, None, Some(sha)) => Err(format!(
                "its PR/MR head {sha} is not in this repository; fetch it or check out the branch"
            )),
            (None, None, None) => Err("no PR/MR head is known; check out the branch".to_owned()),
        })
    }

    /// The scope at `--head`: a range or commit target's later head, such as its fix, which
    /// MUST descend from the target's end.
    pub(super) fn scope_at(&self, rev: Option<&str>) -> Result<Scope> {
        let mut scope = self.scope()?;
        let Some(rev) = rev else {
            return Ok(scope);
        };
        if scope.kind == TargetKind::Branch {
            return fail("--head is for range and commit targets; a branch target uses its head");
        }
        let head = self.repo.commit(rev)?;
        if !self.repo.is_ancestor(&scope.head, &head)? {
            return fail(format!(
                "--head {rev} does not descend from the end {} of target {}",
                scope.head, scope.key
            ));
        }
        scope.head = head;
        Ok(scope)
    }

    pub(super) fn is_default_branch(&self, scope: &Scope) -> Result<bool> {
        let Some(branch) = scope
            .branch
            .as_deref()
            .filter(|_| scope.kind == TargetKind::Branch)
        else {
            return Ok(false);
        };
        let default = self.repo.default_branch(scope.default_branch.as_deref())?;
        Ok(default == format!("refs/heads/{branch}"))
    }

    /// Returns the review base and the commit that names the target. On the default branch
    /// itself the merge-base is the head, so the root commit names the target instead.
    pub(super) fn branch_base(
        &self,
        branch: &str,
        head: &str,
        stored: Option<&str>,
    ) -> Result<(String, String)> {
        let default = self
            .repo
            .default_branch(self.selection.default_branch.as_deref().or(stored))?;
        if default == format!("refs/heads/{branch}") {
            return Ok((head.to_owned(), self.repo.root_commit(head)?));
        }
        let base = self
            .repo
            .merge_base(head, &default)?
            .ok_or_else(|| Error(format!("{branch} has no merge-base with {default}")))?;
        Ok((base.clone(), base))
    }

    /// Finds an older target of the same branch that this branch continues after a rebase or a
    /// merge from the default branch moved its merge-base. A reused branch name shares no
    /// reviewed commits or patch-ids with the old target, so it starts fresh.
    pub(super) fn adopt(
        &self,
        candidates: Vec<Target>,
        base: &str,
        head: &str,
    ) -> Result<Option<Target>> {
        if candidates.is_empty() {
            return Ok(None);
        }
        let current: HashSet<String> = self
            .repo
            .commits(Some(base), head)?
            .into_iter()
            .filter_map(|commit| commit.patch_id)
            .collect();
        for target in candidates {
            if self.continues(&target, base, head, &current)? {
                return Ok(Some(target));
            }
        }
        Ok(None)
    }

    pub(super) fn continues(
        &self,
        target: &Target,
        base: &str,
        head: &str,
        current: &HashSet<String>,
    ) -> Result<bool> {
        for pass in passes(target) {
            let reachable = self.repo.is_ancestor(&pass.head, head)?
                && !self.repo.is_ancestor(&pass.head, base)?;
            if reachable || pass_patch_ids(pass).any(|id| current.contains(id)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn new_target(&self, scope: &Scope, now: &str) -> Target {
        Target {
            version: VERSION,
            revision: 0,
            key: scope.key.clone(),
            repo: self.repo_key.clone(),
            kind: scope.kind,
            branch: scope.branch.clone(),
            branch_birth: scope.birth.clone(),
            base: scope.base.clone(),
            head: (scope.kind != TargetKind::Branch).then(|| scope.head.clone()),
            default_branch: scope.default_branch.clone(),
            created_at: now.to_owned(),
            updated_at: now.to_owned(),
            review: None,
            runs: Vec::new(),
            records: Vec::new(),
            threads: Vec::new(),
            qa_runs: Vec::new(),
        }
    }

    pub(super) fn update<R>(
        &self,
        scope: &Scope,
        change: impl FnOnce(&mut Target, &str) -> Result<R>,
    ) -> Result<R> {
        let now = clock::now()?;
        self.store.update(
            &self.dir,
            &scope.key,
            &now,
            || self.new_target(scope, &now),
            |target| {
                if target.default_branch.is_none() {
                    target.default_branch.clone_from(&scope.default_branch);
                }
                change(target, &now)
            },
        )
    }

    /// The reviewed state: the work tree for a branch whose head is checked out (possibly
    /// uncommitted), else the head commit, such as for `--target` while elsewhere.
    pub(super) fn reviewed_state(&self, scope: &Scope) -> Result<(Option<String>, String)> {
        let head_tree = self.repo.tree_of(&scope.head)?;
        if scope.kind != TargetKind::Branch || self.repo.head()? != scope.head {
            return Ok((Some(scope.head.clone()), head_tree));
        }
        let tree = self.repo.worktree_tree()?;
        if tree == head_tree {
            Ok((Some(scope.head.clone()), tree))
        } else {
            Ok((None, tree))
        }
    }
}

pub(super) struct Recorded {
    pub(super) commits: Vec<CommitInfo>,
    pub(super) head_patch_id: Option<String>,
}

/// Patch-ids to adopt for passes whose reviewed tree now equals the head's tree, computed
/// before taking the lock, which only covers the read-modify-write.
pub(super) struct Adoption {
    pub(super) head: String,
    pub(super) head_tree: String,
    pub(super) passes: HashMap<String, Recorded>,
}

impl Ctx {
    pub(super) fn adoption(&self, scope: &Scope) -> Result<Adoption> {
        let head_tree = self.repo.tree_of(&scope.head)?;
        let mut passes = HashMap::new();
        for run in scope.existing.iter().flat_map(|target| &target.runs) {
            for pass in &run.passes {
                if pass.tree != head_tree || pass.head == scope.head {
                    continue;
                }
                let commits = if self.repo.is_ancestor(&pass.head, &scope.head)? {
                    self.repo.commits(Some(&pass.head), &scope.head)?
                } else if pass.partial {
                    Vec::new()
                } else {
                    self.repo.commits(scope.base.as_deref(), &scope.head)?
                };
                let head_patch_id = match commits.iter().find(|commit| commit.sha == scope.head) {
                    Some(commit) => commit.patch_id.clone(),
                    None => self.repo.commit_patch_id(&scope.head)?,
                };
                passes.insert(
                    format!("{}.{}", run.id, pass.id),
                    Recorded {
                        commits,
                        head_patch_id,
                    },
                );
            }
        }
        Ok(Adoption {
            head: scope.head.clone(),
            head_tree,
            passes,
        })
    }
}

impl Adoption {
    /// Records the patch-ids and points tree-only anchors at the head; returns the pass ids.
    pub(super) fn apply(&self, target: &mut Target) -> Vec<String> {
        let mut recorded = Vec::new();
        for run in &mut target.runs {
            for pass in &mut run.passes {
                let id = format!("{}.{}", run.id, pass.id);
                let Some(record) = self.passes.get(&id) else {
                    continue;
                };
                for commit in &record.commits {
                    if !pass.commits.iter().any(|known| known.sha == commit.sha) {
                        pass.commits.push(CommitPatch {
                            sha: commit.sha.clone(),
                            patch_id: commit.patch_id.clone(),
                        });
                    }
                }
                if pass.head_patch_id.is_none() {
                    pass.head_patch_id.clone_from(&record.head_patch_id);
                }
                recorded.push(id);
            }
        }
        for thread in &mut target.threads {
            if let Some(anchor) = &mut thread.anchor
                && anchor.commit.is_none()
                && anchor.tree == self.head_tree
            {
                anchor.commit = Some(self.head.clone());
            }
        }
        recorded
    }
}

/// What a name given to a status command stands for.
enum Named {
    /// A stored target that already holds the PR/MR.
    Target(String),
    /// A branch, and the PR/MR number that named it when the forge resolved one.
    Branch { branch: String, pr: Option<u64> },
}

impl Ctx {
    /// Points the selection at the stored target of `name`: a PR/MR (`12`, `#12`, `!12` or its
    /// URL), a worktree (its directory name or path) or a branch. A name that is the current
    /// branch leaves the selection alone, so the current work tree is still evaluated.
    pub(super) fn resolve_name(&mut self, name: &str) -> Result<()> {
        let targets = self.store.targets(&self.dir)?;
        let mut forge_error = None;
        let named = match crate::forge::parse_pr(name) {
            Ok(pr) => match self.named_pr(&pr, &targets) {
                Ok(Some(named)) => Some(named),
                Ok(None) => self.named_local(name, &targets)?,
                Err(why) => {
                    forge_error = Some(why);
                    self.named_local(name, &targets)?
                }
            },
            Err(_) => self.named_local(name, &targets)?,
        };
        let Some(named) = named else {
            let note = forge_error.map_or_else(String::new, |why| format!(" ({why})"));
            return fail(format!(
                "{name} is no known PR/MR, worktree or branch with review or QA records{note}"
            ));
        };
        let (branch, pr) = match named {
            Named::Target(key) => {
                self.selection.target = Some(key);
                return Ok(());
            }
            Named::Branch { branch, pr } => (branch, pr),
        };
        let newest = targets
            .iter()
            .filter(|target| {
                target.kind == TargetKind::Branch
                    && target.branch.as_deref() == Some(&branch)
                    // A target holding another PR/MR's review is that one's.
                    && pr.is_none_or(|number| {
                        target
                            .review
                            .as_ref()
                            .is_none_or(|review| review.number == number)
                    })
            })
            .max_by(|a, b| a.updated_at.cmp(&b.updated_at));
        // The current branch keeps its work tree, unless a PR/MR named it and every target of
        // the branch holds another PR/MR's review.
        if self.repo.current_branch()?.as_deref() == Some(&branch)
            && (pr.is_none() || newest.is_some())
        {
            return Ok(());
        }
        match (newest, pr) {
            (Some(target), _) => self.selection.target = Some(target.key.clone()),
            (None, Some(number)) => {
                return fail(format!(
                    "{name} is PR/MR {number} of branch {branch}, which has no review or QA records here"
                ));
            }
            (None, None) => return fail(format!("branch {branch} has no review or QA records")),
        }
        Ok(())
    }

    /// The stored target that holds PR/MR `pr`, else its head branch as the forge reports it
    /// (in any state, and not when offline).
    fn named_pr(&self, pr: &crate::forge::PrRef, targets: &[Target]) -> Result<Option<Named>> {
        let holder = targets
            .iter()
            .filter(|target| {
                target.review.as_ref().is_some_and(|review| {
                    review.number == pr.number
                        && pr.forge.is_none_or(|forge| forge == review.forge)
                        && pr.location.as_ref().is_none_or(|(host, repo)| {
                            same_location(
                                (host.as_str(), repo.as_str()),
                                (review.host.as_str(), review.repo.as_str()),
                            )
                        })
                })
            })
            .max_by(|a, b| a.updated_at.cmp(&b.updated_at));
        if let Some(target) = holder {
            return Ok(Some(Named::Target(target.key.clone())));
        }
        if crate::forge::auto::Offline::current().is_set() {
            return Ok(None);
        }
        // A URL names its tracker, which must be one of the repository's; a bare number is the
        // chosen tracker's, as for `agt pr`.
        let source = match &pr.location {
            Some((host, repo)) => {
                let config = crate::core::config::Config::load_for_repo(&self.repo)?;
                crate::forge::landed::Source::trackers(&self.repo, &config)?
                    .into_iter()
                    .find(|source| {
                        same_location((host.as_str(), repo.as_str()), (&source.host, &source.repo))
                    })
            }
            None => crate::forge::landed::Source::chosen(&self.repo)?,
        };
        let Some(origin) = source else {
            return Ok(None);
        };
        // A fork's head branch name says nothing about local branches of the same name.
        Ok(origin
            .pull(pr.number)?
            .filter(|pull| pull.same_repo)
            .map(|pull| Named::Branch {
                branch: pull.head_branch,
                pr: Some(pull.number),
            }))
    }

    /// The branch a worktree name or a branch name stands for; two different branches are an
    /// error that names both.
    fn named_local(&self, name: &str, targets: &[Target]) -> Result<Option<Named>> {
        let mut branches: Vec<String> = Vec::new();
        let wanted = std::env::current_dir()
            .map(|cwd| cwd.join(name))
            .ok()
            .map(|path| path.canonicalize().unwrap_or(path));
        for worktree in self.repo.worktrees()? {
            let path = worktree
                .path
                .canonicalize()
                .unwrap_or_else(|_| worktree.path.clone());
            let named = !worktree.bare
                && (worktree.path == Path::new(name)
                    || wanted.as_ref().is_some_and(|wanted| *wanted == path)
                    || worktree.path.file_name().is_some_and(|base| base == name));
            if let (true, Some(branch)) = (named, worktree.branch) {
                branches.push(branch);
            }
        }
        if self.repo.commit(&format!("refs/heads/{name}")).is_ok()
            || targets.iter().any(|target| {
                target.kind == TargetKind::Branch && target.branch.as_deref() == Some(name)
            })
        {
            branches.push(name.to_owned());
        }
        branches.sort();
        branches.dedup();
        match branches.as_slice() {
            [] => Ok(None),
            [branch] => Ok(Some(Named::Branch {
                branch: branch.clone(),
                pr: None,
            })),
            many => fail(format!(
                "{name} names more than one branch ({}); pass a branch name",
                many.join(", ")
            )),
        }
    }
}

/// Whether two `(host, repo)` pairs name one repository: forges ignore case in both, and a URL
/// may carry a port the remote leaves out.
pub(super) fn same_location(a: (&str, &str), b: (&str, &str)) -> bool {
    let host = |host: &str| host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    host(a.0) == host(b.0) && a.1.eq_ignore_ascii_case(b.1)
}

#[cfg(test)]
mod tests {
    use super::same_location;

    #[test]
    fn locations_match_without_case_or_port() {
        assert!(same_location(
            ("github.com", "Acme/App"),
            ("github.com", "acme/app")
        ));
        assert!(same_location(
            ("gitlab.example.com:8443", "g/p"),
            ("gitlab.example.com", "g/p")
        ));
        assert!(!same_location(
            ("github.com", "acme/app"),
            ("github.com", "acme/other")
        ));
    }
}
