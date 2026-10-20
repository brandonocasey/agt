use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::core::error::{Error, Result, ctx, fail};
use crate::core::local::read_source;
use crate::forge;
use crate::review::cli::{AuthorArgs, CommentArgs, ReplyArgs, ResolveArgs};
use crate::store::model::{
    Anchor, Author, Comment, FindingInfo, KilledFinding, PostState, Resolution, Review, Severity,
    Side, Target, Thread, ThreadType, Triage, last_number,
};

use super::input::{AnchorInput, build_anchor, limited, read_body};
use super::{Ctx, find_pass, find_pass_mut, find_thread_mut, major_found, passes};

/// A comment written here, numbered after the thread's other local comments.
pub(super) fn local_comment(
    thread: &str,
    existing: &[Comment],
    author: Author,
    body: String,
    now: &str,
) -> Comment {
    let prefix = format!("{thread}/c");
    let number = last_number(existing.iter().map(|comment| &comment.id), &prefix) + 1;
    Comment::new(
        format!("{prefix}{number}"),
        author,
        body,
        now.to_owned(),
        PostState::Pending,
        None,
    )
}

pub(super) struct NewThread {
    pub(super) id: String,
    pub(super) kind: ThreadType,
    pub(super) severity: Option<Severity>,
    pub(super) anchor: Option<Anchor>,
    pub(super) finding: Option<FindingInfo>,
}

pub(super) fn local_thread(new: NewThread, author: Author, body: String, now: &str) -> Thread {
    let first = local_comment(&new.id, &[], author, body, now);
    Thread {
        severity: new.severity,
        anchor: new.anchor,
        finding: new.finding,
        ..Thread::new(
            new.id,
            new.kind,
            vec![first],
            now.to_owned(),
            now.to_owned(),
        )
    }
}

pub(super) fn author(
    args: &AuthorArgs,
    target: &Target,
    default: Option<Author>,
) -> Result<Author> {
    let agent = args.model.is_some()
        || args.effort.is_some()
        || args.harness.is_some()
        || args.harness_version.is_some()
        || args.run.is_some()
        || args.pass.is_some()
        || args.reviewer.is_some()
        || args.session.is_some();
    if let Some(login) = &args.author {
        if agent {
            return fail("--author names a human; drop the agent flags");
        }
        return Ok(Author::Human {
            login: limited(login.clone(), "author")?,
            git_name: false,
        });
    }
    if !agent {
        return default
            .ok_or_else(|| Error("no author: pass --author or set git user.name".into()));
    }
    let stored = match (&args.run, &args.pass, &args.reviewer) {
        (Some(run), Some(pass), Some(id)) => find_pass(target, run, pass)?
            .reviewers
            .iter()
            .find(|reviewer| reviewer.id == *id),
        (_, _, Some(_)) => return fail("--reviewer needs --run and --pass"),
        _ => None,
    };
    let model = args
        .model
        .clone()
        .or_else(|| stored.map(|reviewer| reviewer.model.clone()))
        .ok_or_else(|| Error("an agent author needs --model, or a stored --reviewer".into()))?;
    let harness = args
        .harness
        .clone()
        .or_else(|| stored.map(|reviewer| reviewer.harness.clone()))
        .ok_or_else(|| Error("an agent author needs --harness, or a stored --reviewer".into()))?;
    Ok(Author::Agent {
        model,
        effort: args
            .effort
            .clone()
            .or_else(|| stored.map(|reviewer| reviewer.effort.clone()))
            .unwrap_or_else(|| "default".to_owned()),
        harness,
        harness_version: args
            .harness_version
            .clone()
            .or_else(|| stored.and_then(|reviewer| reviewer.harness_version.clone())),
        run: args.run.clone(),
        pass: args.pass.clone(),
        reviewer: args.reviewer.clone(),
        session: args
            .session
            .clone()
            .or_else(|| stored.and_then(|reviewer| reviewer.session.clone())),
    })
}

/// Moves a surviving finding to its pass's killed list. A locked pass allows this: the lock
/// freezes what reviewers reported, not the verdict.
pub(super) fn finding_kill(ctx: &Ctx, id: &str, reason: &str) -> Result<Value> {
    let scope = ctx.scope()?;
    if scope.existing.is_none() {
        return fail(format!("finding {id} not found"));
    }
    let reason = limited(reason.to_owned(), "reason")?;
    let killed = ctx.update(&scope, |target, now| {
        let index = target
            .threads
            .iter()
            .position(|thread| thread.id == id && thread.finding.is_some())
            .ok_or_else(|| Error(format!("finding {id} not found")))?;
        let thread = &target.threads[index];
        let sent = thread.remote.is_some()
            || thread
                .comments
                .iter()
                .any(|comment| comment.delivery.post != PostState::Pending);
        if sent {
            return fail(format!(
                "finding {id} is posted or being posted; resolve it with --triage stale instead"
            ));
        }
        let thread = target.threads.remove(index);
        let finding = thread
            .finding
            .ok_or_else(|| Error(format!("finding {id} not found")))?;
        let killed = KilledFinding {
            id: thread.id,
            kind: thread.kind,
            severity: thread.severity,
            anchor: thread.anchor,
            scenario: thread
                .comments
                .first()
                .map(|first| first.body.clone())
                .unwrap_or_default(),
            evidence: finding.evidence,
            tests_run: finding.tests_run,
            post_reveal: finding.post_reveal,
            reason,
            created_at: now.to_owned(),
        };
        let major = major_found(target, &finding.run, &finding.pass);
        let pass = find_pass_mut(target, &finding.run, &finding.pass)?;
        pass.killed.push(killed.clone());
        pass.major_found = major;
        Ok(killed)
    })?;
    Ok(json!({ "target": scope.key, "killed": killed }))
}

/// One reviewer finding as `finding add` reads it; see README "Finding input".
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FindingInput {
    #[serde(rename = "type")]
    pub(super) kind: ThreadType,
    pub(super) severity: Option<Severity>,
    pub(super) path: Option<String>,
    pub(super) line: Option<u32>,
    pub(super) old_line: Option<u32>,
    pub(super) side: Option<Side>,
    pub(super) start_line: Option<u32>,
    #[serde(alias = "body")]
    pub(super) scenario: String,
    pub(super) evidence: Option<String>,
    #[serde(default)]
    pub(super) tests_run: Vec<String>,
    #[serde(default)]
    pub(super) post_reveal: bool,
    #[serde(default)]
    pub(super) killed: bool,
    pub(super) reason: Option<String>,
    /// Reviewer id, for `pass end --findings`; `finding add` takes `--reviewer`.
    pub(super) reviewer: Option<String>,
}

/// Findings from `source` (`-` for stdin); `allow_empty` accepts `[]` (a clean pass).
pub(super) fn read_findings(source: &Path, allow_empty: bool) -> Result<Vec<FindingInput>> {
    let value: Value = serde_json::from_str(&read_source(source, "findings")?)
        .map_err(ctx("parse findings JSON"))?;
    parse_findings(value, allow_empty)
}

pub(super) fn parse_findings(value: Value, allow_empty: bool) -> Result<Vec<FindingInput>> {
    let items = match value {
        Value::Array(items) => items,
        item @ Value::Object(_) => vec![item],
        _ => return fail("findings must be a JSON object or array"),
    };
    if items.is_empty() && !allow_empty {
        return fail("no findings in input");
    }
    let mut findings = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        let finding: FindingInput =
            serde_json::from_value(item).map_err(ctx(format!("finding {index}")))?;
        if finding.scenario.trim().is_empty() {
            return fail(format!("finding {index}: scenario must not be empty"));
        }
        if finding.kind == ThreadType::Bug && finding.severity.is_none() {
            return fail(format!("finding {index}: a bug needs a severity"));
        }
        match (&finding.reason, finding.killed) {
            (None, true) => {
                return fail(format!("finding {index}: a killed finding needs a reason"));
            }
            (Some(_), false) => {
                return fail(format!(
                    "finding {index}: reason is only for killed findings"
                ));
            }
            (Some(reason), true) if reason.trim().is_empty() => {
                return fail(format!("finding {index}: reason must not be empty"));
            }
            _ => {}
        }
        findings.push(finding);
    }
    Ok(findings)
}

pub(super) fn finding_add(
    ctx: &Ctx,
    run_id: &str,
    pass_id: &str,
    reviewer_id: &str,
) -> Result<Value> {
    let scope = ctx.scope()?;
    let findings = read_findings(Path::new("-"), false)?;
    if let Some(index) = findings.iter().position(|finding| {
        finding
            .reviewer
            .as_deref()
            .is_some_and(|id| id != reviewer_id)
    }) {
        return fail(format!(
            "finding {index}: its reviewer differs from --reviewer {reviewer_id}"
        ));
    }
    let (added, killed) = ctx.update(&scope, |target, now| {
        add_findings(target, run_id, pass_id, reviewer_id, findings, now)
    })?;
    Ok(json!({ "target": scope.key, "added": added, "killed": killed }))
}

/// Stores one reviewer's findings on a pass; returns the new thread and killed ids.
pub(super) fn add_findings(
    target: &mut Target,
    run_id: &str,
    pass_id: &str,
    reviewer_id: &str,
    findings: Vec<FindingInput>,
    now: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    {
        let run_ended = target
            .runs
            .iter()
            .any(|run| run.id == run_id && run.ended_at.is_some());
        let pass = find_pass(target, run_id, pass_id)?;
        if run_ended {
            return fail(format!("run {run_id} already ended"));
        }
        if pass.locked {
            return fail(format!("pass {run_id}.{pass_id} is locked"));
        }
        let reviewer = pass
            .reviewers
            .iter()
            .find(|reviewer| reviewer.id == reviewer_id)
            .ok_or_else(|| {
                let known: Vec<&str> = pass.reviewers.iter().map(|r| r.id.as_str()).collect();
                Error(format!(
                    "reviewer {reviewer_id} is not in pass {pass_id}; its reviewers: {}",
                    known.join(", ")
                ))
            })?;
        let commit = (!pass.dirty).then(|| pass.head.clone());
        let tree = pass.tree.clone();
        let agent = Author::Agent {
            model: reviewer.model.clone(),
            effort: reviewer.effort.clone(),
            harness: reviewer.harness.clone(),
            harness_version: reviewer.harness_version.clone(),
            run: Some(run_id.to_owned()),
            pass: Some(pass_id.to_owned()),
            reviewer: Some(reviewer_id.to_owned()),
            session: reviewer.session.clone(),
        };
        let prefix = format!("{run_id}.{pass_id}.{reviewer_id}.f");
        let ids = target
            .threads
            .iter()
            .map(|thread| thread.id.clone())
            .chain(passes(target).flat_map(|pass| pass.killed.iter().map(|f| f.id.clone())));
        let mut number = last_number(ids, &prefix);
        let mut threads = Vec::new();
        let mut killed = Vec::new();
        for finding in findings {
            number += 1;
            let id = format!("{prefix}{number}");
            let anchor = build_anchor(
                &AnchorInput {
                    path: finding.path.as_deref(),
                    line: finding.line,
                    old_line: finding.old_line,
                    side: finding.side,
                    start_line: finding.start_line,
                },
                commit.clone(),
                tree.clone(),
            )
            .map_err(|error| Error(format!("finding {id}: {error}")))?;
            if let Some(reason) = finding.reason {
                killed.push(KilledFinding {
                    id,
                    kind: finding.kind,
                    severity: finding.severity,
                    anchor,
                    scenario: finding.scenario,
                    evidence: finding.evidence,
                    tests_run: finding.tests_run,
                    post_reveal: finding.post_reveal,
                    reason,
                    created_at: now.to_owned(),
                });
                continue;
            }
            threads.push(local_thread(
                NewThread {
                    id,
                    kind: finding.kind,
                    severity: finding.severity,
                    anchor,
                    finding: Some(FindingInfo {
                        run: run_id.to_owned(),
                        pass: pass_id.to_owned(),
                        reviewer: reviewer_id.to_owned(),
                        evidence: finding.evidence,
                        tests_run: finding.tests_run,
                        post_reveal: finding.post_reveal,
                    }),
                },
                agent.clone(),
                finding.scenario,
                now,
            ));
        }
        let added: Vec<String> = threads.iter().map(|thread| thread.id.clone()).collect();
        let killed_ids: Vec<String> = killed.iter().map(|finding| finding.id.clone()).collect();
        target.threads.extend(threads);
        let major = major_found(target, run_id, pass_id);
        let pass = find_pass_mut(target, run_id, pass_id)?;
        pass.killed.extend(killed);
        pass.major_found = major;
        Ok((added, killed_ids))
    }
}

pub(super) fn comment(ctx: &Ctx, args: &CommentArgs) -> Result<Value> {
    let scope = ctx.scope()?;
    let body = read_body(&args.body)?;
    let anchor_input = AnchorInput {
        path: args.path.as_deref(),
        line: args.line,
        old_line: args.old_line,
        side: args.side,
        start_line: args.start_line,
    };
    let anchor = if anchor_input.path.is_some() {
        let (commit, tree) = match &args.commit {
            Some(rev) => {
                let commit = ctx.repo.commit(rev)?;
                let tree = ctx.repo.tree_of(&commit)?;
                (Some(commit), tree)
            }
            None => ctx.reviewed_state(&scope)?,
        };
        let anchor = build_anchor(&anchor_input, commit, tree)?;
        if let Some(anchor) = &anchor
            && anchor.side == Side::New
        {
            let state = anchor.commit.as_deref().unwrap_or(&anchor.tree);
            if !ctx
                .repo
                .object_exists(&format!("{state}:{}", anchor.path))?
            {
                return fail(format!("{} does not exist at {state}", anchor.path));
            }
        }
        anchor
    } else {
        build_anchor(&anchor_input, None, String::new())?
    };
    let default = ctx.default_author()?;
    let thread = ctx.update(&scope, |target, now| {
        let author = author(&args.author, target, default)?;
        let thread = local_thread(
            NewThread {
                id: target.next_thread_id(),
                kind: args.kind,
                severity: args.severity,
                anchor,
                finding: None,
            },
            author,
            body,
            now,
        );
        target.threads.push(thread.clone());
        Ok(thread)
    })?;
    Ok(json!({ "target": scope.key, "thread": thread }))
}

pub(super) fn reply(ctx: &Ctx, args: &ReplyArgs) -> Result<Value> {
    let scope = ctx.scope()?;
    if scope.existing.is_none() {
        return fail(format!("thread {} not found", args.id));
    }
    let body = read_body(&args.body)?;
    let default = ctx.default_author()?;
    let thread = ctx.update(&scope, |target, now| {
        let author = author(&args.author, target, default)?;
        let thread = find_thread_mut(target, &args.id)?;
        let mut comment = local_comment(&thread.id, &thread.comments, author, body, now);
        comment.local = args.local;
        thread.comments.push(comment);
        thread.updated_at = now.to_owned();
        Ok(thread.clone())
    })?;
    Ok(json!({ "target": scope.key, "thread": thread }))
}

pub(super) fn normalize_resolution(ctx: &Ctx, resolution: &str) -> Result<Resolution> {
    if resolution == "stale" {
        return Ok(Resolution::Stale);
    }
    if resolution == "answered" {
        return Ok(Resolution::Answered);
    }
    if let Some(reason) = resolution.strip_prefix("not fixed:") {
        let reason = limited(reason.trim().to_owned(), "not fixed reason")?;
        return Ok(Resolution::NotFixed { reason });
    }
    if let Some(reason) = resolution.strip_prefix("rejected:") {
        let reason = limited(reason.trim().to_owned(), "rejected reason")?;
        return Ok(Resolution::Rejected { reason });
    }
    if let Some(rev) = resolution.strip_prefix("fixed ") {
        return Ok(Resolution::Fixed {
            sha: ctx.repo.commit(rev.trim())?,
        });
    }
    fail(
        "resolution must be `fixed <sha>`, `not fixed: <reason>`, `rejected: <reason>`, \
         `stale` or `answered`",
    )
}

/// `✅ Fixed in [<sha> <subject>](<commit URL>).`; the link text alone before the target is
/// linked to a PR/MR.
fn fixed_reply(sha: &str, subject: &str, review: Option<&Review>) -> String {
    let mut text = format!("{} ", &sha[..sha.len().min(7)]);
    for char in subject.chars() {
        if matches!(char, '\\' | '[' | ']') {
            text.push('\\');
        }
        text.push(char);
    }
    match review.and_then(|review| forge::commit_url(review, sha)) {
        Some(url) => format!("✅ Fixed in [{text}]({url})."),
        None => format!("✅ Fixed in {text}."),
    }
}

pub(super) fn resolve(ctx: &Ctx, args: &ResolveArgs) -> Result<Value> {
    let scope = ctx.scope()?;
    if scope.existing.is_none() {
        return fail(format!("thread {} not found", args.ids[0]));
    }
    // `--fixed`, `--not-fixed` and `--rejected` are shorthands for the triage, resolution and
    // reply of a fix, a failed fix check and a finding that will not be fixed.
    let (triage, resolution, reopen, default_reply) = match (&args.fixed, &args.not_fixed) {
        (None, None) if let Some(reason) = &args.rejected => {
            let reason = reason.trim().trim_end_matches('.');
            (
                None,
                Some(format!("rejected: {reason}")),
                false,
                Some(format!("🚫 Rejected: {reason}.")),
            )
        }
        (Some(sha), _) => {
            let sha = ctx.repo.commit(sha)?;
            let review = scope
                .existing
                .as_ref()
                .and_then(|target| target.review.as_ref());
            let reply = fixed_reply(&sha, &ctx.repo.subject(&sha)?, review);
            (
                Some(Triage::Fixed),
                Some(format!("fixed {sha}")),
                false,
                Some(reply),
            )
        }
        (None, Some(reason)) => {
            let reason = reason.trim().trim_end_matches('.');
            (
                None,
                Some(format!("not fixed: {reason}")),
                true,
                Some(format!("❌ Not fixed: {reason}.")),
            )
        }
        (None, None) => (args.triage, args.resolution.clone(), args.reopen, None),
    };
    let resolution = resolution
        .as_deref()
        .map(|text| normalize_resolution(ctx, text))
        .transpose()?;
    let records_only = matches!(triage, Some(Triage::Valid | Triage::Question));
    if !reopen && records_only && resolution.is_some() {
        return fail("--resolution needs a resolving triage (fixed or stale)");
    }
    let resolution = match (triage, resolution) {
        (Some(Triage::Stale), None) if !reopen => Some(Resolution::Stale),
        (Some(Triage::Fixed), None) if !reopen => {
            return fail("--triage fixed needs --resolution 'fixed <sha>'");
        }
        (_, resolution) => resolution,
    };
    let reply = match (&args.reply, default_reply) {
        (Some(body), _) => Some(read_body(body)?),
        (None, reply) => reply,
    };
    let default = match reply {
        Some(_) => ctx.default_author()?,
        None => None,
    };
    let threads = ctx.update(&scope, |target, now| {
        let author = match reply {
            Some(_) => Some(author(&args.author, target, default)?),
            None => None,
        };
        let mut changed = Vec::new();
        for id in &args.ids {
            let thread = find_thread_mut(target, id)?;
            if let (Some(body), Some(author)) = (&reply, &author) {
                let comment = local_comment(
                    &thread.id,
                    &thread.comments,
                    author.clone(),
                    body.clone(),
                    now,
                );
                thread.comments.push(comment);
            }
            if triage.is_some() {
                thread.triage = triage;
            } else if args.rejected.is_some() {
                thread.triage = None;
            }
            let was = thread.resolved;
            if reopen {
                thread.resolved = false;
                thread.resolution.clone_from(&resolution);
                thread.held = triage == Some(Triage::Question);
            } else if records_only {
                thread.held = triage == Some(Triage::Question);
            } else {
                thread.resolved = true;
                thread.held = false;
                if resolution.is_some() {
                    thread.resolution.clone_from(&resolution);
                }
            }
            if thread.resolved != was {
                thread.resolve_pending = Some(thread.resolved);
            }
            thread.updated_at = now.to_owned();
            changed.push(thread.clone());
        }
        Ok(changed)
    })?;
    if let [thread] = threads.as_slice() {
        return Ok(json!({ "target": scope.key, "thread": thread }));
    }
    let threads: Vec<Value> = threads
        .iter()
        .map(|thread| {
            json!({
                "id": thread.id,
                "resolved": thread.resolved,
                "resolution": thread.resolution,
                "held": thread.held,
            })
        })
        .collect();
    Ok(json!({ "target": scope.key, "threads": threads }))
}
