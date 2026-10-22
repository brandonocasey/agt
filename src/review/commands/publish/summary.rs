//! The living summary: one comment per PR/MR that `publish` posts once and then edits in place.
//! It leads with what is left to fix and the findings moved to follow-up issues, collapses the
//! QA checks and the settled findings, and holds the record marker of every run it covers.

use std::fmt::Write as _;

use super::render::{duration, kind_heading, location, outcome, short};
use crate::forge::markers;
use crate::git::CommitInfo;
use crate::review::commands::approve::is_open;
use crate::review::commands::qa::{CheckState, environment};
use crate::review::commands::usage::run_spend;
use crate::store::model::{CommitPatch, Outcome, PassKind, QaRun, Resolution, Run, Target, Thread};

/// Longest finding gist in a list item or table cell.
const CELL: usize = 90;
/// Longest text shown for an open finding that is not posted as its own thread.
const MAX_NOTE: usize = 2000;
/// Longest summary, markers included: GitHub takes at most 65,536 characters.
const MAX_BODY: usize = 65_000;
/// Most unreviewed commits the status line names.
const MAX_NAMED: usize = 5;

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
    pub(super) approved: bool,
}

/// The summary text. `commits` are the PR/MR's commits, oldest first, when git could list them.
pub(super) fn summary_body(
    target: &Target,
    covered: &[Covered],
    qa: &QaSummary,
    commits: Option<&[CommitInfo]>,
    attempt: &str,
) -> String {
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
    // A failed or stale check is left to fix unless an open thread already says why.
    let failing: Vec<&CheckState> = qa
        .checks
        .iter()
        .filter(|state| state.check.outcome.blocks_approval() || !state.current)
        .filter(|state| {
            !open
                .iter()
                .any(|thread| thread.check.as_deref() == Some(state.check.id.as_str()))
        })
        .collect();
    let mut text = headline(covered, qa, open.len() + failing.len());
    for (index, thread) in open.iter().enumerate() {
        open_item(&mut text, index + 1, qa, thread);
    }
    for (index, state) in failing.iter().enumerate() {
        check_item(&mut text, open.len() + index + 1, state);
    }
    status(&mut text, covered, qa, &reported, commits);
    follow_ups(&mut text, qa, &reported);
    details(&mut text, qa, &reported);
    footer(&mut text, covered);
    let marks = markers_block(covered, &reported, qa, attempt);
    fit(&mut text, MAX_BODY.saturating_sub(marks.chars().count()));
    text.push_str(&marks);
    text
}

/// `**⚠️ 2 to fix** · agent review and QA at `f48de58``.
fn headline(covered: &[Covered], qa: &QaSummary, todo: usize) -> String {
    let what = match (covered.is_empty(), qa.runs.is_empty()) {
        (false, false) => "agent review and QA",
        (false, true) => "agent review",
        _ => "agent QA",
    };
    let head = covered
        .last()
        .map_or(qa.head.as_str(), |covered| covered.head.as_str());
    let verdict = match todo {
        0 => "✅ Nothing to fix".to_owned(),
        todo => format!("⚠️ {todo} to fix"),
    };
    let mut text = format!("**{verdict}** · {what} at `{}`", short(head));
    if todo > 0 {
        text.push('\n');
    }
    text
}

/// One open finding or QA thread: its kind, place and first sentence, linked to its thread.
/// One that is not posted as its own thread shows its whole text here.
fn open_item(text: &mut String, number: usize, qa: &QaSummary, thread: &Thread) {
    let first = thread.comments.first();
    let said = first.map_or("", |first| first.body.as_str());
    let _ = write!(text, "\n{number}. **{}**", kind_heading(thread));
    if let Some(anchor) = &thread.anchor {
        let _ = write!(text, " · `{}`", location(anchor));
    }
    if let Some(name) = qa_check(qa, thread) {
        let _ = write!(text, " · QA: {}", clip(name, CELL));
    }
    let gist = gist(said);
    let _ = write!(text, " · {gist}");
    if thread.held {
        text.push_str(" ⏸️ Waits for the author.");
    }
    if let Some(issue) = &thread.issue {
        let _ = write!(text, " Related: {issue}.");
    }
    let remote = first.and_then(|first| first.remote.as_ref());
    match remote.and_then(|remote| remote.url.as_deref()) {
        Some(url) => {
            let _ = write!(text, " [thread]({url})");
        }
        // Text the first sentence already says in full needs no quote.
        None if remote.is_none() && gist != said.trim() => {
            let mut full = clip(said.trim(), MAX_NOTE);
            close_fence(&mut full);
            text.push('\n');
            for line in full.lines() {
                let _ = write!(text, "\n   > {line}");
            }
            text.push('\n');
        }
        None => {}
    }
}

/// A failed, blocked or stale check, with its first evidence link.
fn check_item(text: &mut String, number: usize, state: &CheckState) {
    let check = state.check;
    let result = if state.current {
        check.outcome.as_str()
    } else {
        "stale"
    };
    let _ = write!(
        text,
        "\n{number}. **🧪 QA {result}: {}**",
        clip(&check.name, CELL)
    );
    let environment = environment(state.run, check);
    if !environment.is_empty() {
        let _ = write!(text, " · {}", clip(&environment.join(", "), CELL));
    }
    if !state.current {
        text.push_str(" · tested an older head");
    }
    if let Some(url) = check.evidence.iter().find(|item| is_url(item)) {
        let _ = write!(text, " [evidence]({url})");
    }
}

/// The one line of what is fine and what else needs a look: QA, settled findings and commits
/// no review read.
fn status(
    text: &mut String,
    covered: &[Covered],
    qa: &QaSummary,
    reported: &[&Thread],
    commits: Option<&[CommitInfo]>,
) {
    let mut parts = Vec::new();
    if !qa.checks.is_empty() {
        let passed = qa
            .checks
            .iter()
            .filter(|state| state.current && state.check.outcome == Outcome::Pass)
            .count();
        let mark = if passed == qa.checks.len() {
            "✓"
        } else {
            "⚠"
        };
        parts.push(format!("{mark} QA {passed} of {} pass", qa.checks.len()));
    }
    if qa.approved {
        parts.push("✓ QA approved".to_owned());
    }
    let settled = reported
        .iter()
        .filter(|thread| thread.resolved && moved_to(thread).is_none())
        .count();
    if settled > 0 {
        parts.push(format!("✓ {} settled", count(settled, "finding")));
    }
    if !covered.is_empty() {
        let missing: Vec<String> = commits
            .unwrap_or_default()
            .iter()
            .filter(|commit| !reviewed(covered, commit))
            .map(|commit| format!("`{}`", short(&commit.sha)))
            .collect();
        match missing.len() {
            0 => {}
            1..=MAX_NAMED => parts.push(format!("⚠ {} not reviewed", missing.join(", "))),
            more => parts.push(format!(
                "⚠ {} and {} more not reviewed",
                missing[..MAX_NAMED].join(", "),
                more - MAX_NAMED
            )),
        }
    }
    if !parts.is_empty() {
        let _ = write!(text, "\n\n{}", parts.join(" · "));
    }
}

/// The findings left for follow-up issues: work that remains, so never collapsed.
fn follow_ups(text: &mut String, qa: &QaSummary, reported: &[&Thread]) {
    let moved: Vec<(&Thread, &str)> = reported
        .iter()
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
        let from = match (&thread.anchor, qa_check(qa, thread)) {
            (Some(anchor), _) => format!(" (from `{}`)", location(anchor)),
            (None, Some(name)) => format!(" (from QA: {})", clip(name, CELL)),
            (None, None) => String::new(),
        };
        let _ = write!(text, "\n{}. {issue} · {}{from}", index + 1, gist(said));
    }
}

/// The collapsed QA checks and settled findings.
fn details(text: &mut String, qa: &QaSummary, reported: &[&Thread]) {
    let settled: Vec<&Thread> = reported
        .iter()
        .copied()
        .filter(|thread| thread.resolved && moved_to(thread).is_none())
        .collect();
    let (posted, unposted): (Vec<&Thread>, Vec<&Thread>) = settled.iter().partition(|thread| {
        thread
            .comments
            .first()
            .is_some_and(|first| first.remote.is_some())
    });
    if qa.checks.is_empty() && settled.is_empty() {
        return;
    }
    text.push_str("\n\n<details><summary>Details</summary>\n\n");
    if !qa.checks.is_empty() {
        qa_table(text, qa);
    }
    if !posted.is_empty() {
        if !qa.checks.is_empty() {
            text.push('\n');
        }
        text.push_str("| Settled finding | Outcome |\n|---|---|\n");
        for thread in &posted {
            let first = thread.comments.first();
            let said = gist(first.map_or("", |first| first.body.as_str()));
            let finding = match first
                .and_then(|first| first.remote.as_ref())
                .and_then(|remote| remote.url.as_deref())
            {
                Some(url) => format!("[{}]({url})", cell(&said)),
                None => cell(&said),
            };
            let _ = writeln!(
                text,
                "| {}: {finding} | {} |",
                kind_heading(thread),
                cell(&outcome(thread))
            );
        }
    }
    if !unposted.is_empty() {
        let _ = writeln!(
            text,
            "\n{} fixed or dismissed before posting, so not posted.",
            count(unposted.len(), "more finding")
        );
    }
    text.push_str("\n</details>");
}

/// The table of the latest check of each name.
fn qa_table(text: &mut String, qa: &QaSummary) {
    text.push_str("| QA check | Result | Environment | Evidence |\n|---|---|---|---|\n");
    for state in &qa.checks {
        let check = state.check;
        let emoji = match check.outcome {
            Outcome::Pass => "✅",
            Outcome::Fail => "❌",
            Outcome::Blocked => "⛔",
            Outcome::Skipped => "⏭️",
        };
        let stale = if state.current { "" } else { " (stale)" };
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
            let _ = write!(evidence, "{local} local");
        }
        let _ = writeln!(
            text,
            "| {} | {emoji} {}{stale} | {} | {evidence} |",
            cell(&clip(&check.name, CELL)),
            check.outcome.as_str(),
            cell(&clip(&environment(state.run, check).join(", "), CELL)),
        );
    }
}

/// `agt 0.19.0 · 2 reviewers, claude-opus-5-5 · 14m`.
fn footer(text: &mut String, covered: &[Covered]) {
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
    let mut parts = vec![format!("agt {}", env!("CARGO_PKG_VERSION"))];
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
    parts.push("edited on each publish".to_owned());
    let _ = write!(text, "\n\n> <sub>{}</sub>", parts.join(" · "));
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
/// location line of a file outside the diff and the `**🐛 bug (high):**` heading; both go.
fn gist(body: &str) -> String {
    let mut lines = body.lines().map(str::trim).filter(|line| !line.is_empty());
    let mut line = lines.next().unwrap_or_default();
    if line.len() > 1 && line.starts_with('`') && line.ends_with('`') {
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
        assert_eq!(count(1, "commit"), "1 commit");
        assert_eq!(count(3, "commit"), "3 commits");
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
