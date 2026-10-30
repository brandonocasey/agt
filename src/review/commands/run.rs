use std::collections::HashSet;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::core::clock;
use crate::core::error::{Error, Result, ctx, fail};
use crate::core::local::read_source;
use crate::git::CommitInfo;
use crate::store::model::{
    Author, CommitPatch, Pass, PassKind, Reveal, Reviewer, RoleUsage, Run, Thread,
};

use super::findings::{FindingInput, add_findings, parse_findings};
use super::prune::prune_dir;
use super::status::{Requested, already_reviewed};
use super::usage::{UsageInput, run_spend};
use super::{
    Ctx, Scope, find_pass_mut, find_run_mut, fresh_id, major_found, pass_missing, random_id,
};

pub(super) fn run_start(ctx: &Ctx) -> Result<Value> {
    let scope = ctx.scope()?;
    // Run ids are unique in the repository, so `--run` alone finds the target.
    let taken: HashSet<String> = ctx
        .store
        .targets(&ctx.dir)?
        .into_iter()
        .flat_map(|target| target.runs.into_iter().map(|run| run.id))
        .collect();
    let run = ctx.update(&scope, |target, now| {
        let id = fresh_id(
            &taken,
            |id| target.runs.iter().any(|run| run.id == id),
            random_id,
        );
        target.runs.push(Run {
            id: id.clone(),
            started_at: now.to_owned(),
            ended_at: None,
            passes: Vec::new(),
            repaired_head: None,
            repair: Vec::new(),
            reveal: None,
            record: None,
            publication_target: None,
            usage: Vec::new(),
        });
        Ok(id)
    })?;
    Ok(json!({ "target": scope.key, "run": run }))
}

pub(super) fn run_end(
    ctx: &Ctx,
    run_id: &str,
    reveal: Option<Reveal>,
    head: Option<&str>,
    usage: Vec<RoleUsage>,
) -> Result<Value> {
    let scope = ctx.scope_at(head)?;
    let Some(existing) = &scope.existing else {
        return fail(format!("run {run_id} not found in target {}", scope.key));
    };
    let adoption = ctx.adoption(&scope)?;
    // Commits after the run's last reviewed head (the fix commits) are recorded as repaired.
    let last_head = existing
        .runs
        .iter()
        .find(|run| run.id == run_id)
        .and_then(|run| run.passes.last())
        .map(|pass| pass.head.clone());
    let repair = match &last_head {
        Some(last) if ctx.repo.is_ancestor(last, &scope.head)? => ctx
            .repo
            .commits_first_parent(Some(last), &scope.head)?
            .into_iter()
            .map(|commit| CommitPatch {
                sha: commit.sha,
                patch_id: commit.patch_id,
            })
            .collect(),
        _ => Vec::new(),
    };
    let (ended_at, recorded, spent) = ctx.update(&scope, |target, now| {
        let run = find_run_mut(target, run_id)?;
        if run.ended_at.is_some() {
            return fail(format!("run {run_id} already ended"));
        }
        if let Some(open) = run.passes.iter().find(|pass| pass.ended_at.is_none()) {
            return fail(format!(
                "pass {run_id}.{} is still open; end it first",
                open.id
            ));
        }
        run.ended_at = Some(now.to_owned());
        run.repaired_head = Some(scope.head.clone());
        run.repair = repair;
        run.reveal = reveal;
        run.usage = usage;
        let spent = run_spend(run);
        Ok((now.to_owned(), adoption.apply(target), spent))
    })?;
    let pruned = prune_dir(&ctx.store, &ctx.dir, clock::now_secs()?)?;
    Ok(json!({
        "target": scope.key,
        "run": run_id,
        "ended_at": ended_at,
        "recorded": recorded,
        "repaired_head": scope.head,
        "pruned": pruned,
        "spent": spent,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReviewerInput {
    pub(super) id: Option<String>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) harness: Option<String>,
    pub(super) harness_version: Option<String>,
    pub(super) starting_context: Option<String>,
    pub(super) session: Option<String>,
}

pub(super) fn read_reviewers(source: Option<&Path>) -> Result<Vec<ReviewerInput>> {
    let Some(source) = source else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&read_source(source, "reviewers")?)
        .map_err(ctx("parse reviewers (expected a JSON array)"))
}

/// `manual` is the main session's own pass and `fix` the fixer, which signs fix replies.
pub(super) const SPECIAL_REVIEWERS: [&str; 2] = ["manual", "fix"];

pub(super) fn reviewer_number(id: &str) -> Option<u32> {
    id.strip_prefix('r')?.parse().ok()
}

pub(super) fn assign_reviewers(run: &Run, inputs: Vec<ReviewerInput>) -> Result<Vec<Reviewer>> {
    let known: Vec<&Reviewer> = run.passes.iter().flat_map(|pass| &pass.reviewers).collect();
    let mut next = known
        .iter()
        .filter_map(|reviewer| reviewer_number(&reviewer.id))
        .chain(
            inputs
                .iter()
                .filter_map(|input| input.id.as_deref().and_then(reviewer_number)),
        )
        .max()
        .unwrap_or(0);
    let mut reviewers: Vec<Reviewer> = Vec::new();
    for input in inputs {
        let id = match input.id {
            Some(id) => {
                if reviewer_number(&id).is_none() && !SPECIAL_REVIEWERS.contains(&id.as_str()) {
                    return fail(format!(
                        "reviewer id must look like r<n>, or be manual or fix: {id}"
                    ));
                }
                id
            }
            None => {
                next += 1;
                format!("r{next}")
            }
        };
        if reviewers.iter().any(|reviewer| reviewer.id == id) {
            return fail(format!("reviewer {id} is listed twice"));
        }
        let earlier = known.iter().rev().find(|reviewer| reviewer.id == id);
        let model = input
            .model
            .or_else(|| earlier.map(|reviewer| reviewer.model.clone()))
            .ok_or_else(|| Error(format!("reviewer {id} needs a model")))?;
        let harness = input
            .harness
            .or_else(|| earlier.map(|reviewer| reviewer.harness.clone()))
            .ok_or_else(|| Error(format!("reviewer {id} needs a harness")))?;
        reviewers.push(Reviewer {
            model,
            harness,
            effort: input
                .effort
                .or_else(|| earlier.map(|reviewer| reviewer.effort.clone()))
                .unwrap_or_else(|| "default".to_owned()),
            harness_version: input
                .harness_version
                .or_else(|| earlier.and_then(|reviewer| reviewer.harness_version.clone())),
            starting_context: input.starting_context.unwrap_or_else(|| "fresh".to_owned()),
            session: input
                .session
                .or_else(|| earlier.and_then(|reviewer| reviewer.session.clone())),
            usage: None,
            id,
        });
    }
    Ok(reviewers)
}

/// What a pass about to start records, computed before anything is written.
struct Plan {
    scope: Scope,
    commit: Option<String>,
    tree: String,
    base: Option<String>,
    partial: bool,
    commits: Vec<CommitInfo>,
    range_base: Option<String>,
    range_patch_id: Option<String>,
    head_patch_id: Option<String>,
}

fn plan(
    ctx: &Ctx,
    run_id: Option<&str>,
    kind: PassKind,
    base: Option<&str>,
    head: Option<&str>,
) -> Result<Plan> {
    let scope = ctx.scope_at(head)?;
    let (commit, tree) = ctx.reviewed_state(&scope)?;
    let previous = scope.existing.as_ref().and_then(|target| {
        target
            .runs
            .iter()
            .find(|run| Some(run.id.as_str()) == run_id)
            .and_then(|run| run.passes.last())
    });
    let previous_head = previous.map(|pass| pass.head.clone());
    let base = match (base, kind) {
        (Some(rev), _) => Some(ctx.repo.commit(rev)?),
        (None, PassKind::Full) => scope.base.clone(),
        (None, PassKind::FixCheck | PassKind::Reveal) => {
            previous_head.clone().or_else(|| scope.base.clone())
        }
    };
    // A pass that starts after the merge-base reviews only its own delta, unless it continues a
    // whole-branch pass from that pass's head. The default branch has no merge-base: its
    // reviews always start at an explicit or earlier head and cover the branch.
    let continues = previous.is_some_and(|pass| !pass.partial) && base == previous_head;
    let partial = base != scope.base && !continues && !ctx.is_default_branch(&scope)?;
    let range_base = if partial {
        base.clone()
    } else {
        scope.base.clone()
    };
    let commits = ctx.repo.commits(base.as_deref(), &scope.head)?;
    let range_patch_id = ctx.repo.range_patch_id(range_base.as_deref(), &tree)?;
    let head_patch_id = match (&commit, commits.last()) {
        (Some(_), Some(last)) if last.sha == scope.head => last.patch_id.clone(),
        _ => None,
    };
    Ok(Plan {
        scope,
        commit,
        tree,
        base,
        partial,
        commits,
        range_base,
        range_patch_id,
        head_patch_id,
    })
}

/// What earlier passes already reviewed of the commits `plan` covers.
fn overlap(ctx: &Ctx, plan: &Plan, run_id: Option<&str>, kind: PassKind) -> Result<Value> {
    already_reviewed(
        ctx,
        &Requested {
            scope: &plan.scope,
            run: run_id,
            kind,
            range_base: plan.range_base.as_deref(),
            base: plan.base.as_deref(),
            commits: &plan.commits,
            tree: &plan.tree,
            range_patch_id: plan.range_patch_id.as_deref(),
            committed: plan.commit.is_some(),
        },
    )
}

/// Fails when `run` cannot take a new pass.
fn check_startable(run: &Run, run_id: &str) -> Result<()> {
    if run.ended_at.is_some() {
        return fail(format!("run {run_id} already ended"));
    }
    if let Some(open) = run.passes.iter().find(|pass| pass.ended_at.is_none()) {
        return fail(format!(
            "pass {run_id}.{} is still open; end it first",
            open.id
        ));
    }
    Ok(())
}

/// `pass start --dry-run`: the report `pass start` adds, and nothing written.
pub(super) fn pass_dry_run(
    ctx: &Ctx,
    run_id: Option<&str>,
    kind: PassKind,
    base: Option<&str>,
    head: Option<&str>,
) -> Result<Value> {
    if run_id.is_none() && kind != PassKind::Full {
        return fail("--run is required unless --kind is full, which starts the run");
    }
    let plan = plan(ctx, run_id, kind, base, head)?;
    if let Some(run_id) = run_id {
        let run = plan
            .scope
            .existing
            .as_ref()
            .and_then(|target| target.runs.iter().find(|run| run.id == run_id))
            .ok_or_else(|| {
                Error(format!(
                    "run {run_id} not found in target {}",
                    plan.scope.key
                ))
            })?;
        check_startable(run, run_id)?;
    }
    let already_reviewed = overlap(ctx, &plan, run_id, kind)?;
    Ok(json!({
        "target": plan.scope.key,
        "run": run_id,
        "dry_run": true,
        "base": plan.base,
        "head": plan.scope.head,
        "partial": plan.partial,
        "commit_count": plan.commits.len(),
        "already_reviewed": already_reviewed,
    }))
}

pub(super) fn pass_start(
    ctx: &Ctx,
    run_id: &str,
    kind: PassKind,
    base: Option<&str>,
    reviewers: Option<&Path>,
    head: Option<&str>,
) -> Result<Value> {
    let inputs = read_reviewers(reviewers)?;
    let plan = plan(ctx, Some(run_id), kind, base, head)?;
    let already_reviewed = overlap(ctx, &plan, Some(run_id), kind)?;
    let Plan {
        scope,
        commit,
        tree,
        base,
        partial,
        commits,
        range_patch_id,
        head_patch_id,
        ..
    } = plan;
    let pass = ctx.update(&scope, |target, now| {
        let run = find_run_mut(target, run_id)?;
        check_startable(run, run_id)?;
        let (cycle, number) = match (kind, run.passes.last()) {
            (PassKind::Full, last) => (last.map_or(0, |pass| pass.cycle) + 1, 0),
            (_, Some(last)) => {
                let number = run
                    .passes
                    .iter()
                    .filter(|pass| pass.cycle == last.cycle)
                    .map(|pass| pass.number)
                    .max()
                    .unwrap_or(0);
                (last.cycle, number + 1)
            }
            (_, None) => return fail("a fix-check or reveal pass needs a full pass first"),
        };
        let reviewers = assign_reviewers(run, inputs)?;
        let pass = Pass {
            id: format!("c{cycle}p{number}"),
            cycle,
            number,
            kind,
            base,
            head: scope.head.clone(),
            tree,
            dirty: commit.is_none(),
            commits: commits
                .iter()
                .map(|commit| CommitPatch {
                    sha: commit.sha.clone(),
                    patch_id: commit.patch_id.clone(),
                })
                .collect(),
            range_patch_id,
            head_patch_id,
            partial,
            reviewers,
            started_at: now.to_owned(),
            ended_at: None,
            locked: false,
            major_found: false,
            killed: Vec::new(),
        };
        run.passes.push(pass.clone());
        Ok(pass)
    })?;
    Ok(json!({
        "target": scope.key,
        "run": run_id,
        "pass": pass_view(&pass),
        "already_reviewed": already_reviewed,
    }))
}

/// `pass abort`: deletes the open pass of a run, and the run when that leaves it without
/// passes. The pass counts for nothing.
pub(super) fn pass_abort(ctx: &Ctx, run_id: &str, pass_id: Option<&str>) -> Result<Value> {
    let scope = ctx.scope()?;
    let (pass, run_removed) = ctx.update(&scope, |target, _| {
        let run = find_run_mut(target, run_id)?;
        let index = match pass_id {
            Some(id) => {
                let index = run
                    .passes
                    .iter()
                    .position(|pass| pass.id == id)
                    .ok_or_else(|| pass_missing(run, id))?;
                if run.passes[index].ended_at.is_some() {
                    return fail(format!(
                        "pass {run_id}.{id} already ended; only an open pass can be aborted"
                    ));
                }
                index
            }
            None => run
                .passes
                .iter()
                .position(|pass| pass.ended_at.is_none())
                .ok_or_else(|| Error(format!("run {run_id} has no open pass")))?,
        };
        let id = run.passes[index].id.clone();
        // A fix-check reviewer can reopen a thread or reply while its pass is still open.
        let wrote = |thread: &Thread| {
            thread.comments.iter().any(|comment| {
                matches!(
                    &comment.author,
                    Author::Agent { run: Some(run), pass: Some(pass), .. }
                        if run == run_id && *pass == id
                )
            })
        };
        if target.threads.iter().any(|thread| {
            wrote(thread)
                || thread
                    .finding
                    .as_ref()
                    .is_some_and(|finding| finding.run == run_id && finding.pass == id)
        }) {
            return fail(format!(
                "pass {run_id}.{id} has findings or replies and cannot be aborted"
            ));
        }
        let run = find_run_mut(target, run_id)?;
        let pass = run.passes.remove(index);
        let empty = run.passes.is_empty();
        if empty {
            target.runs.retain(|known| known.id != run_id);
        }
        Ok((pass, empty))
    })?;
    Ok(json!({
        "target": scope.key,
        "run": run_id,
        "aborted": pass.id,
        "run_removed": run_removed,
    }))
}

/// A pass without its per-commit patch-ids, which only coverage needs; `show` prints them.
pub(super) fn pass_view(pass: &Pass) -> Value {
    let mut view = json!(pass);
    if let Some(fields) = view.as_object_mut() {
        fields.remove("commits");
        fields.insert("commit_count".to_owned(), json!(pass.commits.len()));
    }
    view
}

pub(super) fn pass_end(
    ctx: &Ctx,
    run_id: &str,
    pass_id: &str,
    lock: bool,
    findings: Option<&Path>,
    reviewer: Option<&str>,
) -> Result<Value> {
    let scope = ctx.scope()?;
    let (findings, usage) = match findings {
        Some(source) => read_pass_input(source)?,
        None => (Vec::new(), Vec::new()),
    };
    // Group by reviewer, keeping input order, so each reviewer's ids number in order; the
    // output lists ids in input order.
    let mut groups: Vec<(String, Vec<usize>, Vec<FindingInput>)> = Vec::new();
    let mut is_killed = Vec::new();
    for (index, mut finding) in findings.into_iter().enumerate() {
        let id = finding
            .reviewer
            .take()
            .or_else(|| reviewer.map(str::to_owned))
            .ok_or_else(|| {
                Error(format!(
                    "finding {index}: no reviewer; add one or --reviewer"
                ))
            })?;
        is_killed.push(finding.reason.is_some());
        match groups.iter_mut().find(|(known, _, _)| *known == id) {
            Some((_, indexes, group)) => {
                indexes.push(index);
                group.push(finding);
            }
            None => groups.push((id, vec![index], vec![finding])),
        }
    }
    let (pass, added, killed) = ctx.update(&scope, |target, now| {
        let mut ids = vec![String::new(); is_killed.len()];
        for (reviewer, indexes, group) in groups {
            let (new, dead) = add_findings(target, run_id, pass_id, &reviewer, group, now)?;
            let (mut new, mut dead) = (new.into_iter(), dead.into_iter());
            for index in indexes {
                let next = if is_killed[index] {
                    dead.next()
                } else {
                    new.next()
                };
                ids[index] = next.unwrap_or_default();
            }
        }
        let (killed, added): (Vec<_>, Vec<_>) = ids
            .into_iter()
            .enumerate()
            .partition(|(index, _)| is_killed[*index]);
        let added: Vec<String> = added.into_iter().map(|(_, id)| id).collect();
        let killed: Vec<String> = killed.into_iter().map(|(_, id)| id).collect();
        let major = major_found(target, run_id, pass_id);
        let pass = find_pass_mut(target, run_id, pass_id)?;
        if pass.ended_at.is_some() {
            return fail(format!("pass {run_id}.{pass_id} already ended"));
        }
        for spent in &usage {
            let id = spent.who("reviewer")?;
            let reviewer = pass
                .reviewers
                .iter_mut()
                .find(|reviewer| reviewer.id == id)
                .ok_or_else(|| Error(format!("usage: reviewer {id} is not in pass {pass_id}")))?;
            reviewer.usage = Some(spent.usage());
        }
        pass.ended_at = Some(now.to_owned());
        pass.locked = lock;
        pass.major_found = major;
        Ok((pass.clone(), added, killed))
    })?;
    Ok(json!({
        "target": scope.key,
        "run": run_id,
        "pass": pass_view(&pass),
        "added": added,
        "killed": killed,
    }))
}

/// `pass end --findings` input: a findings array, or `{"findings": [...], "usage": [...]}`.
pub(super) fn read_pass_input(source: &Path) -> Result<(Vec<FindingInput>, Vec<UsageInput>)> {
    let value: Value = serde_json::from_str(&read_source(source, "findings")?)
        .map_err(ctx("parse findings JSON"))?;
    match value {
        Value::Object(mut fields)
            if fields.contains_key("findings") || fields.contains_key("usage") =>
        {
            if let Some(other) = fields
                .keys()
                .find(|key| *key != "findings" && *key != "usage")
            {
                return fail(format!(
                    "unknown field {other}; expected findings and usage"
                ));
            }
            let findings = parse_findings(fields.remove("findings").unwrap_or(json!([])), true)?;
            let usage = serde_json::from_value(fields.remove("usage").unwrap_or(json!([])))
                .map_err(ctx("parse usage"))?;
            Ok((findings, usage))
        }
        value => Ok((parse_findings(value, true)?, Vec::new())),
    }
}
