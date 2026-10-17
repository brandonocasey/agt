use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::core::error::Result;
use crate::git::{CommitInfo, LogEntry, Repo};
use crate::store::model::{ImportedRecord, Pass, PassKind, Target, TargetKind};

use super::view::{Locator, run_views, thread_views};
use super::{Ctx, Scope, pass_patch_ids, passes};

#[derive(Default)]
pub(super) struct Reviewed {
    pub(super) patch_ids: HashSet<String>,
    pub(super) trees: HashSet<String>,
    pub(super) shas: HashSet<String>,
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
pub(super) fn head_coverage<'a>(
    repo: &Repo,
    targets: &[&'a Target],
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
    Ok(coverage(
        targets,
        head_tree,
        range_patch_id.as_deref(),
        head_patch_id.as_deref(),
    ))
}

/// Changed merges after the last reviewed head: they bring in code no pass reviewed.
pub(super) fn unreviewed_merges<'h>(
    repo: &Repo,
    merges: &[&'h LogEntry],
    last_reviewed: Option<&str>,
) -> Result<HashSet<&'h str>> {
    let mut unreviewed = HashSet::new();
    for merge in merges {
        let after = match last_reviewed {
            Some(last) => !repo.is_ancestor(&merge.sha, last)?,
            None => true,
        };
        if after {
            unreviewed.insert(merge.sha.as_str());
        }
    }
    Ok(unreviewed)
}

/// The reviews of run `run` covering `head`, counted the way `status` counts
/// `reviews_at_head`.
pub(super) fn run_reviews_at(ctx: &Ctx, target: &Target, run: &str, head: &str) -> Result<usize> {
    if !ctx.repo.object_exists(&format!("{head}^{{commit}}"))? {
        return Ok(0);
    }
    let base = match (target.kind, &target.branch) {
        (TargetKind::Branch, Some(branch)) => Some(ctx.branch_base(branch, head)?.0),
        _ => target.base.clone(),
    };
    let history = ctx.repo.log(base.as_deref(), head)?;
    let merges = changed_merges(&ctx.repo, &history)?;
    let last_reviewed = last_reviewed_head(ctx, head)?;
    if !unreviewed_merges(&ctx.repo, &merges, last_reviewed.as_deref())?.is_empty() {
        return Ok(0);
    }
    let commits = ctx.repo.commits(base.as_deref(), head)?;
    let tree = ctx.repo.tree_of(head)?;
    let covered: Vec<_> =
        head_coverage(&ctx.repo, &[target], base.as_deref(), head, &tree, &commits)?
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
    let unreviewed_merges = unreviewed_merges(&ctx.repo, &merges, last_reviewed.as_deref())?;
    let merge_after = !unreviewed_merges.is_empty();
    let others: Vec<Target> = ctx
        .store
        .targets(&ctx.dir)?
        .into_iter()
        .filter(|other| Some(&other.key) != target.map(|target| &target.key))
        .collect();
    let others_ref: Vec<&Target> = others.iter().collect();
    // Another target's pass (a `--range` review of the branch's end point) covers the head
    // when it reviewed the same diff; a tree match alone may sit on another base.
    // A run stored in two files (a rename to the PR/MR key cut short) counts once.
    let covered_at = |tree: &str, range: Option<&str>, head_id: Option<&str>| {
        let mut covered = coverage(target.as_slice(), tree, range, head_id);
        for mut other in coverage(&others_ref, tree, range, head_id) {
            other.matched.retain(|matched| *matched != "tree");
            let seen = covered
                .iter()
                .any(|known| known.run == other.run && known.pass.id == other.pass.id);
            if !other.matched.is_empty() && !seen {
                covered.push(other);
            }
        }
        covered
    };
    let covered = if merge_after {
        Vec::new()
    } else {
        let head_patch_id = match commits.as_slice() {
            [only] if only.sha == head => only.patch_id.clone(),
            _ => None,
        };
        let range = ctx.repo.range_patch_id(scope.base.as_deref(), &head)?;
        covered_at(&head_tree, range.as_deref(), head_patch_id.as_deref())
    };

    let mut reviewed = Reviewed::default();
    for pass in target
        .iter()
        .flat_map(|target| passes(target))
        .filter(|pass| pass.ended_at.is_some())
    {
        reviewed.patch_ids.extend(pass_patch_ids(pass).cloned());
        reviewed.trees.insert(pass.tree.clone());
        if !pass.dirty {
            reviewed.shas.insert(pass.head.clone());
        }
        reviewed
            .shas
            .extend(pass.commits.iter().map(|commit| commit.sha.clone()));
    }
    // Commits a review of another target of this repository recorded (a `--range` review of
    // the branch's end point, an earlier branch target) are reviewed here too.
    for other in &others {
        for pass in passes(other).filter(|pass| pass.ended_at.is_some()) {
            reviewed.patch_ids.extend(pass_patch_ids(pass).cloned());
            if !pass.dirty {
                reviewed.shas.insert(pass.head.clone());
            }
            reviewed
                .shas
                .extend(pass.commits.iter().map(|commit| commit.sha.clone()));
        }
    }
    // Repaired commits (fixes after a run's last pass) count as reviewed but never add to
    // reviews_at_head, also when another target's run repaired them (a range review's fix).
    let mut repaired = Reviewed::default();
    for commit in target
        .iter()
        .flat_map(|target| &target.runs)
        .chain(others.iter().flat_map(|other| &other.runs))
        .flat_map(|run| &run.repair)
    {
        repaired.shas.insert(commit.sha.clone());
        repaired.patch_ids.extend(commit.patch_id.clone());
    }
    let mut commit_views: Vec<Value> = commits
        .iter()
        .map(|commit| {
            let in_set = |set: &Reviewed| {
                set.shas.contains(&commit.sha)
                    || set.trees.contains(&commit.tree)
                    || commit
                        .patch_id
                        .as_ref()
                        .is_some_and(|id| set.patch_ids.contains(id))
            };
            let matched = if in_set(&reviewed) {
                Some("pass")
            } else if in_set(&repaired) {
                Some("repair")
            } else {
                None
            };
            json!({
                "sha": commit.sha,
                "subject": commit.subject,
                "patch_id": commit.patch_id,
                "reviewed": matched.is_some(),
                "match": matched,
            })
        })
        .collect();
    // A changed merge is reviewed once a whole-branch pass reviewed a head that contains it.
    commit_views.extend(merges.iter().map(|merge| {
        let reviewed = !unreviewed_merges.contains(merge.sha.as_str());
        json!({
            "sha": merge.sha,
            "subject": merge.subject,
            "patch_id": null,
            "reviewed": reviewed,
            "match": reviewed.then_some("pass"),
            "merge": true,
        })
    }));
    let order: HashMap<&str, usize> = history
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.sha.as_str(), index))
        .collect();
    commit_views.sort_by_key(|view| view["sha"].as_str().and_then(|sha| order.get(sha)).copied());
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
            covered_at(&tree, range.as_deref(), None)
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
    // Without a pass covering the whole head, each commit since the merge-base must be reviewed.
    let every_commit = !commit_views.is_empty() && unreviewed.is_empty();
    Ok(json!({
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
        "open_threads": threads.open,
        "outdated_threads": threads.outdated,
        "held_questions": threads.held,
        "runs": run_views(target),
    }))
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
    json!({
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
        "open_threads": threads.open,
        "outdated_threads": threads.outdated,
        "held_questions": threads.held,
        "runs": run_views(target),
    })
}

/// The reviewed head nearest to `head` among its ancestors: whole-branch pass heads and
/// repaired heads of this repository's branch and range targets. It scopes the next review,
/// such as on the default branch after a merge.
pub(super) fn last_reviewed_head(ctx: &Ctx, head: &str) -> Result<Option<String>> {
    let mut best: Option<(u64, String)> = None;
    let targets = ctx.store.targets(&ctx.dir)?;
    let candidates = targets
        .iter()
        .filter(|target| matches!(target.kind, TargetKind::Branch | TargetKind::Range))
        .flat_map(|target| &target.runs)
        .flat_map(|run| {
            run.passes
                .iter()
                .filter(|pass| pass.ended_at.is_some() && !pass.dirty && !pass.partial)
                .map(|pass| &pass.head)
                // A run of only partial passes repaired a delta, not the branch.
                .chain(
                    run.repaired_head
                        .iter()
                        .filter(|_| run.passes.iter().any(|pass| !pass.partial)),
                )
        });
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
