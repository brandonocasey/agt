//! The living summary: one comment per PR/MR that `publish` posts once and then edits in place.
//! Its headline is the only status; under it come what is left to fix, the commits no review
//! read and the findings moved to follow-up issues. It collapses the QA checks and the closed
//! findings, and holds the record marker of every run it covers. One too long for a comment
//! continues in part comments that hold items only.

use std::fmt::Write as _;

use super::render::{duration, kind_emoji, location, outcome, short};
use crate::forge::markers;
use crate::git::CommitInfo;
use crate::review::commands::approve::{blocking_threads, is_open};
use crate::review::commands::qa::{CheckState, environment};
use crate::review::commands::usage::run_spend;
use crate::store::model::{CommitPatch, Outcome, PassKind, QaRun, Resolution, Run, Target, Thread};

/// Longest finding gist in a list item or table cell.
const CELL: usize = 90;
/// Longest text shown for an open finding that is not posted as its own thread.
const MAX_NOTE: usize = 2000;
/// Longest summary, markers included: GitHub takes at most 65,536 characters.
const MAX_BODY: usize = 65_000;
/// Most unreviewed commits the summary names.
const MAX_NAMED: usize = 5;
/// Room the first comment keeps for the line that points at the parts.
const POINTER: usize = 300;
/// Room a part keeps for its first line and markers.
const PART_ROOM: usize = 500;

/// A run the summary covers, with the head and count its record marker names.
pub(super) struct Covered<'a> {
    pub(super) run: &'a Run,
    pub(super) head: String,
    pub(super) at_head: usize,
}

/// The QA runs a summary covers, with the latest check of each name at the PR/MR head.
pub(super) struct QaSummary<'a> {
    pub(super) runs: Vec<&'a QaRun>,
    pub(super) checks: Vec<CheckState<'a>>,
    pub(super) head: String,
}

/// Where the summary and its parts are on the host, as far as known.
#[derive(Default)]
pub(super) struct Links {
    pub(super) summary: Option<String>,
    /// The urls of the parts already posted, part 2 first.
    pub(super) parts: Vec<Option<String>>,
}

/// Which kinds of the covered runs approve the PR/MR head, and when the last of them did.
struct Approvals<'a> {
    review: bool,
    qa: bool,
    /// Whether every covered kind approves the head.
    all: bool,
    /// When every covered kind was last approved, at any head.
    since: Option<&'a str>,
}

impl Approvals<'_> {
    /// Whether closed or moved `thread` is no longer shown: the head is approved, or the thread
    /// settled by the time everything was.
    fn drops(&self, thread: &Thread) -> bool {
        self.all || self.since.is_some_and(|since| closed_at(thread) <= since)
    }
}

/// When `thread` was closed. A thread closed before the time was recorded falls back to its last
/// change.
fn closed_at(thread: &Thread) -> &str {
    thread.closed_at.as_deref().unwrap_or(&thread.updated_at)
}

/// The summary text, then the text of each part. `commits` are the PR/MR's commits, oldest
/// first, when git could list them; `head_local` is whether the PR/MR head is in the local
/// repository. A part the host still holds from an earlier, longer summary
/// is blanked.
pub(super) fn summary_texts(
    target: &Target,
    covered: &[Covered],
    qa: &QaSummary,
    commits: Option<&[CommitInfo]>,
    head_local: bool,
    links: &Links,
    attempt: &str,
) -> Vec<String> {
    let reported: Vec<&Thread> = target
        .threads
        .iter()
        .filter(|thread| {
            covered
                .iter()
                .any(|covered| raised_in(thread, &covered.run.id))
                || qa_check(qa, thread).is_some()
        })
        .collect();
    let open: Vec<&Thread> = reported
        .iter()
        .copied()
        .filter(|thread| is_open(thread))
        .collect();
    // A failed check is left to fix unless an open thread already says why. One that ran
    // against an older head is shown in the table only.
    let failing: Vec<&CheckState> = qa
        .checks
        .iter()
        .filter(|state| state.current && state.check.outcome.blocks_approval())
        .filter(|state| {
            !open
                .iter()
                .any(|thread| thread.check.as_deref() == Some(state.check.id.as_str()))
        })
        .collect();
    // A head the repository lacks cannot be listed or read: the headline names the newest head
    // a run read, and the PR/MR head is not reviewed.
    let unlisted =
        !head_local && !covered.is_empty() && !covered.iter().any(|run| run.head == qa.head);
    let (missing, shown) = match covered.last() {
        Some(newest) if unlisted => (vec![short(&qa.head).to_owned()], newest.head.as_str()),
        _ => (unreviewed(covered, commits), qa.head.as_str()),
    };
    let approvals = approvals(target, covered, qa);
    let mut items: Vec<String> = open
        .iter()
        .enumerate()
        .map(|(index, thread)| open_item(index + 1, qa, thread))
        .collect();
    items.extend(
        failing
            .iter()
            .enumerate()
            .map(|(index, state)| check_item(open.len() + index + 1, state)),
    );
    let head = headline(shown, items.len(), missing.len(), &approvals);
    let mut tail = String::new();
    not_reviewed(&mut tail, &missing);
    if unlisted {
        tail.push_str(" (not in the local repository)");
    }
    follow_ups(&mut tail, &reported, &approvals);
    details(&mut tail, qa, &reported, &approvals);
    footer(&mut tail, covered, qa);
    let marks = markers_block(covered, &reported, qa, attempt);
    layout(&head, &items, &tail, &marks, links, attempt)
}

/// Splits the summary into comments at item boundaries when it passes the host's limit.
fn layout(
    head: &str,
    items: &[String],
    tail: &str,
    marks: &str,
    links: &Links,
    attempt: &str,
) -> Vec<String> {
    let limit = MAX_BODY.saturating_sub(marks.chars().count());
    let cost = |item: &String| item.chars().count() + 1;
    let whole = join(head, items, tail);
    let mut texts = Vec::new();
    if whole.chars().count() <= limit {
        texts.push(whole);
    } else {
        let room = limit
            .saturating_sub(head.chars().count() + tail.chars().count())
            .saturating_sub(POINTER);
        let (first, rest) = take(items, room, cost);
        let mut groups = Vec::new();
        let mut rest = rest;
        while !rest.is_empty() {
            let (group, more) = take(rest, MAX_BODY - PART_ROOM, cost);
            // An item too big for a part is cut by `fit` rather than looped on.
            let (group, more) = if group.is_empty() {
                rest.split_at(1)
            } else {
                (group, more)
            };
            groups.push(group);
            rest = more;
        }
        let mut text = join(head, first, "");
        if let Some(pointer) = pointer(first.len() + 1, items.len(), groups.len(), links) {
            let _ = write!(text, "\n\n{pointer}");
        }
        text.push_str(tail);
        texts.push(text);
        for (index, group) in groups.iter().enumerate() {
            let lead = part_lead(index + 2, links);
            texts.push(format!("{lead}\n\n{}", group.join("\n").trim_end()));
        }
    }
    // A part the host holds from an earlier, longer summary says it is empty now.
    while texts.len() <= links.parts.len() {
        let lead = part_lead(texts.len() + 1, links);
        texts.push(format!("{lead} Nothing continues here now."));
    }
    for (index, text) in texts.iter_mut().enumerate() {
        let mark = match index {
            0 => marks.to_owned(),
            part => format!(
                "\n{}",
                markers::agent(&markers::summary_part_id(part + 1), Some(attempt))
            ),
        };
        fit(text, MAX_BODY.saturating_sub(mark.chars().count()));
        text.push_str(&mark);
    }
    texts
}

/// The headline, a blank line and the items, then the rest.
fn join(head: &str, items: &[String], tail: &str) -> String {
    let mut text = head.to_owned();
    if !items.is_empty() {
        let _ = write!(text, "\n\n{}", items.join("\n").trim_end());
    }
    text.push_str(tail);
    text
}

/// The items that fit in `room` characters, and the rest.
fn take(items: &[String], room: usize, cost: impl Fn(&String) -> usize) -> (&[String], &[String]) {
    let mut used = 0;
    let fits = items
        .iter()
        .take_while(|item| {
            used += cost(item);
            used <= room
        })
        .count();
    items.split_at(fits)
}

/// `Items 212–240 continue in [part 2](url)`, or `part 2 below` before the url is known.
fn pointer(first: usize, last: usize, parts: usize, links: &Links) -> Option<String> {
    if parts == 0 {
        return None;
    }
    let part = |number: usize| match links.parts.get(number - 2) {
        Some(Some(url)) => (format!("[part {number}]({url})"), true),
        _ => (format!("part {number}"), false),
    };
    let (start, known) = part(2);
    let mut where_ = start;
    let mut all_known = known;
    if parts > 1 {
        let (end, known) = part(parts + 1);
        let _ = write!(where_, " to {end}");
        all_known &= known;
    }
    let below = if all_known { "" } else { " below" };
    Some(format!("Items {first}–{last} continue in {where_}{below}."))
}

/// `Part 2 of the [agt summary](url).`
fn part_lead(number: usize, links: &Links) -> String {
    match &links.summary {
        Some(url) => format!("Part {number} of the [agt summary]({url})."),
        None => format!("Part {number} of the agt summary."),
    }
}

/// The only status: `**⚠️ 2 to fix** at `f48de58` · 1 commit not reviewed · 🔍 approved`.
fn headline(sha: &str, todo: usize, missing: usize, approvals: &Approvals) -> String {
    let sha = short(sha);
    let mut text = match (todo, missing) {
        (0, 0) => format!("**✅ Nothing to fix** at `{sha}`"),
        (0, missing) => format!(
            "**⏳ {} not reviewed** at `{sha}`",
            count(missing, "new commit")
        ),
        (todo, 0) => format!("**⚠️ {todo} to fix** at `{sha}`"),
        (todo, missing) => format!(
            "**⚠️ {todo} to fix** at `{sha}` · {} not reviewed",
            count(missing, "commit")
        ),
    };
    if approvals.review {
        text.push_str(" · 🔍 approved");
    }
    if approvals.qa {
        text.push_str(" · 🧪 QA approved");
    }
    text
}

/// The approvals of the PR/MR head: a review approval thread's, and a QA run's. A finding that
/// blocks approval, opened after it, withdraws the review approval and what it hides.
fn approvals<'a>(target: &'a Target, covered: &[Covered], qa: &QaSummary) -> Approvals<'a> {
    let approvals = || {
        target
            .threads
            .iter()
            .filter_map(|thread| Some((thread.approval.as_ref()?, thread.created_at.as_str())))
    };
    let qa_approvals = || {
        target
            .qa_runs
            .iter()
            .filter_map(|run| run.approval.as_ref())
    };
    let blocked = blocking_threads(target).next().is_some();
    let review = !blocked && approvals().any(|(approval, _)| approval.head == qa.head);
    let qa_now = qa_approvals().any(|approval| approval.head == qa.head);
    let reviewed = !blocked && (covered.is_empty() || review);
    let tested = qa.runs.is_empty() || qa_now;
    // The last approval that left every covered kind approved: the older of the latest of each.
    let latest = |times: Vec<&'a str>, needed: bool| match (needed, times.into_iter().max()) {
        (false, _) => Some(None),
        (true, None) => None,
        (true, Some(time)) => Some(Some(time)),
    };
    let since = latest(approvals().map(|(_, at)| at).collect(), !covered.is_empty())
        .zip(latest(
            qa_approvals()
                .map(|approval| approval.approved_at.as_str())
                .collect(),
            !qa.runs.is_empty(),
        ))
        .and_then(|(review, qa)| [review, qa].into_iter().flatten().min())
        .filter(|_| !blocked);
    Approvals {
        review,
        qa: qa_now,
        all: reviewed && tested,
        since,
    }
}

/// One open finding or QA thread: its first sentence, then severity, place and thread link.
/// One that is not posted as its own thread shows its whole text below.
fn open_item(number: usize, qa: &QaSummary, thread: &Thread) -> String {
    let first = thread.comments.first();
    let said = first.map_or("", |first| first.body.as_str());
    let name = qa_check(qa, thread);
    let emoji = if name.is_some() {
        "🧪"
    } else {
        kind_emoji(thread.kind)
    };
    let gist = gist(said);
    let mut meta = Vec::new();
    if let Some(severity) = thread.severity {
        meta.push(capitalized(severity.as_str()));
    }
    if thread.held {
        meta.push("⏸️ Waiting on the PR author".to_owned());
    }
    if let Some(name) = name {
        meta.push(format!("QA: {}", clip(name, CELL)));
    }
    meta.push(match &thread.anchor {
        Some(anchor) => format!("`{}`", location(anchor)),
        None => "whole PR".to_owned(),
    });
    if let Some(issue) = &thread.issue {
        meta.push(format!("Related: {issue}"));
    }
    let remote = first.and_then(|first| first.remote.as_ref());
    let mut quote = None;
    match remote.and_then(|remote| remote.url.as_deref()) {
        Some(url) => meta.push(format!("[thread]({url})")),
        // Text the first sentence already says in full needs no quote.
        None if remote.is_none() && gist != said.trim() => {
            meta.push("not posted as a thread, full text below".to_owned());
            let mut full = clip(said.trim(), MAX_NOTE);
            close_fence(&mut full);
            quote = Some(full);
        }
        None => {}
    }
    let pad = indent(number);
    let mut text = format!("{number}. {emoji} **{gist}**\n{pad}{}", meta.join(" · "));
    if let Some(full) = quote {
        text.push('\n');
        for line in full.lines() {
            let _ = write!(text, "\n{pad}> {line}");
        }
        text.push('\n');
    }
    text
}

/// A failed or blocked check no thread explains, with its environment and first evidence link.
fn check_item(number: usize, state: &CheckState) -> String {
    let check = state.check;
    let result = match check.outcome {
        Outcome::Blocked => "was blocked.",
        _ => "failed.",
    };
    let mut text = format!("{number}. 🧪 **{} {result}**", clip(&check.name, CELL));
    let mut meta = Vec::new();
    let environment = environment(state.run, check);
    if !environment.is_empty() {
        meta.push(clip(&environment.join(", "), CELL));
    }
    if let Some(url) = check.evidence.iter().find(|item| is_url(item)) {
        meta.push(format!("[evidence]({url})"));
    }
    if !meta.is_empty() {
        let _ = write!(text, "\n{}{}", indent(number), meta.join(" · "));
    }
    text
}

/// The short ids of the PR/MR commits no covered run read, oldest first.
fn unreviewed(covered: &[Covered], commits: Option<&[CommitInfo]>) -> Vec<String> {
    if covered.is_empty() {
        return Vec::new();
    }
    commits
        .unwrap_or_default()
        .iter()
        .filter(|commit| !reviewed(covered, commit))
        .map(|commit| short(&commit.sha).to_owned())
        .collect()
}

/// `Not reviewed: `a`, `b``, naming at most `MAX_NAMED` commits.
fn not_reviewed(text: &mut String, missing: &[String]) {
    if missing.is_empty() {
        return;
    }
    let named: Vec<String> = missing
        .iter()
        .take(MAX_NAMED)
        .map(|sha| format!("`{sha}`"))
        .collect();
    let _ = write!(text, "\n\nNot reviewed: {}", named.join(", "));
    if let Some(more) = missing
        .len()
        .checked_sub(MAX_NAMED)
        .filter(|more| *more > 0)
    {
        let _ = write!(text, " and {more} more");
    }
}

/// The findings left for follow-up issues: work that remains, so never collapsed. Approval
/// drops them.
fn follow_ups(text: &mut String, reported: &[&Thread], approvals: &Approvals) {
    let moved: Vec<(&Thread, &str)> = reported
        .iter()
        .filter(|thread| !approvals.drops(thread))
        .filter_map(|thread| moved_to(thread).map(|issue| (*thread, issue)))
        .collect();
    if moved.is_empty() {
        return;
    }
    text.push_str("\n\n**📋 Follow-up issues**\n");
    for (index, (thread, issue)) in moved.iter().enumerate() {
        let said = thread
            .comments
            .first()
            .map_or("", |first| first.body.as_str());
        let _ = write!(text, "\n{}. {issue} · {}", index + 1, gist(said));
    }
}

/// The collapsed QA checks and closed findings. Approval drops the closed findings.
fn details(text: &mut String, qa: &QaSummary, reported: &[&Thread], approvals: &Approvals) {
    let closed: Vec<&Thread> = reported
        .iter()
        .copied()
        .filter(|thread| thread.resolved && moved_to(thread).is_none())
        .filter(|thread| !approvals.drops(thread))
        .collect();
    let (posted, unposted): (Vec<&Thread>, Vec<&Thread>) = closed.iter().partition(|thread| {
        thread
            .comments
            .first()
            .is_some_and(|first| first.remote.is_some())
    });
    let mut label = Vec::new();
    if !qa.checks.is_empty() {
        label.push(format!(
            "{} 🧪 QA check{}",
            qa.checks.len(),
            plural(qa.checks.len())
        ));
    }
    if !closed.is_empty() {
        label.push(format!(
            "{} 🔍 closed finding{}",
            closed.len(),
            plural(closed.len())
        ));
    }
    if label.is_empty() {
        return;
    }
    let _ = write!(
        text,
        "\n\n<details><summary>{}</summary>\n\n",
        label.join(" · ")
    );
    if !qa.checks.is_empty() {
        qa_table(text, qa);
    }
    if !posted.is_empty() {
        if !qa.checks.is_empty() {
            text.push('\n');
        }
        text.push_str("| 🔍 Closed finding | Outcome |\n|---|---|\n");
        for thread in &posted {
            let first = thread.comments.first();
            let said = gist(first.map_or("", |first| first.body.as_str()));
            let emoji = if thread.check.is_some() {
                "🧪"
            } else {
                kind_emoji(thread.kind)
            };
            let finding = match first
                .and_then(|first| first.remote.as_ref())
                .and_then(|remote| remote.url.as_deref())
            {
                Some(url) => format!("[{}]({url})", cell(&said)),
                None => cell(&said),
            };
            let _ = writeln!(text, "| {emoji} {finding} | {} |", cell(&outcome(thread)));
        }
    }
    if !unposted.is_empty() {
        let (verb, pronoun) = match unposted.len() {
            1 => ("was", "it was"),
            _ => ("were", "they were"),
        };
        if !qa.checks.is_empty() || !posted.is_empty() {
            text.push('\n');
        }
        let _ = writeln!(
            text,
            "{} {verb} fixed or dropped before {pronoun} posted.",
            count(unposted.len(), "more finding")
        );
    }
    text.push_str("\n</details>");
}

/// The table of the latest check of each name.
fn qa_table(text: &mut String, qa: &QaSummary) {
    text.push_str("| 🧪 QA check | Result | Where | Evidence |\n|---|---|---|---|\n");
    for state in &qa.checks {
        let check = state.check;
        let emoji = match check.outcome {
            Outcome::Pass => "✅",
            Outcome::Fail => "❌",
            Outcome::Blocked => "⛔",
            Outcome::Skipped => "⏭️",
        };
        let old = if state.current { "" } else { " (out of date)" };
        let links: Vec<String> = check
            .evidence
            .iter()
            .filter(|item| is_url(item))
            .enumerate()
            .map(|(index, url)| format!("[{}]({url})", index + 1))
            .collect();
        let local = check.evidence.len() - links.len();
        let mut evidence = links.join(" ");
        if local > 0 {
            if !evidence.is_empty() {
                evidence.push(' ');
            }
            let _ = write!(evidence, "{local} not uploaded");
        }
        let _ = writeln!(
            text,
            "| {} | {emoji} {}{old} | {} | {evidence} |",
            cell(&clip(&check.name, CELL)),
            check.outcome.as_str(),
            cell(&clip(&environment(state.run, check).join(", "), CELL)),
        );
    }
}

/// `agt 0.19.0 · 🔍 review and 🧪 QA · 2 reviewers, claude-opus-5-5 · 14m · updated after each review`.
fn footer(text: &mut String, covered: &[Covered], qa: &QaSummary) {
    let mut reviewers: Vec<(&str, &str)> = Vec::new();
    let mut models: Vec<&str> = Vec::new();
    let mut total: Option<i64> = None;
    for covered in covered {
        let run = covered.run;
        for reviewer in run.passes.iter().flat_map(|pass| &pass.reviewers) {
            if reviewer.id == "fix" {
                continue;
            }
            if !reviewers.contains(&(run.id.as_str(), reviewer.id.as_str())) {
                reviewers.push((run.id.as_str(), reviewer.id.as_str()));
            }
            if !models.contains(&reviewer.model.as_str()) {
                models.push(reviewer.model.as_str());
            }
        }
        if let Some(secs) = run_spend(run)["total_secs"].as_i64() {
            *total.get_or_insert(0) += secs;
        }
    }
    let what = match (covered.is_empty(), qa.runs.is_empty()) {
        (false, false) => "🔍 review and 🧪 QA",
        (false, true) => "🔍 review",
        _ => "🧪 QA",
    };
    let mut parts = vec![
        format!("agt {}", env!("CARGO_PKG_VERSION")),
        what.to_owned(),
    ];
    if !reviewers.is_empty() {
        parts.push(format!(
            "{}, {}",
            count(reviewers.len(), "reviewer"),
            models.join(", ")
        ));
    }
    if let Some(secs) = total {
        parts.push(duration(secs));
    }
    parts.push("updated after each review".to_owned());
    let _ = write!(text, "\n\n> <sub>{}</sub>", parts.join(" · "));
}

/// The indent that keeps a line in the list item: as wide as `10. `.
fn indent(number: usize) -> String {
    " ".repeat(number.to_string().len() + 2)
}

fn plural(number: usize) -> &'static str {
    if number == 1 { "" } else { "s" }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Whether a pass of a covered run read `commit`, by its id or the same patch.
fn reviewed(covered: &[Covered], commit: &CommitInfo) -> bool {
    let same = |patch: &CommitPatch| {
        patch.sha == commit.sha || (patch.patch_id.is_some() && patch.patch_id == commit.patch_id)
    };
    covered.iter().any(|covered| {
        covered
            .run
            .passes
            .iter()
            .any(|pass| pass.kind != PassKind::Reveal && pass.commits.iter().any(same))
    })
}

/// The record marker of every covered run, newest first, then the marker of this summary.
fn markers_block(
    covered: &[Covered],
    reported: &[&Thread],
    qa: &QaSummary,
    attempt: &str,
) -> String {
    let mut marks = String::new();
    for run in qa.runs.iter().rev() {
        let _ = write!(marks, "\n{}", markers::qa_record(&run.id, &qa.head));
    }
    for covered in covered.iter().rev() {
        let open = reported
            .iter()
            .filter(|thread| raised_in(thread, &covered.run.id) && is_open(thread))
            .count();
        let _ = write!(
            marks,
            "\n{}",
            markers::record(&covered.run.id, &covered.head, covered.at_head, open)
        );
    }
    let _ = write!(
        marks,
        "\n{}",
        markers::agent(markers::SUMMARY_ID, Some(attempt))
    );
    marks
}

/// Whether `thread` is a finding of `run`.
fn raised_in(thread: &Thread, run: &str) -> bool {
    thread
        .finding
        .as_ref()
        .is_some_and(|finding| finding.run == run)
}

/// The name of the covered QA check that found `thread`.
fn qa_check<'a>(qa: &QaSummary<'a>, thread: &Thread) -> Option<&'a str> {
    let check = thread.check.as_deref()?;
    qa.runs
        .iter()
        .flat_map(|run| &run.checks)
        .find(|known| known.id == check)
        .map(|known| known.name.as_str())
}

/// The issue a settled thread was moved to.
fn moved_to(thread: &Thread) -> Option<&str> {
    match &thread.resolution {
        Some(Resolution::Moved { issue }) if thread.resolved => Some(issue),
        _ => None,
    }
}

fn is_url(text: &str) -> bool {
    text.starts_with("https://") || text.starts_with("http://")
}

/// Cuts `text` at a line to at most `max` characters, closing the code block and sections the
/// cut leaves open.
fn fit(text: &mut String, max: usize) {
    const CUT: &str = "\n✂️ The rest is cut to fit the host's size limit.";
    if text.chars().count() <= max {
        return;
    }
    // Room for the closing lines and the notice.
    let room = max.saturating_sub(1000);
    let mut kept = String::new();
    let mut length = 0;
    let mut open = 0usize;
    for line in text.lines() {
        length += line.chars().count() + 1;
        if length > room {
            break;
        }
        open += line.matches("<details>").count();
        open = open.saturating_sub(line.matches("</details>").count());
        kept.push_str(line);
        kept.push('\n');
    }
    close_fence(&mut kept);
    for _ in 0..open {
        kept.push_str("\n</details>\n");
    }
    kept.push_str(CUT);
    *text = kept;
}

/// Closes the code block a cut `text` leaves open, so what follows does not render as code.
fn close_fence(text: &mut String) {
    let fences = text
        .lines()
        .filter(|line| line.trim_start().starts_with("```"))
        .count();
    if fences % 2 == 1 {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("```\n");
    }
}

/// The first sentence of a finding's text. A posted body, as sync stores it, opens with the
/// location line of a file outside the diff and the kind heading, `**🐛 bug (high):**` or on its
/// own line `**🐛 bug (high)**`; both go.
fn gist(body: &str) -> String {
    let mut lines = body.lines().map(str::trim).filter(|line| !line.is_empty());
    let mut line = lines.next().unwrap_or_default();
    if line.len() > 1 && line.starts_with('`') && line.ends_with('`') {
        line = lines.next().unwrap_or_default();
    }
    // The kind heading on a line of its own, `**🐛 bug (high)**`.
    if line.starts_with("**")
        && line.ends_with("**")
        && line.chars().nth(2).is_some_and(|first| !first.is_ascii())
    {
        line = lines.next().unwrap_or_default();
    }
    if let Some((_, rest)) = line
        .strip_prefix("**")
        .and_then(|rest| rest.split_once(":** "))
    {
        line = rest;
    }
    let end = [". ", "? ", "! "]
        .iter()
        .filter_map(|stop| line.find(stop))
        .min();
    let sentence = end.map_or(line, |end| &line[..=end]);
    clip(sentence, CELL)
}

/// `text` cut to `max` characters, ending in `…` when cut.
fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

/// Text that stays in one table cell.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace(['\r', '\n'], " ")
}

fn count(number: usize, noun: &str) -> String {
    match number {
        1 => format!("1 {noun}"),
        number => format!("{number} {noun}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gists_and_cells_read_as_text() {
        assert_eq!(cell("a | b\nc"), "a \\| b c");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(gist("\nFirst one. Second one.\nMore."), "First one.");
        assert_eq!(
            gist("`docs/a.md:3`\n\n**❓ question:** Why so? More.\n\n> <sub>m</sub>"),
            "Why so?"
        );
        assert_eq!(gist("**question:** Old heading."), "Old heading.");
        assert_eq!(
            gist("`docs/a.md:3`\n**❓ question (high)**\nWhy so? More."),
            "Why so?"
        );
        assert_eq!(gist("**Whole bold line.**\nMore."), "**Whole bold line.**");
        assert_eq!(count(1, "commit"), "1 commit");
        assert_eq!(count(3, "commit"), "3 commits");
    }

    fn approvals(review: bool, qa: bool, all: bool, since: Option<&str>) -> Approvals<'_> {
        Approvals {
            review,
            qa,
            all,
            since,
        }
    }

    #[test]
    fn the_headline_is_the_only_status() {
        let none = approvals(false, false, false, None);
        let sha = "f48de58aa0";
        assert_eq!(
            headline(sha, 0, 0, &none),
            "**✅ Nothing to fix** at `f48de58`"
        );
        assert_eq!(headline(sha, 2, 0, &none), "**⚠️ 2 to fix** at `f48de58`");
        assert_eq!(
            headline(sha, 2, 1, &none),
            "**⚠️ 2 to fix** at `f48de58` · 1 commit not reviewed"
        );
        assert_eq!(
            headline(sha, 0, 1, &none),
            "**⏳ 1 new commit not reviewed** at `f48de58`"
        );
        assert_eq!(
            headline(sha, 0, 3, &approvals(true, true, true, None)),
            "**⏳ 3 new commits not reviewed** at `f48de58` · 🔍 approved · 🧪 QA approved"
        );
        assert_eq!(
            headline(sha, 0, 0, &approvals(false, true, false, None)),
            "**✅ Nothing to fix** at `f48de58` · 🧪 QA approved"
        );
    }

    #[test]
    fn unreviewed_commits_are_named_up_to_a_limit() {
        let names = |count: usize| {
            let missing: Vec<String> = (1..=count).map(|number| format!("c{number}")).collect();
            let mut text = String::new();
            not_reviewed(&mut text, &missing);
            text
        };
        assert_eq!(names(0), "");
        assert_eq!(names(2), "\n\nNot reviewed: `c1`, `c2`");
        assert_eq!(names(5), "\n\nNot reviewed: `c1`, `c2`, `c3`, `c4`, `c5`");
        assert_eq!(
            names(8),
            "\n\nNot reviewed: `c1`, `c2`, `c3`, `c4`, `c5` and 3 more"
        );
    }

    #[test]
    fn closed_threads_stay_dropped_after_a_later_push() {
        let thread = |updated: &str| {
            Thread::new(
                "t1".into(),
                crate::store::model::ThreadType::Bug,
                Vec::new(),
                "2026-01-01T00:00:00Z".into(),
                updated.into(),
            )
        };
        let approved = approvals(true, false, true, Some("2026-01-02T00:00:00Z"));
        assert!(approved.drops(&thread("2026-01-03T00:00:00Z")));
        let pushed = approvals(false, false, false, Some("2026-01-02T00:00:00Z"));
        assert!(pushed.drops(&thread("2026-01-02T00:00:00Z")));
        assert!(!pushed.drops(&thread("2026-01-03T00:00:00Z")));
        assert!(!approvals(false, false, false, None).drops(&thread("2026-01-01T00:00:00Z")));
        // A sync bumps `updated_at` of a closed thread; when it closed is what counts.
        let mut synced = thread("2026-01-04T00:00:00Z");
        synced.closed_at = Some("2026-01-01T00:00:00Z".into());
        assert!(pushed.drops(&synced));
    }

    #[test]
    fn a_blocking_finding_withdraws_the_approval() {
        let mut target: Target = serde_json::from_value(serde_json::json!({
            "version": 1, "revision": 1, "key": "k", "repo": "r", "kind": "branch",
            "branch": null, "branch_birth": null, "base": null, "head": null,
            "created_at": "t", "updated_at": "t", "review": null,
            "runs": [], "records": [], "threads": [],
        }))
        .unwrap();
        let thread = |id: &str| {
            Thread::new(
                id.into(),
                crate::store::model::ThreadType::Bug,
                Vec::new(),
                "2026-01-01T00:00:00Z".into(),
                "2026-01-01T00:00:00Z".into(),
            )
        };
        let mut approval = thread("a1");
        approval.resolved = true;
        approval.approval = Some(crate::store::model::Approval {
            head: "abc".into(),
            formal: false,
            approved: false,
            refused: None,
        });
        target.threads.push(approval);
        let qa = QaSummary {
            runs: Vec::new(),
            checks: Vec::new(),
            head: "abc".into(),
        };
        let approved = super::approvals(&target, &[], &qa);
        assert!(approved.review && approved.all);
        // A finding opened at the same head leaves nothing approved.
        target.threads.push(thread("f1"));
        let blocked = super::approvals(&target, &[], &qa);
        assert!(!blocked.review && !blocked.all && blocked.since.is_none());
    }

    #[test]
    fn items_are_indented_under_their_number() {
        assert_eq!(indent(3), "   ");
        assert_eq!(indent(12), "    ");
    }

    #[test]
    fn a_long_summary_continues_in_parts_at_item_boundaries() {
        let items: Vec<String> = (1..=300)
            .map(|number| format!("{number}. ✏️ **Item.**\n   whole PR\n{}", "x".repeat(400)))
            .collect();
        let head = "**⚠️ 300 to fix** at `abc1234`";
        let tail = "\n\n> <sub>agt</sub>";
        let marks = "\n<!-- agent-review id=summary attempt=a -->";
        let links = Links::default();
        let texts = layout(head, &items, tail, marks, &links, "a");
        assert!(texts.len() > 2, "{}", texts.len());
        assert!(texts.iter().all(|text| text.chars().count() <= MAX_BODY));
        assert!(texts[0].starts_with(head));
        assert!(texts[0].ends_with(marks));
        assert!(texts[0].contains("> <sub>agt</sub>"));
        assert!(
            texts[0].contains("continue in part 2 to part"),
            "{}",
            &texts[0][texts[0].len() - 300..]
        );
        assert!(texts[0].contains(" below."));
        assert!(texts[1].starts_with("Part 2 of the agt summary.\n\n"));
        assert!(texts[1].ends_with("attempt=a -->"));
        assert!(texts[1].contains("<!-- agent-review id=summary/p2 "));
        assert!(!texts[1].contains("to fix"));
        // Every item is somewhere, once, in order.
        let all = texts.join("\n");
        let mut at = 0;
        for number in 1..=300 {
            let found = all[at..].find(&format!("\n{number}. ✏️")).unwrap();
            at += found + 1;
        }
        // The first comment says which items moved.
        let first = texts[0].matches("whole PR").count();
        assert!(texts[0].contains(&format!("Items {}–300 continue in", first + 1)));
    }

    #[test]
    fn part_pointers_link_once_the_urls_are_known() {
        let known = Links {
            summary: Some("https://h/1".into()),
            parts: vec![Some("https://h/2".into()), Some("https://h/3".into())],
        };
        assert_eq!(
            pointer(5, 9, 1, &known).as_deref(),
            Some("Items 5–9 continue in [part 2](https://h/2).")
        );
        assert_eq!(
            pointer(5, 9, 2, &known).as_deref(),
            Some("Items 5–9 continue in [part 2](https://h/2) to [part 3](https://h/3).")
        );
        assert_eq!(
            pointer(5, 9, 1, &Links::default()).as_deref(),
            Some("Items 5–9 continue in part 2 below.")
        );
        assert_eq!(pointer(5, 9, 0, &known), None);
        assert_eq!(
            part_lead(2, &known),
            "Part 2 of the [agt summary](https://h/1)."
        );
    }

    #[test]
    fn a_part_the_summary_no_longer_needs_is_blanked() {
        let links = Links {
            summary: Some("https://h/1".into()),
            parts: vec![Some("https://h/2".into())],
        };
        let texts = layout(
            "**✅ Nothing to fix** at `abc`",
            &[],
            "",
            "\n<!-- m -->",
            &links,
            "a",
        );
        assert_eq!(texts.len(), 2);
        assert!(
            texts[1].starts_with(
                "Part 2 of the [agt summary](https://h/1). Nothing continues here now."
            ),
            "{}",
            texts[1]
        );
    }

    #[test]
    fn a_cut_note_closes_its_code_block() {
        let mut note = clip("Text\n```\nlet a = 1;\nlet b = 2;\n```", 20);
        close_fence(&mut note);
        assert_eq!(note, "Text\n```\nlet a = 1;…\n```\n");
        let mut whole = "Text\n```\ncode\n```".to_owned();
        close_fence(&mut whole);
        assert_eq!(whole, "Text\n```\ncode\n```");
    }

    #[test]
    fn a_long_summary_is_cut_and_closed() {
        let mut short = "Short.".to_owned();
        fit(&mut short, 100);
        assert_eq!(short, "Short.");
        let mut text = "Head.\n\n<details><summary>Notes</summary>\n\n```\n".to_owned();
        for line in 0..5000 {
            let _ = writeln!(text, "line {line} of a long note");
        }
        text.push_str("```\n\n</details>");
        fit(&mut text, 10_000);
        assert!(text.chars().count() <= 10_000);
        assert!(text.starts_with("Head.\n"));
        assert!(
            text.ends_with("```\n\n</details>\n\n✂️ The rest is cut to fit the host's size limit."),
            "{}",
            &text[text.len() - 100..]
        );
    }
}
