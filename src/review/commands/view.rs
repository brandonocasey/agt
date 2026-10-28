use std::collections::HashMap;
use std::fmt::Write as _;

use serde_json::{Value, json};

use crate::core::error::{Error, Result, ctx};
use crate::forge::anchors::{self, FileDiff, Location};
use crate::git::Repo;
use crate::review::cli::{Format, ListArgs};
use crate::store::model::{
    Anchor, Author, Comment, KilledFinding, Severity, Target, Thread, ThreadType,
};

use super::Ctx;
use super::approve::is_open;
use crate::core::output::Output;

/// Maps anchors to the current head, caching one diff per anchor commit or tree.
pub(super) struct Locator<'a> {
    pub(super) repo: &'a Repo,
    /// `None` when no head is known; local anchors then have no current place.
    pub(super) head: Option<String>,
    pub(super) diffs: HashMap<String, Option<Vec<FileDiff>>>,
}

impl Locator<'_> {
    /// An anchor whose commit or tree is gone (collected) counts as outdated.
    pub(super) fn locate(&mut self, anchor: &Anchor, head: &str) -> Location {
        let from = anchor.commit.clone().unwrap_or_else(|| anchor.tree.clone());
        if from == head {
            return anchors::unchanged(anchor);
        }
        let repo = self.repo;
        let files = self.diffs.entry(from).or_insert_with_key(|from| {
            repo.diff_u0(from, head)
                .ok()
                .map(|diff| anchors::parse(&diff))
        });
        match files {
            Some(files) => anchors::locate(files, anchor),
            None => Location {
                outdated: true,
                ..anchors::unchanged(anchor)
            },
        }
    }

    /// Local anchors map through git; imported threads use the host's path, line and outdated
    /// state.
    pub(super) fn current(&mut self, thread: &Thread) -> Value {
        if let Some(anchor) = &thread.anchor {
            let Some(head) = self.head.clone() else {
                return Value::Null;
            };
            let location = self.locate(anchor, &head);
            return json!({ "path": location.path, "line": location.line, "outdated": location.outdated });
        }
        match &thread.remote {
            Some(remote) if remote.path.is_some() => json!({
                "path": remote.path,
                "line": remote.line.or(remote.old_line).or(remote.original_line),
                "outdated": remote.outdated,
            }),
            _ => Value::Null,
        }
    }

    pub(super) fn view(&mut self, thread: &Thread) -> Result<Value> {
        let mut value = serde_json::to_value(thread).map_err(ctx("serialize thread"))?;
        value["current"] = self.current(thread);
        Ok(value)
    }
}

pub(super) fn list(ctx: &Ctx, args: &ListArgs) -> Result<Output> {
    let scope = ctx.scope()?;
    let (no_threads, no_runs) = (Vec::new(), Vec::new());
    let (threads, runs) = scope
        .existing
        .as_ref()
        .map_or((&no_threads, &no_runs), |target| {
            (&target.threads, &target.runs)
        });
    let in_run = |id: &str| {
        let Some(run) = &args.run else {
            return true;
        };
        let Some(rest) = id.strip_prefix(&format!("{run}.c")) else {
            return false;
        };
        args.cycle.is_none_or(|cycle| {
            rest.split_once('p')
                .is_some_and(|(found, _)| found == cycle.to_string())
        })
    };
    let selected: Vec<&Thread> = threads
        .iter()
        .filter(|thread| !args.open || is_open(thread))
        .filter(|thread| in_run(&thread.id))
        .collect();
    let killed: Vec<&KilledFinding> = runs
        .iter()
        .flat_map(|run| &run.passes)
        .flat_map(|pass| &pass.killed)
        .filter(|finding| in_run(&finding.id))
        .collect();
    let mut locator = Locator {
        repo: &ctx.repo,
        head: match &args.head {
            Some(rev) => Some(ctx.repo.commit(rev)?),
            None => scope.head_known.then(|| scope.head.clone()),
        },
        diffs: HashMap::new(),
    };
    if let Format::Prompt = args.format {
        return Ok(Output::Text(prompt(
            &scope.key,
            &selected,
            &killed,
            &mut locator,
        )));
    }
    let views = selected
        .iter()
        .map(|thread| locator.view(thread))
        .collect::<Result<Vec<_>>>()?;
    Ok(Output::Json(json!({
        "target": scope.existing.as_ref().map(|_| &scope.key),
        "threads": views,
        "killed": killed,
    })))
}

pub(super) fn author_label(author: &Author) -> String {
    match author {
        Author::Human { login, .. } => login.clone(),
        Author::Agent {
            model,
            effort,
            harness,
            run,
            pass,
            reviewer,
            ..
        } => {
            let mut label = format!("{model} {effort} {harness}");
            if let (Some(run), Some(pass), Some(reviewer)) = (run, pass, reviewer) {
                let _ = write!(label, " {run}.{pass}.{reviewer}");
            }
            label
        }
    }
}

pub(super) fn kind_label(kind: ThreadType, severity: Option<Severity>) -> String {
    match severity {
        Some(severity) => format!("{}/{}", kind.as_str(), severity.as_str()),
        None => kind.as_str().to_owned(),
    }
}

pub(super) const HIDDEN: &str = "  (comment by another author, text hidden)\n";

/// Comments written here or by the authenticated forge user; reviewers never see other text.
pub(super) fn mine(comment: &Comment) -> bool {
    comment.remote.as_ref().is_none_or(|remote| remote.is_me)
}

pub(super) fn place(current: &Value) -> String {
    let Some(path) = current["path"].as_str() else {
        return "general".to_owned();
    };
    let mut text = match current["line"].as_u64() {
        Some(line) => format!("{path}:{line}"),
        None => path.to_owned(),
    };
    if current["outdated"] == true {
        text.push_str(" (outdated)");
    }
    text
}

/// Plain-text input for fix-check and reveal prompts.
pub(super) fn prompt(
    key: &str,
    threads: &[&Thread],
    killed: &[&KilledFinding],
    locator: &mut Locator,
) -> String {
    let mut out = format!("Review items for {key}\n");
    let _ = writeln!(out, "\nThreads ({}):", threads.len());
    if threads.is_empty() {
        out.push_str("- none\n");
    }
    for thread in threads {
        let current = locator.current(thread);
        let state = match (&thread.resolution, thread.resolved, thread.held) {
            (_, false, true) => "held question".to_owned(),
            (Some(resolution), true, _) => format!("resolved: {resolution}"),
            (None, true, _) => "resolved".to_owned(),
            (Some(resolution), false, _) => format!("open: {resolution}"),
            (None, false, false) => "open".to_owned(),
        };
        let _ = writeln!(
            out,
            "- {} [{}] {} ({state})",
            thread.id,
            kind_label(thread.kind, thread.severity),
            place(&current)
        );
        let url = thread
            .remote
            .as_ref()
            .and_then(|remote| remote.url.as_deref())
            .or_else(|| {
                thread
                    .comments
                    .first()
                    .and_then(|first| first.remote.as_ref()?.url.as_deref())
            });
        if let Some(url) = url {
            let _ = writeln!(out, "  URL: {url}");
        }
        let mut comments = thread.comments.iter();
        if let Some(first) = comments.next() {
            if mine(first) {
                let _ = writeln!(
                    out,
                    "  {}: {}",
                    author_label(&first.author),
                    indent(&first.body)
                );
            } else {
                out.push_str(HIDDEN);
            }
        }
        if let Some(finding) = &thread.finding {
            if let Some(evidence) = &finding.evidence {
                let _ = writeln!(out, "  Evidence: {}", indent(evidence));
            }
            if !finding.tests_run.is_empty() {
                let _ = writeln!(out, "  Tests run: {}", finding.tests_run.join("; "));
            }
            if finding.post_reveal {
                out.push_str("  post-reveal\n");
            }
        }
        for reply in comments {
            if !mine(reply) {
                out.push_str(HIDDEN);
                continue;
            }
            let _ = writeln!(
                out,
                "  Reply from {}: {}",
                author_label(&reply.author),
                indent(&reply.body)
            );
        }
    }
    let _ = writeln!(out, "\nKilled findings ({}):", killed.len());
    if killed.is_empty() {
        out.push_str("- none\n");
    }
    for finding in killed {
        let _ = writeln!(
            out,
            "- {} [{}] {}: {}\n  Killed: {}",
            finding.id,
            kind_label(finding.kind, finding.severity),
            finding.anchor.as_ref().map_or_else(
                || "general".to_owned(),
                |anchor| place(&json!({
                    "path": anchor.path,
                    "line": anchor.line.or(anchor.old_line),
                    "outdated": false,
                }))
            ),
            indent(&finding.scenario),
            indent(&finding.reason)
        );
    }
    out
}

pub(super) fn indent(text: &str) -> String {
    text.trim_end().replace('\n', "\n    ")
}

pub(super) fn show(ctx: &Ctx, id: &str) -> Result<Value> {
    let scope = ctx.scope()?;
    let not_found = || Error(format!("{id} not found in target {}", scope.key));
    let target = scope.existing.as_ref().ok_or_else(not_found)?;
    let mut locator = Locator {
        repo: &ctx.repo,
        head: scope.head_known.then(|| scope.head.clone()),
        diffs: HashMap::new(),
    };
    if let Some(thread) = target.threads.iter().find(|thread| thread.id == id) {
        return Ok(
            json!({ "target": scope.key, "kind": "thread", "thread": locator.view(thread)? }),
        );
    }
    for run in &target.runs {
        if run.id == id {
            return Ok(json!({ "target": scope.key, "kind": "run", "run": run }));
        }
        for pass in &run.passes {
            if format!("{}.{}", run.id, pass.id) == id {
                return Ok(
                    json!({ "target": scope.key, "kind": "pass", "run": run.id, "pass": pass }),
                );
            }
            if let Some(finding) = pass.killed.iter().find(|finding| finding.id == id) {
                return Ok(json!({ "target": scope.key, "kind": "killed", "finding": finding }));
            }
        }
    }
    for run in &target.qa_runs {
        if run.id == id {
            return Ok(json!({ "target": scope.key, "kind": "qa-run", "run": run }));
        }
        if let Some(check) = run.checks.iter().find(|check| check.id == id) {
            return Ok(json!({
                "target": scope.key,
                "kind": "check",
                "run": run.id,
                "check": check,
                "env": super::qa::environment(run, check),
            }));
        }
    }
    Err(not_found())
}

pub(super) struct ThreadViews {
    pub(super) open: Vec<Value>,
    pub(super) outdated: Vec<String>,
    pub(super) held: Vec<String>,
}

pub(super) fn thread_views(target: Option<&Target>, locator: &mut Locator) -> ThreadViews {
    let mut views = ThreadViews {
        open: Vec::new(),
        outdated: Vec::new(),
        held: Vec::new(),
    };
    for thread in target.iter().flat_map(|target| &target.threads) {
        let current = locator.current(thread);
        if current["outdated"] == true {
            views.outdated.push(thread.id.clone());
        }
        if !is_open(thread) {
            continue;
        }
        if thread.held {
            views.held.push(thread.id.clone());
        }
        views.open.push(json!({
            "id": thread.id,
            "type": thread.kind,
            "severity": thread.severity,
            "triage": thread.triage,
            "check": thread.check,
            "summary": super::report::summary_line(thread),
            "held": thread.held,
            "current": current,
            "post": thread.comments.first().map(|first| first.delivery.post),
            "remote_id": thread.remote.as_ref().map(|remote| &remote.id),
            "url": thread.remote.as_ref().and_then(|remote| remote.url.as_ref()),
        }));
    }
    views
}

pub(super) fn run_views(target: Option<&Target>) -> Vec<Value> {
    target
        .iter()
        .flat_map(|target| &target.runs)
        .map(|run| {
            json!({
                "id": run.id,
                "started_at": run.started_at,
                "ended_at": run.ended_at,
                "passes": run.passes.iter().map(|pass| &pass.id).collect::<Vec<_>>(),
                "repaired_head": run.repaired_head,
                "record": run.record,
            })
        })
        .collect()
}
