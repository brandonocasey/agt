//! `agt qa`: QA runs and their checks (what was tested, where, and the outcome), the threads a
//! check found, and the QA approval. They live in the review target, so threads, `status` and
//! `publish` are shared with the review.

use std::collections::HashSet;

use serde_json::{Value, json};

use super::approve::is_open;
use super::findings::{self, NewThread, author, issue_key, local_thread};
use super::input::{limited, read_body};
use super::{
    Ctx, Selection, auto, fresh_id, open_ctx, publish, run_not_found, run_of_id, usage, view,
};
use crate::core::error::{Error, Result, fail};
use crate::core::output::Output;
use crate::core::text::argument_or_stdin;
use crate::forge::auto::Offline;
use crate::review::cli::{AuthorArgs, CheckArgs, QaCli, QaCommand, QaFindingArgs, QaRunAction};
use crate::store::model::{
    Author, Check, Outcome, QaApproval, QaRun, Target, Thread, ThreadType, last_number,
};

const QA_ID_PREFIX: &str = "qa-";
const QA_ID_CHARS: usize = 4;

pub fn run_qa(cli: QaCli) -> Result<Output> {
    let selection = Selection {
        target: cli.target,
        range: cli.range,
        default_branch: cli.default_branch,
    };
    let reading = matches!(
        cli.command,
        QaCommand::List | QaCommand::Show { .. } | QaCommand::Status { .. }
    );
    let preview = matches!(cli.command, QaCommand::Publish { dry_run: true, .. });
    let publishing = matches!(cli.command, QaCommand::Publish { .. });
    if publishing {
        Offline::current().forbid("qa publish")?;
    }
    // Publish writes no git objects and needs `merge-tree` for its records' coverage.
    let mut ctx = open_ctx(
        selection,
        (reading || preview, reading || publishing, preview),
        run_hint(&cli.command),
    )?;
    if reading {
        auto::sync_stale(&ctx, Offline::current())?;
    }
    let value = match cli.command {
        QaCommand::Run {
            action: QaRunAction::Start { env, tester },
        } => run_start(&ctx, env, &tester)?,
        QaCommand::Run {
            action: QaRunAction::End { run, usage },
        } => {
            let usage = usage::read_role_usage_opt(usage.as_deref())?;
            let mut ended = run_end(&ctx, &run, usage)?;
            auto::publish_ended(&mut ctx, &run, Offline::current());
            ended["status"] = status(&ctx, None)?;
            ended
        }
        QaCommand::Check(args) => check(&ctx, args)?,
        QaCommand::Finding(args) => finding(&ctx, &args)?,
        QaCommand::Comment(args) => {
            findings::comment(&ctx, &args.comment, true, args.check.as_deref())?
        }
        QaCommand::Reply(args) => findings::reply(&ctx, &args, true)?,
        QaCommand::List => list(&ctx)?,
        QaCommand::Show { id } => view::show(&ctx, &id)?,
        QaCommand::Status { head } => status(&ctx, head.as_deref())?,
        QaCommand::Approve => approve(&ctx)?,
        QaCommand::Publish {
            dry_run,
            not_own,
            only,
            pr,
        } => publish::publish(
            &mut ctx,
            pr.as_deref(),
            &publish::Options {
                dry_run,
                not_own,
                only,
                approval: None,
                run: None,
            },
        )?,
    };
    Ok(Output::Json(value))
}

/// The QA run a command names, through `--run` or an id that starts with one (`qa-xxxx...`).
fn run_hint(command: &QaCommand) -> Option<String> {
    let from_id = |id: &str| run_of_id(id, is_qa_run_id);
    match command {
        QaCommand::Run {
            action: QaRunAction::End { run, .. },
        } => Some(run.clone()),
        QaCommand::Check(args) => Some(args.run.clone()),
        QaCommand::Finding(args) => from_id(&args.check),
        QaCommand::Comment(args) => args.check.as_deref().and_then(from_id),
        QaCommand::Reply(args) => from_id(&args.id),
        QaCommand::Show { id } => from_id(id),
        _ => None,
    }
}

fn is_qa_run_id(text: &str) -> bool {
    text.len() == QA_ID_PREFIX.len() + QA_ID_CHARS && text.starts_with(QA_ID_PREFIX)
}

/// The latest check of one name, and whether it ran against the state being evaluated.
pub(super) struct CheckState<'a> {
    pub(super) run: &'a QaRun,
    pub(super) check: &'a Check,
    pub(super) current: bool,
}

/// The latest check of each name, in the order the names were first tested. A check is current
/// when it ran against `tree`, so a rebase that changes no content keeps it.
pub(super) fn latest<'a>(runs: &[&'a QaRun], tree: Option<&str>) -> Vec<CheckState<'a>> {
    let mut latest: Vec<CheckState> = Vec::new();
    for run in runs {
        for check in &run.checks {
            let state = CheckState {
                run,
                check,
                current: tree == Some(check.tree.as_str()),
            };
            match latest
                .iter_mut()
                .find(|known| known.check.name == check.name)
            {
                Some(known) if known.check.recorded_at <= check.recorded_at => *known = state,
                Some(_) => {}
                None => latest.push(state),
            }
        }
    }
    latest
}

/// Threads a QA check found.
pub(super) fn qa_threads(target: &Target) -> impl Iterator<Item = &Thread> {
    target
        .threads
        .iter()
        .filter(|thread| thread.check.is_some())
}

/// The run's environment with the check's facts added, a check's value replacing the run's.
pub(super) fn environment(run: &QaRun, check: &Check) -> Vec<String> {
    let key = |fact: &String| {
        fact.split_once('=')
            .map_or(fact.clone(), |(key, _)| key.to_owned())
    };
    let mut facts: Vec<String> = run
        .env
        .iter()
        .filter(|fact| !check.env.iter().any(|own| key(own) == key(fact)))
        .cloned()
        .collect();
    facts.extend(check.env.iter().cloned());
    facts
}

/// Why QA cannot approve `tree`: no checks, a failing, blocked or stale latest check, or an open
/// blocking thread a check found.
fn blockers(target: &Target, tree: Option<&str>) -> Vec<String> {
    let runs: Vec<&QaRun> = target.qa_runs.iter().collect();
    let states = latest(&runs, tree);
    let mut reasons = Vec::new();
    if states.is_empty() {
        reasons.push("no checks recorded".to_owned());
    }
    for state in &states {
        let check = state.check;
        if check.outcome.blocks_approval() {
            reasons.push(format!(
                "{} {}: {}",
                check.id,
                check.outcome.as_str(),
                check.name
            ));
        } else if !state.current {
            reasons.push(format!("{} is stale: {}", check.id, check.name));
        }
    }
    for thread in
        qa_threads(target).filter(|thread| is_open(thread) && thread.kind.blocks_approval())
    {
        reasons.push(format!("{} is open", thread.id));
    }
    reasons
}

fn check_view(state: &CheckState) -> Value {
    let check = state.check;
    json!({
        "id": check.id,
        "name": check.name,
        "kind": check.kind,
        "outcome": check.outcome,
        "current": state.current,
        "head": check.head,
        "env": environment(state.run, check),
        "evidence": check.evidence,
        "note": check.note,
        "recorded_at": check.recorded_at,
    })
}

fn thread_view(thread: &Thread) -> Value {
    json!({
        "id": thread.id,
        "type": thread.kind,
        "severity": thread.severity,
        "check": thread.check,
        "open": is_open(thread),
        "resolution": thread.resolution,
        "body": thread.comments.first().map(|first| first.body.as_str()),
    })
}

/// The QA state of `target` at `head` (with tree `tree`); `null` when nothing was tested.
pub(super) fn qa_state(target: Option<&Target>, head: &str, tree: Option<&str>) -> Value {
    let Some(target) = target.filter(|target| !target.qa_runs.is_empty()) else {
        return Value::Null;
    };
    let runs: Vec<&QaRun> = target.qa_runs.iter().collect();
    let states = latest(&runs, tree);
    let tally = |outcome: Outcome| {
        states
            .iter()
            .filter(|state| state.check.outcome == outcome)
            .count()
    };
    let blockers = blockers(target, tree);
    json!({
        "checks": states.iter().map(check_view).collect::<Vec<_>>(),
        "pass": tally(Outcome::Pass),
        "fail": tally(Outcome::Fail),
        "blocked": tally(Outcome::Blocked),
        "skipped": tally(Outcome::Skipped),
        "stale": states.iter().filter(|state| !state.current).count(),
        "open_threads": qa_threads(target)
            .filter(|thread| is_open(thread))
            .map(|thread| thread.id.as_str())
            .collect::<Vec<_>>(),
        "approvable": blockers.is_empty(),
        "blockers": blockers,
        "approved": target
            .qa_runs
            .iter()
            .any(|run| run.approval.as_ref().is_some_and(|approval| approval.head == head)),
        "runs": target
            .qa_runs
            .iter()
            .map(|run| json!({ "id": run.id, "ended": run.ended_at.is_some() }))
            .collect::<Vec<_>>(),
    })
}

/// `key=value` facts, each with a key.
fn facts(env: Vec<String>) -> Result<Vec<String>> {
    env.into_iter()
        .map(|fact| match fact.split_once('=') {
            Some((key, _)) if !key.trim().is_empty() => limited(fact, "env"),
            _ => fail(format!("--env must be key=value: {fact}")),
        })
        .collect()
}

/// A tester or finder: the flags' author, an agent tied to `run` so its run's publish posts its
/// threads.
/// The QA run holding check `check`.
pub(super) fn check_run<'a>(target: &'a Target, check: &str) -> Result<&'a QaRun> {
    target
        .qa_runs
        .iter()
        .find(|run| run.checks.iter().any(|known| known.id == check))
        .ok_or_else(|| Error(format!("check {check} not found in target {}", target.key)))
}

pub(super) fn qa_author(
    args: &AuthorArgs,
    target: &Target,
    default: Option<Author>,
    run: Option<&str>,
) -> Result<Author> {
    if args.reviewer.is_some() {
        return fail("--reviewer names a review pass reviewer; QA takes --model and --harness");
    }
    let mut found = author(args, target, default)?;
    if let (Author::Agent { run: owner, .. }, Some(run)) = (&mut found, run) {
        *owner = Some(run.to_owned());
    }
    Ok(found)
}

fn run_start(ctx: &Ctx, env: Vec<String>, tester: &AuthorArgs) -> Result<Value> {
    let scope = ctx.scope()?;
    let env = facts(env)?;
    let default = ctx.default_author()?;
    // QA run ids are unique in the repository, so `--run` alone finds the target.
    let taken: HashSet<String> = ctx
        .store
        .targets(&ctx.dir)?
        .into_iter()
        .flat_map(|target| target.qa_runs.into_iter().map(|run| run.id))
        .collect();
    let run = ctx.update(&scope, |target, now| {
        let id = fresh_id(
            &taken,
            |id| target.qa_runs.iter().any(|run| run.id == id),
            || crate::core::random::token(QA_ID_PREFIX, QA_ID_CHARS),
        );
        let run = QaRun {
            tester: qa_author(tester, target, default, Some(&id))?,
            id,
            started_at: now.to_owned(),
            ended_at: None,
            env,
            checks: Vec::new(),
            record: None,
            approval: None,
            publication_target: None,
            usage: Vec::new(),
        };
        target.qa_runs.push(run.clone());
        Ok(run)
    })?;
    Ok(json!({ "target": scope.key, "run": run.id, "tester": run.tester }))
}

fn find_qa_run_mut<'a>(target: &'a mut Target, id: &str) -> Result<&'a mut QaRun> {
    match target.qa_runs.iter().position(|run| run.id == id) {
        Some(index) => Ok(&mut target.qa_runs[index]),
        None => {
            let open = target
                .qa_runs
                .iter()
                .filter(|run| run.ended_at.is_none())
                .map(|run| run.id.as_str());
            Err(run_not_found("QA run", id, target, open))
        }
    }
}

fn run_end(ctx: &Ctx, id: &str, usage: Vec<crate::store::model::RoleUsage>) -> Result<Value> {
    let scope = ctx.scope()?;
    let run = ctx.update(&scope, |target, now| {
        let run = find_qa_run_mut(target, id)?;
        if run.ended_at.is_some() {
            return fail(format!("QA run {id} already ended"));
        }
        run.ended_at = Some(now.to_owned());
        run.usage = usage;
        Ok(run.clone())
    })?;
    Ok(json!({ "target": scope.key, "run": run }))
}

fn check(ctx: &Ctx, args: CheckArgs) -> Result<Value> {
    let scope = ctx.scope()?;
    if !scope.head_known {
        return fail("a check needs a head: check out the branch or pass --target");
    }
    let name = limited(args.name.trim().to_owned(), "name")?;
    if name.is_empty() {
        return fail("--name must not be empty");
    }
    let steps = match &args.steps {
        Some(steps) => Some(limited(argument_or_stdin(steps, "steps")?, "steps")?),
        None => None,
    };
    let command = args
        .command
        .map(|command| limited(command, "command"))
        .transpose()?;
    let note = args.note.map(|note| limited(note, "note")).transpose()?;
    let evidence = args
        .evidence
        .into_iter()
        .map(|item| limited(item, "evidence"))
        .collect::<Result<Vec<_>>>()?;
    let env = facts(args.env)?;
    let (head, tree) = ctx.reviewed_state(&scope)?;
    let check = ctx.update(&scope, |target, now| {
        let run = find_qa_run_mut(target, &args.run)?;
        if run.ended_at.is_some() {
            return fail(format!("QA run {} already ended", run.id));
        }
        let prefix = format!("{}.k", run.id);
        let number = last_number(run.checks.iter().map(|check| &check.id), &prefix) + 1;
        let check = Check {
            id: format!("{prefix}{number}"),
            name,
            kind: args.kind,
            steps,
            command,
            env,
            outcome: args.outcome,
            evidence,
            note,
            head,
            tree,
            recorded_at: now.to_owned(),
        };
        run.checks.push(check.clone());
        Ok(check)
    })?;
    Ok(json!({ "target": scope.key, "check": check }))
}

fn finding(ctx: &Ctx, args: &QaFindingArgs) -> Result<Value> {
    let scope = ctx.scope()?;
    if args.kind == ThreadType::Bug && args.severity.is_none() {
        return fail("a bug needs --severity");
    }
    let body = read_body(&args.body)?;
    let issue = args
        .issue
        .as_deref()
        .map(|text| issue_key(ctx, text))
        .transpose()?;
    let default = ctx.default_author()?;
    let thread = ctx.update(&scope, |target, now| {
        let run = check_run(target, &args.check)?;
        let finder = if args.author.is_set() {
            qa_author(&args.author, target, default, Some(&run.id))?
        } else {
            run.tester.clone()
        };
        let mut thread = local_thread(
            NewThread {
                id: target.next_thread_id(),
                kind: args.kind,
                severity: args.severity,
                anchor: None,
                finding: None,
                qa: true,
            },
            finder,
            body,
            now,
        );
        thread.check = Some(args.check.clone());
        thread.issue = issue;
        target.threads.push(thread.clone());
        Ok(thread)
    })?;
    Ok(json!({ "target": scope.key, "thread": thread }))
}

/// The tree checks are judged against: `--head`'s, else the work tree of a checked-out branch.
fn evaluated(ctx: &Ctx, head: Option<&str>) -> Result<Option<(String, String)>> {
    if let Some(rev) = head {
        let head = ctx.repo.commit(rev)?;
        let tree = ctx.repo.tree_of(&head)?;
        return Ok(Some((head, tree)));
    }
    let scope = ctx.scope()?;
    if !scope.head_known {
        return Ok(None);
    }
    let (_, tree) = ctx.reviewed_state(&scope)?;
    Ok(Some((scope.head, tree)))
}

fn status(ctx: &Ctx, head: Option<&str>) -> Result<Value> {
    let scope = ctx.scope()?;
    let evaluated = evaluated(ctx, head)?;
    let (head, tree) = match &evaluated {
        Some((head, tree)) => (head.as_str(), Some(tree.as_str())),
        None => ("", None),
    };
    Ok(json!({
        "target": scope.existing.as_ref().map(|_| &scope.key),
        "head": (!head.is_empty()).then_some(head),
        "qa": qa_state(scope.existing.as_ref(), head, tree),
    }))
}

fn list(ctx: &Ctx) -> Result<Value> {
    let scope = ctx.scope()?;
    let tree = evaluated(ctx, None)?.map(|(_, tree)| tree);
    let Some(target) = scope.existing.as_ref() else {
        return Ok(json!({ "target": null, "checks": [], "threads": [] }));
    };
    let runs: Vec<&QaRun> = target.qa_runs.iter().collect();
    Ok(json!({
        "target": scope.key,
        "checks": latest(&runs, tree.as_deref()).iter().map(check_view).collect::<Vec<_>>(),
        "threads": qa_threads(target).map(thread_view).collect::<Vec<_>>(),
    }))
}

fn approve(ctx: &Ctx) -> Result<Value> {
    let scope = ctx.scope()?;
    if !scope.head_known {
        return fail("approve needs a head: check out the branch or pass --target");
    }
    let (commit, tree) = ctx.reviewed_state(&scope)?;
    let Some(head) = commit else {
        return fail("the work tree has uncommitted changes; QA approves a commit");
    };
    let blockers = scope.existing.as_ref().map_or_else(
        || vec!["no checks recorded".to_owned()],
        |target| blockers(target, Some(&tree)),
    );
    if !blockers.is_empty() {
        return fail(format!("QA approval is blocked: {}", blockers.join("; ")));
    }
    let approval = ctx.update(&scope, |target, now| {
        let run = target
            .qa_runs
            .last_mut()
            .ok_or_else(|| Error("no QA run to approve".into()))?;
        let approval = QaApproval {
            head: head.clone(),
            approved_at: now.to_owned(),
        };
        run.approval = Some(approval.clone());
        Ok(json!({ "run": run.id, "approval": approval }))
    })?;
    Ok(json!({ "target": scope.key, "approved": approval }))
}
