use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::core::error::Result;
use crate::git::{CommitInfo, LogEntry, Repo};
use crate::store::model::{CommitPatch, ImportedRecord, Pass, PassKind, Target, TargetKind};

use super::view::{Locator, run_views, thread_views};
use super::{Ctx, Scope, pass_patch_ids};

/// Where a commit's mark came from: a pass of a run, or (no pass) a run's repair.
#[derive(Clone, Copy)]
pub(super) struct Origin<'a> {
    pub(super) target: &'a str,
    pub(super) run: &'a str,
    pub(super) pass: Option<&'a Pass>,
}

/// What reviewed commits are keyed by, each with the passes (or repairs) that recorded it.
#[derive(Default)]
pub(super) struct Reviewed<'a> {
    patch_ids: HashMap<String, Vec<Origin<'a>>>,
    trees: HashMap<String, Vec<Origin<'a>>>,
    shas: HashMap<String, Vec<Origin<'a>>>,
}

impl<'a> Reviewed<'a> {
    /// Records what an ended `pass` reviewed; its tree only for the target's own whole-branch
    /// passes, since a tree match alone may sit on another base, and a partial pass's tree says
    /// nothing about a squash or reorder of the commits around its delta.
    fn add_pass(&mut self, origin: Origin<'a>, with_tree: bool) {
        let Some(pass) = origin.pass else {
            return;
        };
        for id in pass_patch_ids(pass) {
            self.patch_ids.entry(id.clone()).or_default().push(origin);
        }
        if with_tree {
            self.trees
                .entry(pass.tree.clone())
                .or_default()
                .push(origin);
        }
        if !pass.dirty {
            self.shas.entry(pass.head.clone()).or_default().push(origin);
        }
        for commit in &pass.commits {
            self.shas
                .entry(commit.sha.clone())
                .or_default()
                .push(origin);
        }
    }

    fn add_repair(&mut self, origin: Origin<'a>, commit: &CommitPatch) {
        self.shas
            .entry(commit.sha.clone())
            .or_default()
            .push(origin);
        if let Some(id) = &commit.patch_id {
            self.patch_ids.entry(id.clone()).or_default().push(origin);
        }
    }

    /// The origins that mark `commit`, one per pass or run, by the strongest key.
    fn marks(&self, commit: &CommitInfo, via: Option<&'static str>) -> Vec<Mark<'a>> {
        let keys = [
            ("sha", self.shas.get(&commit.sha)),
            (
                "patch-id",
                commit
                    .patch_id
                    .as_ref()
                    .and_then(|id| self.patch_ids.get(id)),
            ),
            ("tree", self.trees.get(&commit.tree)),
        ];
        let mut marks: Vec<Mark<'a>> = Vec::new();
        for (key, origins) in keys {
            for origin in origins.into_iter().flatten() {
                let seen = marks.iter().any(|mark| {
                    mark.origin.run == origin.run
                        && mark.origin.pass.map(|pass| &pass.id) == origin.pass.map(|pass| &pass.id)
                });
                if !seen {
                    marks.push(Mark {
                        via: via.unwrap_or(key),
                        origin: *origin,
                    });
                }
            }
        }
        marks
    }
}

/// One reason a commit counts as reviewed.
pub(super) struct Mark<'a> {
    pub(super) via: &'static str,
    pub(super) origin: Origin<'a>,
}

/// A commit's marks and the kind they are: reviewed by a pass (`"pass"`) or only fixed after
/// its run's last pass (`"repair"`).
pub(super) fn commit_marks<'a>(
    commit: &CommitInfo,
    reviewed: &Reviewed<'a>,
    repaired: &Reviewed<'a>,
) -> (Option<&'static str>, Vec<Mark<'a>>) {
    let passes = reviewed.marks(commit, None);
    if !passes.is_empty() {
        return (Some("pass"), passes);
    }
    let repairs = repaired.marks(commit, Some("repair"));
    if repairs.is_empty() {
        (None, repairs)
    } else {
        (Some("repair"), repairs)
    }
}

impl Mark<'_> {
    pub(super) fn json(&self) -> Value {
        json!({
            "target": self.origin.target,
            "run": self.origin.run,
            "pass": self.origin.pass.map(|pass| &pass.id),
            "kind": self.origin.pass.map(|pass| pass.kind),
            "pass_commits": self.origin.pass.map(|pass| pass.commits.len()),
            "via": self.via,
        })
    }
}

/// A pass covering a head, with its target and run.
#[derive(Clone)]
pub(super) struct Covered<'a> {
    pub(super) target: &'a str,
    pub(super) run: &'a str,
    pub(super) pass: &'a Pass,
    pub(super) matched: Vec<&'static str>,
}

/// Ended passes of `targets` whose reviewed state matches `tree`, the whole-range patch-id or
/// the head commit's patch-id.
pub(super) fn coverage<'a>(
    targets: &[&'a Target],
    tree: &str,
    range_patch_id: Option<&str>,
    head_patch_id: Option<&str>,
) -> Vec<Covered<'a>> {
    let mut covered = Vec::new();
    for target in targets {
        for run in &target.runs {
            // A partial pass reviewed only its delta, so it covers commits, never the whole
            // head.
            for pass in run
                .passes
                .iter()
                .filter(|pass| pass.ended_at.is_some() && !pass.partial)
            {
                let mut matched = Vec::new();
                if pass.tree == tree {
                    matched.push("tree");
                }
                if range_patch_id.is_some() && pass.range_patch_id.as_deref() == range_patch_id {
                    matched.push("range-patch-id");
                }
                // Only a one-commit head (the caller passes no id otherwise) may match by its
                // own patch-id, and only the pass's whole range: a matching last commit says
                // nothing about rewritten earlier ones.
                if let Some(id) = head_patch_id
                    && pass.range_patch_id.as_deref() == Some(id)
                {
                    matched.push("head-patch-id");
                }
                if !matched.is_empty() {
                    covered.push(Covered {
                        target: &target.key,
                        run: &run.id,
                        pass,
                        matched,
                    });
                }
            }
        }
    }
    covered
}

/// Passes covering `head` itself; a changed merge the caller found unreviewed voids them all.
fn head_coverage<'a>(
    repo: &Repo,
    targets: &Targets<'a>,
    base: Option<&str>,
    head: &str,
    head_tree: &str,
    commits: &[CommitInfo],
) -> Result<Vec<Covered<'a>>> {
    let head_patch_id = match commits {
        [only] if only.sha == head => only.patch_id.clone(),
        _ => None,
    };
    let range_patch_id = repo.range_patch_id(base, head)?;
    Ok(targets.covered_at(
        head_tree,
        range_patch_id.as_deref(),
        head_patch_id.as_deref(),
    ))
}

/// The passes whose range holds each changed merge: a merge is reviewed once an ended pass of
/// any target of the repository started after its first parent's history and ended at a head
/// containing it (`base..head`). Ancestors of a reviewed head, tree matches and repairs do not
/// count. A merge no pass holds is absent.
pub(super) fn merge_origins<'a, 'h>(
    repo: &Repo,
    merges: &[&'h LogEntry],
    targets: &[&'a Target],
) -> Result<HashMap<&'h str, Vec<Origin<'a>>>> {
    let mut found: HashMap<&'h str, Vec<Origin<'a>>> = HashMap::new();
    if merges.is_empty() {
        return Ok(found);
    }
    for target in targets {
        for run in &target.runs {
            for pass in run.passes.iter().filter(|pass| pass.ended_at.is_some()) {
                let known = |rev: &str| repo.object_exists(&format!("{rev}^{{commit}}"));
                if !known(&pass.head)?
                    || pass.base.as_deref().map(known).transpose()? == Some(false)
                {
                    continue;
                }
                let inside = repo.merges(pass.base.as_deref(), &pass.head)?;
                for merge in merges.iter().filter(|merge| inside.contains(&merge.sha)) {
                    found.entry(merge.sha.as_str()).or_default().push(Origin {
                        target: &target.key,
                        run: &run.id,
                        pass: Some(pass),
                    });
                }
            }
        }
    }
    Ok(found)
}

/// The changed merges no pass holds: they bring in code nobody reviewed.
pub(super) fn unreviewed_merges<'h>(
    merges: &[&'h LogEntry],
    origins: &HashMap<&str, Vec<Origin>>,
) -> HashSet<&'h str> {
    merges
        .iter()
        .map(|merge| merge.sha.as_str())
        .filter(|sha| !origins.contains_key(sha))
        .collect()
}

/// The reviews of run `run` covering `head`, counted the way `status` counts
/// `reviews_at_head`.
pub(super) fn run_reviews_at(ctx: &Ctx, target: &Target, run: &str, head: &str) -> Result<usize> {
    if !ctx.repo.object_exists(&format!("{head}^{{commit}}"))? {
        return Ok(0);
    }
    let base = match (target.kind, &target.branch) {
        (TargetKind::Branch, Some(branch)) => Some(
            ctx.branch_base(branch, head, target.default_branch.as_deref())?
                .0,
        ),
        _ => target.base.clone(),
    };
    let history = ctx.repo.log(base.as_deref(), head)?;
    let merges = changed_merges(&ctx.repo, &history)?;
    let stored = ctx.store.targets(&ctx.dir)?;
    let all: Vec<&Target> = stored.iter().collect();
    let origins = merge_origins(&ctx.repo, &merges, &all)?;
    if !unreviewed_merges(&merges, &origins).is_empty() {
        return Ok(0);
    }
    let commits = ctx.repo.commits(base.as_deref(), head)?;
    let tree = ctx.repo.tree_of(head)?;
    // Only the run's own target counts for its record.
    let targets = Targets {
        target: Some(target),
        others: &[],
    };
    let covered: Vec<_> =
        head_coverage(&ctx.repo, &targets, base.as_deref(), head, &tree, &commits)?
            .into_iter()
            .filter(|covered| covered.run == run)
            .collect();
    Ok(count(&covered))
}

pub(super) fn coverage_json(covered: &[Covered]) -> Value {
    covered
        .iter()
        .map(|covered| {
            json!({
                "target": covered.target,
                "run": covered.run,
                "pass": covered.pass.id,
                "kind": covered.pass.kind,
                "major_found": covered.pass.major_found,
                "match": covered.matched,
            })
        })
        .collect()
}

/// Full reviews plus clean fix checks; parallel reviewers in one pass count once.
pub(super) fn count(covered: &[Covered]) -> usize {
    covered
        .iter()
        .filter(|covered| {
            let pass = covered.pass;
            pass.kind == PassKind::Full || (pass.kind == PassKind::FixCheck && !pass.major_found)
        })
        .count()
}

/// The stored targets of the repository other than `target`.
pub(super) fn other_targets(ctx: &Ctx, target: Option<&Target>) -> Result<Vec<Target>> {
    Ok(ctx
        .store
        .targets(&ctx.dir)?
        .into_iter()
        .filter(|other| Some(&other.key) != target.map(|target| &target.key))
        .collect())
}

/// The target and the other targets of the repository, whose passes can cover a head.
struct Targets<'a> {
    target: Option<&'a Target>,
    others: &'a [Target],
}

impl<'a> Targets<'a> {
    /// Another target's pass (a `--range` review of the branch's end point) covers the head
    /// when it reviewed the same diff; a tree match alone may sit on another base.
    /// A run stored in two files (a rename to the PR/MR key cut short) counts once.
    fn covered_at(
        &self,
        tree: &str,
        range: Option<&str>,
        head_id: Option<&str>,
    ) -> Vec<Covered<'a>> {
        let mut covered = coverage(self.target.as_slice(), tree, range, head_id);
        let others: Vec<&Target> = self.others.iter().collect();
        for mut other in coverage(&others, tree, range, head_id) {
            other.matched.retain(|matched| *matched != "tree");
            let seen = covered
                .iter()
                .any(|known| known.run == other.run && known.pass.id == other.pass.id);
            if !other.matched.is_empty() && !seen {
                covered.push(other);
            }
        }
        covered
    }

    /// The commits ended passes reviewed, and the repaired ones (fixes after a run's last
    /// pass), which count as reviewed but never add to reviews_at_head, also when another
    /// target's run repaired them (a range review's fix). Run `except` is left out: a fix
    /// check is not told that its own run reviewed the commits.
    fn reviewed(&self, except: Option<&str>) -> (Reviewed<'a>, Reviewed<'a>) {
        let mut reviewed = Reviewed::default();
        let mut repaired = Reviewed::default();
        // Commits a review of another target of this repository recorded (a `--range` review of
        // the branch's end point, an earlier branch target) are reviewed here too, by patch-id
        // and sha only.
        let own = self.target.into_iter().map(|target| (target, true));
        let others = self.others.iter().map(|other| (other, false));
        for (target, own) in own.chain(others) {
            for run in &target.runs {
                let pass_origin = |pass| Origin {
                    target: &target.key,
                    run: &run.id,
                    pass: Some(pass),
                };
                let ended = run.passes.iter().filter(|pass| pass.ended_at.is_some());
                if except != Some(run.id.as_str()) {
                    for pass in ended {
                        reviewed.add_pass(pass_origin(pass), own && !pass.partial);
                    }
                }
                let origin = Origin {
                    target: &target.key,
                    run: &run.id,
                    pass: None,
                };
                for commit in &run.repair {
                    repaired.add_repair(origin, commit);
                }
            }
        }
        (reviewed, repaired)
    }
}

/// Each commit and changed merge since the base with whether a pass or a repair reviewed it
/// and what by, in history order.
fn commit_views(
    commits: &[CommitInfo],
    merges: &[&LogEntry],
    merge_origins: &HashMap<&str, Vec<Origin>>,
    history: &[LogEntry],
    sets: (&Reviewed, &Reviewed),
) -> Vec<Value> {
    let (reviewed, repaired) = sets;
    let mut views: Vec<Value> = commits
        .iter()
        .map(|commit| {
            let (matched, marks) = commit_marks(commit, reviewed, repaired);
            json!({
                "sha": commit.sha,
                "subject": commit.subject,
                "patch_id": commit.patch_id,
                "reviewed": matched.is_some(),
                "match": matched,
                "by": marks.iter().map(Mark::json).collect::<Vec<_>>(),
            })
        })
        .collect();
    // A changed merge is reviewed once an ended pass's range holds it.
    views.extend(merges.iter().map(|merge| {
        let origins = merge_origins.get(merge.sha.as_str());
        let by: Vec<Value> = origins
            .into_iter()
            .flatten()
            .map(|origin| {
                Mark {
                    via: "contained",
                    origin: *origin,
                }
                .json()
            })
            .collect();
        json!({
            "sha": merge.sha,
            "subject": merge.subject,
            "patch_id": null,
            "reviewed": origins.is_some(),
            "match": origins.map(|_| "merge"),
            "by": by,
            "merge": true,
        })
    }));
    let order: HashMap<&str, usize> = history
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.sha.as_str(), index))
        .collect();
    views.sort_by_key(|view| view["sha"].as_str().and_then(|sha| order.get(sha)).copied());
    views
}

/// The requested range of a pass about to start, for [`already_reviewed`].
pub(super) struct Requested<'a> {
    pub(super) scope: &'a Scope,
    /// The run the pass joins; its earlier passes are not reported for a fix check or reveal.
    pub(super) run: Option<&'a str>,
    pub(super) kind: PassKind,
    /// Where the merge-base-scoped range starts (the pass's own base when partial).
    pub(super) range_base: Option<&'a str>,
    pub(super) commits: &'a [CommitInfo],
    pub(super) tree: &'a str,
    pub(super) range_patch_id: Option<&'a str>,
    /// Whether the reviewed state is committed, so the head's own patch-id may match.
    pub(super) committed: bool,
}

/// What earlier passes already reviewed of the commits a pass is about to cover: the passes
/// covering the whole head, each commit's marks, the commits nobody reviewed and a hint. It
/// is information only; the pass still covers the full range.
pub(super) fn already_reviewed(ctx: &Ctx, requested: &Requested) -> Result<Value> {
    let scope = requested.scope;
    let target = scope.existing.as_ref();
    let others = other_targets(ctx, target)?;
    let except = (requested.kind != PassKind::Full)
        .then_some(requested.run)
        .flatten();
    let targets = Targets {
        target,
        others: &others,
    };
    let (reviewed, repaired) = targets.reviewed(except);

    // A changed merge nobody reviewed voids the head's coverage, as for `status`.
    let history = ctx.repo.log(requested.range_base, &scope.head)?;
    let merge_after = if history.iter().any(|entry| entry.parents.len() > 1) {
        let reader = Repo::discover()?.read_only()?;
        let merges = changed_merges(&reader, &history)?;
        let every: Vec<&Target> = target.into_iter().chain(&others).collect();
        let origins = merge_origins(&ctx.repo, &merges, &every)?;
        !unreviewed_merges(&merges, &origins).is_empty()
    } else {
        false
    };
    // Only a head that is the whole range, one commit above the range start, may match by its
    // own patch-id: a fix check's lone commit says nothing about the commits before it.
    let head_patch_id = match requested.commits {
        [only]
            if only.sha == scope.head
                && requested.committed
                && ctx.repo.parent(&only.sha)?.as_deref() == requested.range_base =>
        {
            only.patch_id.clone()
        }
        _ => None,
    };
    let head: Vec<Covered> = if merge_after {
        Vec::new()
    } else {
        targets
            .covered_at(
                requested.tree,
                requested.range_patch_id,
                head_patch_id.as_deref(),
            )
            .into_iter()
            .filter(|covered| except != Some(covered.run))
            .collect()
    };

    let mut commits = Vec::new();
    let mut unreviewed: Vec<&CommitInfo> = Vec::new();
    for commit in requested.commits {
        let (_, marks) = commit_marks(commit, &reviewed, &repaired);
        if marks.is_empty() {
            unreviewed.push(commit);
        } else {
            commits.push(json!({
                "sha": commit.sha,
                "subject": commit.subject,
                "via": marks.first().map(|mark| mark.via),
                "by": marks.iter().map(Mark::json).collect::<Vec<_>>(),
            }));
        }
    }
    let total = requested.commits.len();
    let done = commits.len();
    let hint = match unreviewed.first() {
        _ if done == 0 => Value::Null,
        None => json!(format!("all {total} commits already reviewed")),
        Some(oldest) => {
            let pending = unreviewed.len();
            let from = requested
                .commits
                .iter()
                .position(|commit| commit.sha == oldest.sha)
                .unwrap_or(0);
            let suffix = total - from;
            // A base at the range start would be the pass already requested, not a partial one.
            let start = if from == 0 {
                None
            } else {
                ctx.repo.parent(&oldest.sha)?
            };
            match start {
                Some(sha) if suffix == pending => json!(format!(
                    "{done} of {total} already reviewed; --base {sha} reviews only the other {pending} (partial)"
                )),
                Some(sha) => json!(format!(
                    "{done} of {total} already reviewed; --base {sha} starts at the oldest unreviewed commit and reviews {suffix} (partial)"
                )),
                None => json!(format!("{done} of {total} already reviewed")),
            }
        }
    };
    Ok(json!({
        "head": coverage_json(&head),
        "commits": commits,
        "unreviewed": unreviewed.iter().map(|commit| &commit.sha).collect::<Vec<_>>(),
        "hint": hint,
    }))
}

pub(super) fn status(ctx: &Ctx, head_arg: Option<&str>) -> Result<Value> {
    let mut scope = ctx.scope()?;
    if let Some(rev) = head_arg {
        scope.head = ctx.repo.commit(rev)?;
        scope.head_known = true;
    }
    if !scope.head_known {
        return Ok(status_without_head(ctx, &scope));
    }
    let target = scope.existing.as_ref();
    let head = scope.head.clone();
    let head_tree = ctx.repo.tree_of(&head)?;
    let commits = ctx.repo.commits(scope.base.as_deref(), &head)?;
    let history = ctx.repo.log(scope.base.as_deref(), &head)?;
    let merges = changed_merges(&ctx.repo, &history)?;
    let last_reviewed = last_reviewed_head(ctx, &head)?;
    let others = other_targets(ctx, target)?;
    let every: Vec<&Target> = target.into_iter().chain(&others).collect();
    let merge_origins = merge_origins(&ctx.repo, &merges, &every)?;
    let merge_after = !unreviewed_merges(&merges, &merge_origins).is_empty();
    let targets = Targets {
        target,
        others: &others,
    };
    let covered = if merge_after {
        Vec::new()
    } else {
        head_coverage(
            &ctx.repo,
            &targets,
            scope.base.as_deref(),
            &head,
            &head_tree,
            &commits,
        )?
    };

    let (reviewed, repaired) = targets.reviewed(None);
    let commit_views = commit_views(
        &commits,
        &merges,
        &merge_origins,
        &history,
        (&reviewed, &repaired),
    );
    let unreviewed: Vec<&Value> = commit_views
        .iter()
        .filter(|view| view["reviewed"] == false)
        .map(|view| &view["sha"])
        .collect();
    // Where the next review starts: before the oldest unreviewed commit, else after the last
    // reviewed head.
    let review_from = match unreviewed.first().and_then(|sha| sha.as_str()) {
        Some(oldest) => ctx.repo.parent(oldest)?,
        None => last_reviewed.clone(),
    };

    // The work tree belongs to the target only when its branch is checked out.
    let checked_out = scope.kind == TargetKind::Branch
        && scope.branch.is_some()
        && ctx.repo.current_branch()? == scope.branch;
    let mut worktree_covered = Vec::new();
    let worktree = if head_arg.is_none() && checked_out {
        let tree = ctx.repo.worktree_tree()?;
        let dirty = tree != head_tree;
        worktree_covered = if dirty && !merge_after {
            let range = ctx.repo.range_patch_id(scope.base.as_deref(), &tree)?;
            targets.covered_at(&tree, range.as_deref(), None)
        } else {
            covered.clone()
        };
        json!({
            "dirty": dirty,
            "tree": tree,
            "reviewed": !worktree_covered.is_empty(),
            "reviews": count(&worktree_covered),
            "covered_by": coverage_json(&worktree_covered),
        })
    } else {
        Value::Null
    };

    let mut locator = Locator {
        repo: &ctx.repo,
        head: Some(head.clone()),
        diffs: HashMap::new(),
    };
    let mut threads = thread_views(target, &mut locator);
    for view in &mut threads.open {
        view["target"] = json!(target.map(|_| &scope.key));
    }
    // A covering pass of another target counts here, so its open threads show here too.
    let mut shown = HashSet::new();
    for other in others.iter().filter(|other| {
        covered
            .iter()
            .chain(&worktree_covered)
            .any(|covered| covered.target == other.key)
            && shown.insert(&other.key)
    }) {
        let views = thread_views(Some(other), &mut locator);
        threads.open.extend(views.open.into_iter().map(|mut view| {
            view["target"] = json!(other.key);
            view
        }));
        threads.held.extend(views.held);
    }
    let records: Vec<&ImportedRecord> = target
        .iter()
        .flat_map(|target| &target.records)
        .filter(|record| {
            record.head == head || (record.head.len() >= 7 && head.starts_with(&record.head))
        })
        // Records of our own runs, or of runs covering the head, already count through their
        // passes.
        .filter(|record| {
            !target.is_some_and(|target| target.runs.iter().any(|run| run.id == record.run))
                && !covered.iter().any(|covered| covered.run == record.run)
        })
        .collect();
    let imported: u32 = records.iter().map(|record| record.reviews_at_head).sum();
    let local = count(&covered);
    let already_reviewed = commit_views.len() - unreviewed.len();
    // Without a pass covering the whole head, each commit since the merge-base must be reviewed.
    let every_commit = !commit_views.is_empty() && unreviewed.is_empty();
    let mut status = json!({
        "repo": ctx.repo_key,
        "target": target.map(|_| &scope.key),
        "review": target.and_then(|target| target.review.as_ref()),
        "kind": scope.kind,
        "branch": scope.branch,
        "base": scope.base,
        "head": head,
        "head_tree": head_tree,
        "reviewed": !covered.is_empty() || imported > 0 || every_commit,
        "reviews_at_head": local + usize::try_from(imported).unwrap_or(usize::MAX),
        "last_reviewed_head": last_reviewed,
        "review_from": review_from,
        "covered_by": coverage_json(&covered),
        "imported_records": records,
        "worktree": worktree,
        "commits": commit_views,
        "unreviewed_commits": unreviewed,
        "already_reviewed": already_reviewed,
        "open_threads": threads.open,
        "outdated_threads": threads.outdated,
        "held_questions": threads.held,
        "runs": run_views(target),
        // Checks are judged against the work tree only where the worktree block read it.
        "qa": super::qa::qa_state(
            target,
            &head,
            Some(worktree["tree"].as_str().unwrap_or(&head_tree)),
        ),
    });
    super::report::extend(&mut status, ctx, target, &history);
    Ok(status)
}

/// For each unreviewed commit whose subject and author match a commit a stored pass recorded
/// under another patch, adds `earlier_version` (the pass) to its entry in `commits`: the
/// commit was probably reviewed before it changed. It is shown only and never counts as
/// reviewed.
pub(super) fn earlier_versions(ctx: &Ctx, status: &mut Value) -> Result<()> {
    let unreviewed: Vec<String> = status["unreviewed_commits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|sha| sha.as_str().map(str::to_owned))
        .collect();
    if unreviewed.is_empty() {
        return Ok(());
    }
    let stored = ctx.store.targets(&ctx.dir)?;
    let mut recorded: Vec<(&str, &str, &str, &CommitPatch)> = Vec::new();
    // Only this target's own passes: a repeated subject such as "fix lint" on another branch
    // says nothing about this commit.
    let own = status["target"].as_str();
    for target in stored
        .iter()
        .filter(|target| Some(target.key.as_str()) == own)
    {
        for run in &target.runs {
            for pass in run.passes.iter().filter(|pass| pass.ended_at.is_some()) {
                for commit in &pass.commits {
                    if !unreviewed.contains(&commit.sha) {
                        recorded.push((&target.key, &run.id, &pass.id, commit));
                    }
                }
            }
        }
    }
    let shas: Vec<&str> = unreviewed
        .iter()
        .map(String::as_str)
        .chain(recorded.iter().map(|(_, _, _, commit)| commit.sha.as_str()))
        .collect();
    let authored = ctx.repo.authored(&shas)?;
    let Some(commits) = status["commits"].as_array_mut() else {
        return Ok(());
    };
    for view in commits.iter_mut().filter(|view| view["reviewed"] == false) {
        let Some(sha) = view["sha"].as_str().map(str::to_owned) else {
            continue;
        };
        let patch_id = view["patch_id"].as_str().map(str::to_owned);
        let Some(mine) = authored.get(&sha) else {
            continue;
        };
        let earlier = recorded.iter().find(|(_, _, _, commit)| {
            commit.patch_id != patch_id && authored.get(&commit.sha) == Some(mine)
        });
        if let Some((target, run, pass, commit)) = earlier {
            view["earlier_version"] = json!({
                "target": target,
                "run": run,
                "pass": pass,
                "sha": commit.sha,
            });
        }
    }
    Ok(())
}

/// `status` without the per-commit list, runs, imported records and tree ids; `--full`
/// prints them.
pub(super) fn brief_status(mut status: Value) -> Value {
    if let Some(fields) = status.as_object_mut() {
        for field in ["commits", "runs", "head_tree"] {
            fields.remove(field);
        }
        if let Some(worktree) = fields.get_mut("worktree").and_then(Value::as_object_mut) {
            worktree.remove("tree");
        }
    }
    status
}

/// `status` for a branch target with no branch, PR/MR head or pass: nothing is reviewed and
/// local anchors have no current place.
pub(super) fn status_without_head(ctx: &Ctx, scope: &Scope) -> Value {
    let target = scope.existing.as_ref();
    let threads = thread_views(
        target,
        &mut Locator {
            repo: &ctx.repo,
            head: None,
            diffs: HashMap::new(),
        },
    );
    let mut status = json!({
        "repo": ctx.repo_key,
        "target": target.map(|_| &scope.key),
        "review": target.and_then(|target| target.review.as_ref()),
        "kind": scope.kind,
        "branch": scope.branch,
        "base": scope.base,
        "head": null,
        "head_tree": null,
        "reviewed": false,
        "reviews_at_head": 0,
        "last_reviewed_head": null,
        "review_from": null,
        "covered_by": [],
        "imported_records": [],
        "worktree": null,
        "commits": [],
        "unreviewed_commits": [],
        "already_reviewed": 0,
        "open_threads": threads.open,
        "outdated_threads": threads.outdated,
        "held_questions": threads.held,
        "runs": run_views(target),
        "qa": super::qa::qa_state(target, "", None),
    });
    super::report::extend(&mut status, ctx, target, &[]);
    status
}

/// The reviewed head nearest to `head` among its ancestors: whole-branch pass heads and
/// repaired heads of this repository's branch and range targets. It scopes the next review,
/// such as on the default branch after a merge.
pub(super) fn last_reviewed_head(ctx: &Ctx, head: &str) -> Result<Option<String>> {
    let mut best: Option<(u64, String)> = None;
    let targets = ctx.store.targets(&ctx.dir)?;
    let mut candidates: Vec<&String> = Vec::new();
    for run in targets
        .iter()
        .filter(|target| matches!(target.kind, TargetKind::Branch | TargetKind::Range))
        .flat_map(|target| &target.runs)
    {
        candidates.extend(
            run.passes
                .iter()
                .filter(|pass| pass.ended_at.is_some() && !pass.dirty && !pass.partial)
                .map(|pass| &pass.head),
        );
        // A run of only partial passes repaired a delta, not the branch; and a repaired head
        // that does not descend from the run's last pass (a rebase before `run end`) names
        // commits that no pass of the run saw.
        if let (Some(repaired), Some(last)) = (&run.repaired_head, run.passes.last())
            && run.passes.iter().any(|pass| !pass.partial)
            && ctx.repo.is_ancestor(&last.head, repaired)?
        {
            candidates.push(repaired);
        }
    }
    for candidate in candidates {
        if best.as_ref().is_some_and(|(_, sha)| sha == candidate)
            || !ctx.repo.object_exists(&format!("{candidate}^{{commit}}"))?
            || !ctx.repo.is_ancestor(candidate, head)?
        {
            continue;
        }
        let distance = ctx.repo.distance(candidate, head)?;
        if best.as_ref().is_none_or(|(nearest, _)| distance < *nearest) {
            best = Some((distance, candidate.clone()));
        }
    }
    Ok(best.map(|(_, sha)| sha))
}

/// Merge commits whose tree is not git's automatic merge of their parents: a conflict
/// resolution or another hand edit nobody reviewed. Octopus merges always count.
pub(super) fn changed_merges<'a>(
    repo: &Repo,
    history: &'a [LogEntry],
) -> Result<Vec<&'a LogEntry>> {
    let mut changed = Vec::new();
    for entry in history.iter().filter(|entry| entry.parents.len() > 1) {
        let automatic = match entry.parents.as_slice() {
            [ours, theirs] => repo.merge_tree(ours, theirs)?,
            _ => None,
        };
        if automatic.as_deref() != Some(entry.tree.as_str()) {
            changed.push(entry);
        }
    }
    Ok(changed)
}
