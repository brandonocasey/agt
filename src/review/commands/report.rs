//! The report behind `agt status`, `agt review status` and `agt qa status`: one target in
//! detail, or a summary of every open PR/MR. At a terminal it prints text; piped, or with
//! `--json`, the status JSON.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::IsTerminal;

use serde_json::{Value, json};

use crate::core::clock;
use crate::core::error::{Result, fail};
use crate::core::output::Output;
use crate::core::table::table;
use crate::forge::auto::Offline;
use crate::forge::landed::Source;
use crate::git::LogEntry;
use crate::review::cli::ReportArgs;
use crate::store::model::{Check, Forge, Outcome, Resolution, Target, TargetKind, Thread};

use super::status::{brief_status, status};
use super::{Ctx, qa};

const SUMMARY_WIDTH: usize = 100;

/// Which part of a target's state a command reports.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Section {
    All,
    Review,
    Qa,
}

impl Section {
    fn review(self) -> bool {
        self != Self::Qa
    }

    fn qa(self) -> bool {
        self != Self::Review
    }

    /// Whether a thread view or fixed entry belongs to this section: QA threads carry a check.
    fn keeps(self, thread: &Value) -> bool {
        if thread["check"].is_null() {
            self.review()
        } else {
            self.qa()
        }
    }
}

/// Refuses `NAME` or `--all` beside `--target`, `--range` or each other.
pub(super) fn check_flags(pinned: bool, report: &ReportArgs) -> Result<()> {
    match (&report.name, report.all) {
        (Some(_), true) => fail("--all lists every target and takes no NAME"),
        (None, true) if pinned => fail("--all cannot be combined with --target or --range"),
        (Some(_), false) if pinned => fail("NAME cannot be combined with --target or --range"),
        _ => Ok(()),
    }
}

/// Applies `NAME` to the selection, and refuses it beside `--target`, `--range` or `--all`.
pub(super) fn select(ctx: &mut Ctx, report: &ReportArgs) -> Result<()> {
    check_flags(
        ctx.selection.target.is_some() || ctx.selection.range.is_some(),
        report,
    )?;
    match &report.name {
        Some(name) if !report.all => ctx.resolve_name(name),
        _ => Ok(()),
    }
}

/// Selects `NAME` again when the pull that just ran renamed the selected target (`K` to
/// `gh-N`), which deleted the key `select` chose.
pub(super) fn reselect(ctx: &mut Ctx, report: &ReportArgs) -> Result<()> {
    let (Some(name), Some(key)) = (&report.name, ctx.selection.target.clone()) else {
        return Ok(());
    };
    if report.all || ctx.store.load(&ctx.dir, &key)?.is_some() {
        return Ok(());
    }
    ctx.selection.target = None;
    ctx.resolve_name(name)
}

pub(super) fn run(
    ctx: &mut Ctx,
    report: &ReportArgs,
    section: Section,
    head: Option<&str>,
    full: bool,
) -> Result<Output> {
    let text = !report.json && std::io::stdout().is_terminal();
    if report.all {
        let summary = summary(ctx, section)?;
        return Ok(if text {
            Output::Text(summary_text(&summary, section))
        } else {
            Output::Json(summary)
        });
    }
    if text {
        let status = status(ctx, head)?;
        return Ok(Output::Text(detail_text(
            &status,
            section,
            clock::now_secs()?,
        )));
    }
    Ok(Output::Json(if section == Section::Qa {
        qa::status(ctx, head)?
    } else {
        let status = status(ctx, head)?;
        if full { status } else { brief_status(status) }
    }))
}

/// The first line of a thread's opening comment, shortened.
pub(super) fn summary_line(thread: &Thread) -> Option<String> {
    let line = thread
        .comments
        .first()?
        .body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    if line.chars().count() <= SUMMARY_WIDTH {
        return Some(line.to_owned());
    }
    let cut: String = line.chars().take(SUMMARY_WIDTH - 1).collect();
    Some(format!("{cut}…"))
}

/// The commits of `history` that are not reachable from the newest commit a check ran on,
/// matched by head or by tree so a rebase that changes no content still counts as tested, and
/// that commit. Reachability, not list position, so an older commit merged in later is still
/// untested. Merges are not listed. Without checks every commit is untested.
fn untested<'a>(
    history: &'a [LogEntry],
    target: Option<&Target>,
) -> (Vec<&'a LogEntry>, Option<&'a str>) {
    let checks: Vec<&Check> = target
        .iter()
        .flat_map(|target| &target.qa_runs)
        .flat_map(|run| &run.checks)
        .collect();
    let tested = history.iter().rposition(|entry| {
        checks
            .iter()
            .any(|check| check.head.as_deref() == Some(&entry.sha) || check.tree == entry.tree)
    });
    let by_sha: HashMap<&str, &LogEntry> = history
        .iter()
        .map(|entry| (entry.sha.as_str(), entry))
        .collect();
    let mut covered: HashSet<&str> = HashSet::new();
    let mut pending: Vec<&str> = tested
        .map(|index| history[index].sha.as_str())
        .into_iter()
        .collect();
    while let Some(sha) = pending.pop() {
        if let Some(entry) = by_sha.get(sha)
            && covered.insert(entry.sha.as_str())
        {
            pending.extend(entry.parents.iter().map(String::as_str));
        }
    }
    (
        history
            .iter()
            .filter(|entry| entry.parents.len() < 2 && !covered.contains(entry.sha.as_str()))
            .collect(),
        tested.map(|index| history[index].sha.as_str()),
    )
}

/// Threads resolved as fixed, with the fix commit, and checks that pass now after failing.
fn fixed(ctx: &Ctx, target: Option<&Target>) -> Value {
    let Some(target) = target else {
        return json!({ "threads": [], "checks": [] });
    };
    let threads: Vec<Value> = target
        .threads
        .iter()
        .filter(|thread| thread.resolved)
        .filter_map(|thread| match &thread.resolution {
            Some(Resolution::Fixed { sha }) => Some(json!({
                "id": thread.id,
                "type": thread.kind,
                "severity": thread.severity,
                "check": thread.check,
                "sha": sha,
                "subject": ctx.repo.subject(sha).ok(),
                "summary": summary_line(thread),
            })),
            _ => None,
        })
        .collect();
    let mut checks: Vec<&Check> = target.qa_runs.iter().flat_map(|run| &run.checks).collect();
    checks.sort_by(|a, b| a.recorded_at.cmp(&b.recorded_at));
    let mut names: Vec<&str> = Vec::new();
    for check in &checks {
        if !names.contains(&check.name.as_str()) {
            names.push(&check.name);
        }
    }
    let healed: Vec<Value> = names
        .iter()
        .filter_map(|name| {
            let runs: Vec<&&Check> = checks.iter().filter(|check| check.name == *name).collect();
            let last = runs.last()?;
            let failed = runs.iter().rfind(|check| check.outcome == Outcome::Fail)?;
            (last.outcome == Outcome::Pass).then(|| {
                json!({
                    "name": name,
                    "check": last.id,
                    "failed_at": failed.recorded_at,
                    "passed_at": last.recorded_at,
                })
            })
        })
        .collect();
    json!({ "threads": threads, "checks": healed })
}

/// Adds the fields the reports share to a status: when the PR/MR was opened, the untested
/// commits and what was fixed.
pub(super) fn extend(status: &mut Value, ctx: &Ctx, target: Option<&Target>, history: &[LogEntry]) {
    let (untested, tested_head) = untested(history, target);
    status["opened_at"] = json!(
        target
            .and_then(|target| target.review.as_ref())
            .and_then(|review| review.created_at.as_ref())
    );
    status["untested_commits"] = untested
        .iter()
        .map(|entry| json!({ "sha": entry.sha, "subject": entry.subject }))
        .collect();
    status["tested_head"] = json!(tested_head);
    status["fixed"] = fixed(ctx, target);
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

fn items(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

fn label(thread: &Value) -> String {
    let kind = thread["type"].as_str().unwrap_or("thread");
    match thread["severity"].as_str() {
        Some(severity) => format!("{kind}/{severity}"),
        None => kind.to_owned(),
    }
}

fn place(thread: &Value) -> String {
    let current = &thread["current"];
    match (current["path"].as_str(), current["line"].as_u64()) {
        (Some(path), Some(line)) => format!("{path}:{line}"),
        (Some(path), None) => path.to_owned(),
        _ => "general".to_owned(),
    }
}

fn since(text: Option<&str>, now: i64) -> Option<String> {
    let then = clock::parse(text?).ok()?;
    Some(clock::ago(now, then))
}

/// One target's state as text: PR/MR header, then the review, QA and fixed sections that
/// `section` covers.
pub(super) fn detail_text(status: &Value, section: Section, now: i64) -> String {
    let mut out = String::new();
    let name = status["branch"]
        .as_str()
        .or(status["target"].as_str())
        .unwrap_or("(no branch)");
    let _ = writeln!(out, "{name}");
    let review = &status["review"];
    if let Some(url) = review["url"].as_str() {
        let mut line = format!("  {url}");
        if let Some(author) = review["author"].as_str() {
            let _ = write!(line, "  by {author}");
        }
        if let Some(opened) = since(status["opened_at"].as_str(), now) {
            let _ = write!(line, "  opened {opened}");
        }
        if let Some(state) = review["state"].as_str() {
            let _ = write!(line, "  {state}");
        }
        let _ = writeln!(out, "{line}");
    } else {
        out.push_str("  no PR/MR linked\n");
    }
    if section.review() {
        review_text(&mut out, status);
    }
    if section.qa() {
        qa_text(&mut out, status);
    }
    fixed_text(&mut out, status, section, now);
    out
}

fn threads_text(out: &mut String, status: &Value, section: Section, qa: bool) {
    let open: Vec<&Value> = items(&status["open_threads"])
        .iter()
        .filter(|thread| section.keeps(thread) && thread["check"].is_null() != qa)
        .collect();
    if open.is_empty() {
        return;
    }
    let _ = writeln!(out, "  open findings ({}):", open.len());
    for thread in open {
        let _ = writeln!(
            out,
            "    {}  {}  {}  {}",
            thread["id"].as_str().unwrap_or("?"),
            label(thread),
            place(thread),
            thread["summary"].as_str().unwrap_or("")
        );
    }
}

fn review_text(out: &mut String, status: &Value) {
    out.push_str("\nReview\n");
    if status["head"].is_null() {
        out.push_str("  no head is known for this branch\n");
    }
    let commits = items(&status["commits"]);
    let unreviewed = items(&status["unreviewed_commits"]);
    let total = if commits.is_empty() {
        unreviewed.len()
    } else {
        commits.len()
    };
    let _ = writeln!(out, "  {} of {total} commits unreviewed", unreviewed.len());
    for sha in unreviewed.iter().filter_map(Value::as_str) {
        let subject = commits
            .iter()
            .find(|commit| commit["sha"] == sha)
            .and_then(|commit| commit["subject"].as_str())
            .unwrap_or("");
        let _ = writeln!(out, "    {} {subject}", short(sha));
    }
    match status["last_reviewed_head"].as_str() {
        Some(head) => {
            let _ = writeln!(out, "  last reviewed head: {}", short(head));
        }
        None => out.push_str("  last reviewed head: none\n"),
    }
    threads_text(out, status, Section::Review, false);
    let held: Vec<&str> = items(&status["held_questions"])
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if !held.is_empty() {
        let _ = writeln!(out, "  held questions: {}", held.join(", "));
    }
}

fn qa_text(out: &mut String, status: &Value) {
    out.push_str("\nQA\n");
    let untested = items(&status["untested_commits"]);
    let qa = &status["qa"];
    match (status["tested_head"].as_str(), untested.len()) {
        (Some(head), 0) => {
            let _ = writeln!(out, "  every commit tested (last tested {})", short(head));
        }
        (Some(head), count) => {
            let _ = writeln!(out, "  {count} commits untested since {}", short(head));
        }
        (None, count) => {
            let _ = writeln!(
                out,
                "  no check ran on a commit here; {count} commits untested"
            );
        }
    }
    for commit in untested {
        let _ = writeln!(
            out,
            "    {} {}",
            short(commit["sha"].as_str().unwrap_or("")),
            commit["subject"].as_str().unwrap_or("")
        );
    }
    if qa.is_null() {
        out.push_str("  no checks recorded\n");
    } else {
        let _ = writeln!(
            out,
            "  checks: {} pass, {} fail, {} blocked, {} skipped, {} stale",
            qa["pass"], qa["fail"], qa["blocked"], qa["skipped"], qa["stale"]
        );
        for check in items(&qa["checks"]) {
            if matches!(check["outcome"].as_str(), Some("fail" | "blocked")) {
                let _ = writeln!(
                    out,
                    "  {}: {} {}",
                    check["outcome"].as_str().unwrap_or(""),
                    check["name"].as_str().unwrap_or("?"),
                    check["note"].as_str().unwrap_or("")
                );
            }
        }
    }
    threads_text(out, status, Section::Qa, true);
    if !qa.is_null() {
        let approved = if qa["approved"] == true { "yes" } else { "no" };
        let _ = writeln!(out, "  approved: {approved}");
    }
}

fn fixed_text(out: &mut String, status: &Value, section: Section, now: i64) {
    out.push_str("\nFixed\n");
    let threads: Vec<&Value> = items(&status["fixed"]["threads"])
        .iter()
        .filter(|thread| section.keeps(thread))
        .collect();
    let checks = if section.qa() {
        items(&status["fixed"]["checks"])
    } else {
        &[]
    };
    if threads.is_empty() && checks.is_empty() {
        out.push_str("  nothing yet\n");
    }
    // One heading per fix commit, in the order the threads list them, so a commit that fixed
    // several threads names its subject once.
    let mut shas: Vec<&str> = Vec::new();
    for thread in &threads {
        let sha = thread["sha"].as_str().unwrap_or("");
        if !shas.contains(&sha) {
            shas.push(sha);
        }
    }
    for sha in shas {
        let mut fixed = threads
            .iter()
            .filter(|thread| thread["sha"].as_str().unwrap_or("") == sha)
            .peekable();
        let subject = fixed
            .peek()
            .and_then(|thread| thread["subject"].as_str())
            .unwrap_or("");
        let _ = writeln!(out, "  fixed in {} {subject}", short(sha));
        for thread in fixed {
            let _ = writeln!(
                out,
                "    {}  {}  {}",
                thread["id"].as_str().unwrap_or("?"),
                label(thread),
                thread["summary"].as_str().unwrap_or("")
            );
        }
    }
    for check in checks {
        let failed = since(check["failed_at"].as_str(), now).unwrap_or_default();
        let _ = writeln!(
            out,
            "  check {}: failed {failed}, passes now",
            check["name"].as_str().unwrap_or("?")
        );
    }
}

/// An open PR/MR or a local target with no PR/MR, as one summary row.
struct Row {
    number: Option<u64>,
    url: Option<String>,
    branch: Option<String>,
    opened_at: Option<String>,
    author: Option<String>,
    draft: bool,
    target: Option<Target>,
}

fn opened_secs(row: &Row) -> i64 {
    row.opened_at
        .as_deref()
        .and_then(|time| clock::parse(time).ok())
        .unwrap_or(i64::MAX)
}

/// Stored targets with something recorded for a branch.
fn local_targets(ctx: &Ctx) -> Result<Vec<Target>> {
    Ok(ctx
        .store
        .targets(&ctx.dir)?
        .into_iter()
        .filter(|target| {
            target.kind == TargetKind::Branch
                && (target.review.is_some()
                    || !target.runs.is_empty()
                    || !target.qa_runs.is_empty()
                    || !target.threads.is_empty())
        })
        .collect())
}

/// Whether the target's branch still exists, so deleted branches drop out. A merged branch
/// that is still a local branch stays listed.
fn on_local_branch(ctx: &Ctx, target: &Target) -> bool {
    target
        .branch
        .as_ref()
        .is_some_and(|branch| ctx.repo.commit(&format!("refs/heads/{branch}")).is_ok())
}

fn stored_row(target: Target) -> Row {
    let review = target.review.clone();
    Row {
        number: review.as_ref().map(|review| review.number),
        url: review.as_ref().map(|review| review.url.clone()),
        branch: target.branch.clone(),
        opened_at: review.as_ref().and_then(|review| review.created_at.clone()),
        author: review.and_then(|review| review.author),
        draft: false,
        target: Some(target),
    }
}

/// The rows: open PRs/MRs oldest first, then local targets with no PR/MR. Offline, or when the
/// forge cannot be asked, the PRs/MRs are the open ones in the stored reviews. The second
/// value says why the forge's list was not used.
fn rows(ctx: &Ctx) -> Result<(Vec<Row>, Option<String>)> {
    let mut stored = local_targets(ctx)?;
    stored.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let (pulls, note) = if Offline::current().is_set() {
        (
            None,
            Some("offline: PRs/MRs come from stored reviews".to_owned()),
        )
    } else {
        match Source::chosen(&ctx.repo).and_then(|origin| match origin {
            Some(origin) => origin.open_pulls().map(|pulls| Some((origin, pulls))),
            None => Ok(None),
        }) {
            Ok(Some((origin, pulls))) => (Some((origin, pulls)), None),
            Ok(None) => (
                None,
                Some("no pull request tracker: PRs/MRs come from stored reviews".to_owned()),
            ),
            Err(why) => (
                None,
                Some(format!(
                    "forge unavailable ({why}): PRs/MRs come from stored reviews"
                )),
            ),
        }
    };
    let mut mrs: Vec<Row> = Vec::new();
    let mut local: Vec<Row> = Vec::new();
    match pulls {
        Some((origin, pulls)) => {
            let here = |review: &crate::store::model::Review| {
                review.forge == origin.forge
                    && super::scope::same_location(
                        (review.host.as_str(), review.repo.as_str()),
                        (origin.host.as_str(), origin.repo.as_str()),
                    )
            };
            for pull in pulls {
                // The pull's own URL names its repository as the forge does now, so a renamed
                // repository still matches the reviews synced under its new name.
                let location = crate::forge::parse_pr(&pull.url)
                    .ok()
                    .and_then(|pr| pr.location);
                let this = |review: &crate::store::model::Review| {
                    review.number == pull.number
                        && match &location {
                            Some((host, repo)) => {
                                review.forge == origin.forge
                                    && super::scope::same_location(
                                        (host.as_str(), repo.as_str()),
                                        (review.host.as_str(), review.repo.as_str()),
                                    )
                            }
                            None => here(review),
                        }
                };
                let position = stored
                    .iter()
                    .position(|target| target.review.as_ref().is_some_and(this))
                    .or_else(|| {
                        // A fork's branch name proves nothing, and a target with another
                        // PR/MR's review is that one's.
                        if !pull.same_repo {
                            return None;
                        }
                        stored.iter().position(|target| {
                            target.branch.as_deref() == Some(&pull.head_branch)
                                && target.review.as_ref().is_none_or(this)
                        })
                    });
                mrs.push(Row {
                    number: Some(pull.number),
                    url: Some(pull.url),
                    branch: Some(pull.head_branch),
                    opened_at: pull.created_at,
                    author: pull.author,
                    draft: pull.draft,
                    target: position.map(|index| stored.remove(index)),
                });
            }
            // Reviews of another repository (a fork's parent) are not in origin's list.
            for target in stored {
                match target.review.as_ref() {
                    None if on_local_branch(ctx, &target) => local.push(stored_row(target)),
                    Some(review)
                        if review.state == crate::store::model::ReviewState::Open
                            && !here(review) =>
                    {
                        mrs.push(stored_row(target));
                    }
                    _ => {}
                }
            }
        }
        None => {
            for target in stored {
                match target.review.as_ref().map(|review| review.state) {
                    None if on_local_branch(ctx, &target) => local.push(stored_row(target)),
                    None => {}
                    Some(crate::store::model::ReviewState::Open) => mrs.push(stored_row(target)),
                    Some(_) => {}
                }
            }
        }
    }
    mrs.sort_by_key(opened_secs);
    local.sort_by(|a, b| {
        let created = |row: &Row| row.target.as_ref().map(|target| target.created_at.clone());
        created(a).cmp(&created(b))
    });
    mrs.extend(local);
    Ok((mrs, note))
}

fn qa_counts(qa: &Value) -> Value {
    if qa.is_null() {
        return Value::Null;
    }
    json!({
        "pass": qa["pass"],
        "fail": qa["fail"],
        "blocked": qa["blocked"],
        "stale": qa["stale"],
        "approved": qa["approved"],
    })
}

/// The counts of one row, from the status of its target.
fn counts(status: &Value, section: Section) -> Value {
    let open = items(&status["open_threads"])
        .iter()
        .filter(|thread| section.keeps(thread))
        .count();
    let fixed_threads = items(&status["fixed"]["threads"])
        .iter()
        .filter(|thread| section.keeps(thread))
        .count();
    let fixed_checks = if section.qa() {
        items(&status["fixed"]["checks"]).len()
    } else {
        0
    };
    json!({
        "unreviewed": items(&status["unreviewed_commits"]).len(),
        "untested": items(&status["untested_commits"]).len(),
        "open": open,
        "fixed": fixed_threads + fixed_checks,
        "qa": qa_counts(&status["qa"]),
    })
}

/// Every open PR/MR, and local targets with no PR/MR, with their review and QA counts.
fn summary(ctx: &mut Ctx, section: Section) -> Result<Value> {
    let (rows, note) = rows(ctx)?;
    let selected = ctx.selection.target.take();
    let mut entries = Vec::new();
    for row in rows {
        let evaluated = row.target.as_ref().map(|target| {
            ctx.selection.target = Some(target.key.clone());
            status(ctx, None)
        });
        let mut entry = json!({
            "mr": row.number,
            "url": row.url,
            "branch": row.branch,
            "target": row.target.as_ref().map(|target| &target.key),
            "opened_at": row.opened_at,
            "author": row.author,
            "draft": row.draft,
        });
        // One target that cannot be read leaves the others listed.
        match evaluated {
            Some(Ok(status)) => {
                if let (Some(fields), Some(found)) =
                    (entry.as_object_mut(), counts(&status, section).as_object())
                {
                    fields.extend(found.clone());
                }
            }
            Some(Err(why)) => entry["error"] = json!(why.to_string()),
            None => {}
        }
        entries.push(entry);
    }
    ctx.selection.target = selected;
    Ok(json!({
        "repo": ctx.repo_key,
        "offline": note.is_some(),
        "note": note,
        "targets": entries,
    }))
}

fn count(entry: &Value, field: &str) -> String {
    entry[field]
        .as_u64()
        .map_or_else(|| "-".to_owned(), |count| count.to_string())
}

fn qa_label(qa: &Value) -> String {
    if qa.is_null() {
        return "-".to_owned();
    }
    let mut parts = vec![format!("{} pass", qa["pass"])];
    for (field, name) in [("fail", "fail"), ("blocked", "blocked"), ("stale", "stale")] {
        if qa[field].as_u64().unwrap_or(0) > 0 {
            parts.push(format!("{} {name}", qa[field]));
        }
    }
    if qa["approved"] == true {
        parts.push("approved".to_owned());
    }
    parts.join(", ")
}

/// The summary as a table, with a note on where the PR/MR list came from.
fn summary_text(summary: &Value, section: Section) -> String {
    let mut headers = vec!["MR", "BRANCH", "OPENED"];
    if section.review() {
        headers.push("UNREVIEWED");
    }
    if section.qa() {
        headers.push("UNTESTED");
    }
    headers.extend(["OPEN", "FIXED"]);
    if section.qa() {
        headers.push("QA");
    }
    let now = clock::now_secs().ok();
    let rows: Vec<Vec<String>> = items(&summary["targets"])
        .iter()
        .map(|entry| {
            let mut row = vec![
                entry["mr"].as_u64().map_or_else(
                    || "-".to_owned(),
                    |number| {
                        // GitHub says `#12`, GitLab `!12`.
                        // From the URL's path, so GitHub Enterprise hosts count as GitHub.
                        let github = entry["url"].as_str().is_some_and(|url| {
                            crate::forge::parse_pr(url).ok().and_then(|pr| pr.forge)
                                == Some(Forge::Github)
                        });
                        format!("{}{number}", if github { '#' } else { '!' })
                    },
                ),
                entry["branch"].as_str().unwrap_or("-").to_owned(),
                now.and_then(|now| since(entry["opened_at"].as_str(), now))
                    .unwrap_or_else(|| "-".to_owned()),
            ];
            if section.review() {
                row.push(count(entry, "unreviewed"));
            }
            if section.qa() {
                row.push(count(entry, "untested"));
            }
            row.push(count(entry, "open"));
            row.push(count(entry, "fixed"));
            if section.qa() {
                row.push(qa_label(&entry["qa"]));
            }
            row
        })
        .collect();
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("no open PRs/MRs and no local review or QA records\n");
    } else {
        table(&mut out, &headers, &rows);
    }
    if let Some(note) = summary["note"].as_str() {
        let _ = writeln!(out, "({note})");
    }
    for entry in items(&summary["targets"]) {
        if let Some(why) = entry["error"].as_str() {
            let _ = writeln!(
                out,
                "(could not read {}: {why})",
                entry["branch"].as_str().unwrap_or("a target")
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sha: &str, tree: &str, parents: &[&str]) -> LogEntry {
        LogEntry {
            sha: sha.to_owned(),
            tree: tree.to_owned(),
            parents: parents.iter().map(|parent| (*parent).to_owned()).collect(),
            subject: format!("subject {sha}"),
        }
    }

    fn tested(head: Option<&str>, tree: &str) -> Target {
        let target: Target = serde_json::from_value(json!({
            "version": crate::store::model::VERSION, "revision": 0, "key": "k", "repo": "r",
            "kind": "branch", "branch": "b", "branch_birth": null, "base": null, "head": null,
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
            "review": null, "runs": [], "records": [], "threads": [],
            "qa_runs": [{
                "id": "qa-aaaa", "started_at": "2026-01-01T00:00:00Z", "ended_at": null,
                "env": [], "tester": {"kind": "human", "login": "me"},
                "checks": [{
                    "id": "qa-aaaa.k1", "name": "login", "kind": "manual",
                    "outcome": "pass", "head": head, "tree": tree,
                    "recorded_at": "2026-01-01T00:00:00Z"
                }]
            }],
        }))
        .unwrap();
        target
    }

    #[test]
    fn every_commit_is_untested_without_checks() {
        let history = [entry("a", "ta", &["z"]), entry("b", "tb", &["a"])];
        let (commits, head) = untested(&history, None);
        assert_eq!(commits.len(), 2);
        assert_eq!(head, None);
    }

    #[test]
    fn commits_after_the_tested_head_are_untested() {
        let history = [
            entry("a", "ta", &["z"]),
            entry("b", "tb", &["a"]),
            entry("c", "tc", &["b"]),
        ];
        let target = tested(Some("a"), "elsewhere");
        let (commits, head) = untested(&history, Some(&target));
        assert_eq!(head, Some("a"));
        assert_eq!(
            commits
                .iter()
                .map(|commit| commit.sha.as_str())
                .collect::<Vec<_>>(),
            ["b", "c"]
        );
    }

    #[test]
    fn a_rebase_with_the_same_tree_counts_as_tested() {
        let history = [entry("a2", "ta", &["z"]), entry("b2", "tb", &["a2"])];
        let target = tested(Some("a-before-rebase"), "tb");
        let (commits, head) = untested(&history, Some(&target));
        assert!(commits.is_empty());
        assert_eq!(head, Some("b2"));
    }

    #[test]
    fn merges_are_not_counted_as_untested_commits() {
        let history = [entry("a", "ta", &["z"]), entry("m", "tm", &["a", "o"])];
        let target = tested(Some("a"), "x");
        assert!(untested(&history, Some(&target)).0.is_empty());
    }

    #[test]
    fn an_older_commit_merged_in_after_the_tested_head_is_untested() {
        // `old` is listed first but is not an ancestor of the tested `t`.
        let history = [
            entry("old", "to", &["z"]),
            entry("t", "tt", &["z"]),
            entry("m", "tm", &["t", "old"]),
        ];
        let target = tested(Some("t"), "x");
        let (commits, head) = untested(&history, Some(&target));
        assert_eq!(head, Some("t"));
        assert_eq!(
            commits
                .iter()
                .map(|commit| commit.sha.as_str())
                .collect::<Vec<_>>(),
            ["old"]
        );
    }

    fn status_json() -> Value {
        json!({
            "target": "branch-x", "branch": "feat", "head": "h1",
            "review": {"url": "https://h/pr/3", "author": "ann", "state": "open"},
            "opened_at": "2026-10-25T00:00:00Z",
            "commits": [
                {"sha": "1111111aaaa", "subject": "one", "reviewed": true},
                {"sha": "2222222bbbb", "subject": "two", "reviewed": false},
            ],
            "unreviewed_commits": ["2222222bbbb"],
            "last_reviewed_head": "1111111aaaa",
            "open_threads": [
                {"id": "th-1", "type": "bug", "severity": "major", "check": null,
                 "current": {"path": "src/a.rs", "line": 4}, "summary": "off by one"},
                {"id": "th-2", "type": "bug", "severity": null, "check": "qa-x.k1",
                 "current": null, "summary": "crash"},
            ],
            "held_questions": ["th-9"],
            "untested_commits": [{"sha": "2222222bbbb", "subject": "two"}],
            "tested_head": "1111111aaaa",
            "qa": {"pass": 1, "fail": 1, "blocked": 0, "skipped": 0, "stale": 0,
                   "approved": false,
                   "checks": [{"name": "login", "outcome": "fail", "note": "500"}]},
            "fixed": {
                "threads": [
                    {"id": "th-3", "type": "bug", "severity": "minor", "check": null,
                     "sha": "3333333cccc", "subject": "fix it", "summary": "typo"},
                    {"id": "th-4", "type": "bug", "severity": null, "check": "qa-x.k2",
                     "sha": "4444444dddd", "subject": "fix qa", "summary": "hang"},
                ],
                "checks": [{"name": "signup", "failed_at": "2026-10-26T00:00:00Z"}],
            },
        })
    }

    const NOW: i64 = 1_792_000_000;

    #[test]
    fn the_review_report_hides_qa() {
        let text = detail_text(&status_json(), Section::Review, NOW);
        assert!(text.contains("https://h/pr/3  by ann  opened"), "{text}");
        assert!(text.contains("1 of 2 commits unreviewed"));
        assert!(text.contains("2222222 two"));
        assert!(text.contains("th-1  bug/major  src/a.rs:4  off by one"));
        assert!(text.contains("held questions: th-9"));
        assert!(text.contains("th-3") && text.contains("fixed in 3333333 fix it"));
        for hidden in ["QA\n", "th-2", "th-4", "signup", "untested"] {
            assert!(!text.contains(hidden), "{hidden} in {text}");
        }
    }

    #[test]
    fn the_qa_report_shows_only_qa() {
        let text = detail_text(&status_json(), Section::Qa, NOW);
        assert!(text.contains("1 commits untested since 1111111"), "{text}");
        assert!(text.contains("checks: 1 pass, 1 fail"));
        assert!(text.contains("fail: login 500"));
        assert!(text.contains("th-2  bug  general  crash"));
        assert!(text.contains("th-4") && text.contains("check signup: failed"));
        for hidden in ["Review\n", "th-1", "th-3", "held questions"] {
            assert!(!text.contains(hidden), "{hidden} in {text}");
        }
    }

    #[test]
    fn the_summary_lists_counts_by_section() {
        let status = status_json();
        let counted = counts(&status, Section::Review);
        assert_eq!(counted["unreviewed"], 1);
        assert_eq!(counted["open"], 1);
        assert_eq!(counted["fixed"], 1);
        let counted = counts(&status, Section::All);
        assert_eq!(counted["open"], 2);
        assert_eq!(counted["fixed"], 3);
        let summary = json!({
            "note": "offline: PRs/MRs come from stored reviews",
            "targets": [
                {"mr": 3, "url": "https://gitlab.com/o/r/-/merge_requests/3", "branch": "feat",
                 "opened_at": null, "unreviewed": 1,
                 "untested": 2, "open": 1, "fixed": 0, "qa": null},
                {"mr": null, "branch": "local", "opened_at": null},
            ],
        });
        let text = summary_text(&summary, Section::All);
        assert!(text.contains("UNREVIEWED"));
        assert!(
            text.lines()
                .any(|line| line.starts_with("!3") && line.contains("feat"))
        );
        assert!(
            text.lines()
                .any(|line| line.starts_with('-') && line.contains("local"))
        );
        assert!(text.contains("(offline: PRs/MRs come from stored reviews)"));
        let github =
            json!({"targets": [{"mr": 4, "url": "https://github.com/o/r/pull/4", "branch": "gh"}]});
        assert!(
            summary_text(&github, Section::All)
                .lines()
                .any(|line| line.starts_with("#4"))
        );
        let enterprise =
            json!({"targets": [{"mr": 6, "url": "https://ghe.example/o/r/pull/6", "branch": "e"}]});
        assert!(
            summary_text(&enterprise, Section::All)
                .lines()
                .any(|line| line.starts_with("#6"))
        );
        let other = json!({"targets": [
            {"mr": 5, "url": "https://git.example/o/pull/-/merge_requests/5", "branch": "x"}
        ]});
        assert!(
            summary_text(&other, Section::All)
                .lines()
                .any(|line| line.starts_with("!5"))
        );
        let review = summary_text(&summary, Section::Review);
        assert!(!review.contains("UNTESTED") && !review.contains("QA"));
    }
}
