//! The living summary: one comment per PR/MR that `publish` posts once and then edits in place.
//! It lists the findings, the commits and how each was reviewed, the review steps and the
//! agents, and holds the record marker of every run it covers.

use std::fmt::Write as _;

use super::render::{
    duration, finding_pass, harness_label, kind_heading, location, model_label, outcome,
    pass_label, session_role, short, short_id,
};
use crate::core::clock;
use crate::forge::markers;
use crate::git::CommitInfo;
use crate::review::commands::approve::is_open;
use crate::review::commands::qa::{CheckState, environment};
use crate::review::commands::usage::run_spend;
use crate::store::model::{
    Anchor, Author, CommitPatch, Outcome, Pass, PassKind, QaRun, Resolution, Reviewer, Run, Side,
    Target, Thread,
};

/// Longest finding gist or commit subject in a table cell.
const CELL: usize = 90;
/// Most commits the commits table lists.
const MAX_COMMITS: usize = 100;
/// Longest text shown for a finding that was not posted inline.
const MAX_NOTE: usize = 2000;
/// Longest summary, markers included: GitHub takes at most 65,536 characters.
const MAX_BODY: usize = 65_000;

/// A run the summary covers, with the head and count its record marker names.
pub(super) struct Covered<'a> {
    pub(super) run: &'a Run,
    pub(super) head: String,
    pub(super) at_head: usize,
}

/// The summary text. `commits` are the PR/MR's commits, oldest first, when git could list them.
pub(super) fn summary_body(
    target: &Target,
    covered: &[Covered],
    qa: &QaSummary,
    commits: Option<&[CommitInfo]>,
    attempt: &str,
) -> String {
    let findings: Vec<&Thread> = target
        .threads
        .iter()
        .filter(|thread| {
            covered
                .iter()
                .any(|covered| raised_in(thread, &covered.run.id, None))
        })
        .collect();
    let reviews: Vec<(bool, String)> = commits
        .unwrap_or_default()
        .iter()
        .map(|commit| commit_review(covered, commit))
        .collect();
    let commits = commits.filter(|commits| !commits.is_empty());
    let mut text = String::new();
    if !covered.is_empty() {
        text = headline(covered, &findings, commits, &reviews);
        if !findings.is_empty() {
            findings_table(&mut text, target, &findings);
        }
        if let Some(commits) = commits {
            commit_table(&mut text, commits, &reviews);
        }
        steps(&mut text, covered, commits.unwrap_or_default(), &findings);
        agents(&mut text, covered, &findings);
    }
    if !qa.runs.is_empty() {
        qa_section(&mut text, target, qa);
    }
    let marks = markers_block(covered, &findings, qa, attempt);
    fit(&mut text, MAX_BODY.saturating_sub(marks.chars().count()));
    text.push_str(&marks);
    text
}

/// The QA runs a summary covers, with the latest check of each name at the PR/MR head.
pub(super) struct QaSummary<'a> {
    pub(super) runs: Vec<&'a QaRun>,
    pub(super) checks: Vec<CheckState<'a>>,
    pub(super) head: String,
    pub(super) approved: bool,
}

/// The QA verdict line, the checks table and the threads the checks found.
fn qa_section(text: &mut String, target: &Target, qa: &QaSummary) {
    let threads: Vec<&Thread> = target
        .threads
        .iter()
        .filter(|thread| {
            thread.check.as_deref().is_some_and(|check| {
                qa.runs.iter().any(|run| {
                    check
                        .strip_prefix(run.id.as_str())
                        .is_some_and(|rest| rest.starts_with('.'))
                })
            })
        })
        .collect();
    // Unresolved, as the findings tally counts them: a posted general comment the host cannot
    // resolve still needs an answer here.
    let open = threads.iter().filter(|thread| !thread.resolved).count();
    let stale = qa.checks.iter().filter(|state| !state.current).count();
    let blocked = qa
        .checks
        .iter()
        .any(|state| state.check.outcome.blocks_approval());
    let emoji = if blocked || stale > 0 || open > 0 {
        "⚠️"
    } else {
        "✅"
    };
    let mut results: Vec<String> = [
        Outcome::Pass,
        Outcome::Fail,
        Outcome::Blocked,
        Outcome::Skipped,
    ]
    .into_iter()
    .filter_map(|wanted| {
        let number = qa
            .checks
            .iter()
            .filter(|state| state.check.outcome == wanted)
            .count();
        (number > 0).then(|| format!("{number} {}", wanted.as_str()))
    })
    .collect();
    if stale > 0 {
        results.push(format!("{stale} stale"));
    }
    if open > 0 {
        results.push(format!("{} open", count(open, "finding")));
    }
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    let _ = write!(
        text,
        "**{emoji} Agent QA at {}: {}: {}.**",
        short(&qa.head),
        count(qa.checks.len(), "check"),
        results.join(", ")
    );
    if qa.approved {
        text.push_str(" Approved.");
    }
    open_section(text, &format!("🧪 {}", count(qa.checks.len(), "check")));
    text.push_str("| Check | Result | Environment | Evidence |\n|---|---|---|---|\n");
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
            .filter(|item| item.starts_with("https://") || item.starts_with("http://"))
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
    close_section(text);
    if threads.is_empty() {
        return;
    }
    open_section(
        text,
        &format!(
            "🔎 {}: {}",
            count(threads.len(), "QA finding"),
            tally(&threads)
        ),
    );
    text.push_str("| Finding | Check | Outcome |\n|---|---|---|\n");
    for thread in &threads {
        let check = thread.check.as_deref().unwrap_or_default();
        let name = qa
            .runs
            .iter()
            .flat_map(|run| &run.checks)
            .find(|known| known.id == check)
            .map_or(check, |known| known.name.as_str());
        let said = thread
            .comments
            .first()
            .map(|first| first.body.as_str())
            .unwrap_or_default();
        let _ = writeln!(
            text,
            "| **{}:** {} | {} | {} |",
            kind_heading(thread),
            cell(&gist(said)),
            cell(&clip(name, CELL)),
            cell(&outcome(thread)),
        );
    }
    close_section(text);
}

/// Whether `thread` is a finding of `run` (of pass `pass` when given).
fn raised_in(thread: &Thread, run: &str, pass: Option<&str>) -> bool {
    thread
        .finding
        .as_ref()
        .is_some_and(|finding| finding.run == run && pass.is_none_or(|pass| finding.pass == pass))
}

/// The opening of a collapsed section, up to the start of its content.
fn open_section(text: &mut String, title: &str) {
    let _ = write!(text, "\n\n<details><summary>{title}</summary>\n\n");
}

fn close_section(text: &mut String) {
    text.push_str("\n</details>");
}

/// The verdict line: open findings at the head, the findings tally, and how many commits were
/// reviewed.
fn headline(
    covered: &[Covered],
    findings: &[&Thread],
    commits: Option<&[CommitInfo]>,
    reviews: &[(bool, String)],
) -> String {
    let open = findings.iter().filter(|thread| is_open(thread)).count();
    let head = covered.last().map_or("", |covered| covered.head.as_str());
    let emoji = if open == 0 { "✅" } else { "⚠️" };
    let mut text = format!("**{emoji} Agent review: {open} open at {}.** ", short(head));
    if findings.is_empty() {
        text.push_str("No findings.");
    } else {
        let _ = write!(
            text,
            "{}: {}.",
            count(findings.len(), "finding"),
            tally(findings)
        );
    }
    if let Some(commits) = commits {
        let missing: Vec<&str> = commits
            .iter()
            .zip(reviews)
            .filter(|(_, (reviewed, _))| !reviewed)
            .map(|(commit, _)| short(&commit.sha))
            .collect();
        let _ = write!(
            text,
            " {} of {} reviewed",
            commits.len() - missing.len(),
            count(commits.len(), "commit")
        );
        match missing.len() {
            0 => text.push('.'),
            1..=5 => {
                let _ = write!(text, "; {} had no review.", and_list(&missing));
            }
            more => {
                let _ = write!(
                    text,
                    "; {} and {} more had no review.",
                    missing[..5].join(", "),
                    more - 5
                );
            }
        }
    }
    text
}

/// The collapsed commits table, each commit with how it was reviewed.
fn commit_table(text: &mut String, commits: &[CommitInfo], reviews: &[(bool, String)]) {
    let reviewed = reviews.iter().filter(|(reviewed, _)| *reviewed).count();
    open_section(
        text,
        &format!("📦 {}: {reviewed} reviewed", count(commits.len(), "commit")),
    );
    text.push_str("| Commit | Subject | Review |\n|---|---|---|\n");
    for (commit, (_, review)) in commits.iter().zip(reviews).take(MAX_COMMITS) {
        let _ = writeln!(
            text,
            "| {} | {} | {review} |",
            short(&commit.sha),
            cell(&clip(&commit.subject, CELL))
        );
    }
    if commits.len() > MAX_COMMITS {
        let _ = writeln!(text, "\n{} more not listed.", commits.len() - MAX_COMMITS);
    }
    close_section(text);
}

/// The record marker of every covered run, newest first, then the marker of this summary.
fn markers_block(
    covered: &[Covered],
    findings: &[&Thread],
    qa: &QaSummary,
    attempt: &str,
) -> String {
    let mut marks = String::new();
    for run in qa.runs.iter().rev() {
        let _ = write!(marks, "\n{}", markers::qa_record(&run.id, &qa.head));
    }
    for covered in covered.iter().rev() {
        let open = findings
            .iter()
            .filter(|thread| raised_in(thread, &covered.run.id, None) && is_open(thread))
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

/// The collapsed findings table, then the text of each finding that was not posted inline.
fn findings_table(text: &mut String, target: &Target, findings: &[&Thread]) {
    open_section(
        text,
        &format!(
            "📝 {}: {}",
            count(findings.len(), "finding"),
            tally(findings)
        ),
    );
    text.push_str("| Finding | Where | Raised in | Outcome |\n|---|---|---|---|\n");
    let mut notes = String::new();
    let mut unposted = 0;
    for thread in findings {
        let first = thread.comments.first();
        let place = thread
            .anchor
            .as_ref()
            .map_or_else(|| "general".to_owned(), short_location);
        let url = first
            .and_then(|first| first.remote.as_ref())
            .and_then(|remote| remote.url.as_deref());
        let posted = first.is_some_and(|first| first.remote.is_some());
        let place = match url {
            Some(url) => format!("[{}]({url})", cell(&place)),
            None if posted => cell(&place),
            // A held question waits for its answer; a finding withdrawn before it was posted
            // has nothing to tell the host.
            None if thread.held
                || thread
                    .resolution
                    .as_ref()
                    .is_some_and(Resolution::withdrawn) =>
            {
                format!("{}, not posted", cell(&place))
            }
            None => {
                unposted += 1;
                let mark = superscript(unposted);
                let full = thread
                    .anchor
                    .as_ref()
                    .map_or_else(|| "general".to_owned(), location);
                let mut said = clip(first.map_or("", |first| first.body.trim()), MAX_NOTE);
                close_fence(&mut said);
                let _ = write!(
                    notes,
                    "\n{mark} **{}** at `{full}`, not posted inline:\n\n{said}\n",
                    kind_heading(thread)
                );
                format!("{}, not posted {mark}", cell(&place))
            }
        };
        let raised = thread
            .finding
            .as_ref()
            .map(|finding| match finding_pass(target, finding) {
                Some(pass) => format!("{} {}", finding.run, pass_label(pass).1),
                None => finding.run.clone(),
            })
            .unwrap_or_default();
        let gist = gist(first.map_or("", |first| first.body.as_str()));
        let _ = writeln!(
            text,
            "| {} | {place} | {raised} | {} |",
            cell(&format!("{}: {gist}", kind_heading(thread))),
            cell(&outcome(thread))
        );
    }
    text.push_str(&notes);
    close_section(text);
}

/// The collapsed, numbered steps of every run, and the time each run took.
fn steps(text: &mut String, covered: &[Covered], commits: &[CommitInfo], findings: &[&Thread]) {
    let mut steps = Vec::new();
    let mut spent = Vec::new();
    let mut total: Option<i64> = None;
    for covered in covered {
        let run = covered.run;
        for pass in &run.passes {
            let raised: Vec<&Thread> = findings
                .iter()
                .copied()
                .filter(|thread| raised_in(thread, &run.id, Some(&pass.id)))
                .collect();
            steps.push(pass_step(run, pass, raised.len()));
            outcome_steps(&mut steps, commits, &raised);
        }
        let spend = run_spend(run);
        if let Some(secs) = spend["total_secs"].as_i64() {
            *total.get_or_insert(0) += secs;
            let reviewing = spend["reviewing_secs"]
                .as_i64()
                .map(|secs| format!(" ({} reviewing)", duration(secs)))
                .unwrap_or_default();
            spent.push(format!("Run {}: {}{reviewing}.", run.id, duration(secs)));
        }
    }
    let total = total
        .map(|secs| format!(", {}", duration(secs)))
        .unwrap_or_default();
    open_section(
        text,
        &format!(
            "🧭 How it was reviewed: {}{total}",
            count(steps.len(), "step")
        ),
    );
    for (index, step) in steps.iter().enumerate() {
        let _ = writeln!(text, "{}. {step}", index + 1);
    }
    if !spent.is_empty() {
        let _ = writeln!(text, "\n{}", spent.join(" "));
    }
    close_section(text);
}

/// One pass as a sentence: who read what, and what it found.
fn pass_step(run: &Run, pass: &Pass, found: usize) -> String {
    let names: Vec<String> = pass
        .reviewers
        .iter()
        .filter(|reviewer| reviewer.id != "fix")
        .map(reviewer_name)
        .collect();
    let who = if names.is_empty() {
        "A reviewer".to_owned()
    } else {
        capitalize(&and_list(&names))
    };
    let found = match found {
        0 => "found nothing".to_owned(),
        1 => "found 1 issue".to_owned(),
        found => format!("found {found} issues"),
    };
    let range = match &pass.base {
        Some(base) => format!("{}..{}", short(base), short(&pass.head)),
        None => short(&pass.head).to_owned(),
    };
    let head = short(&pass.head);
    let action = match pass.kind {
        PassKind::Full if pass.locked => {
            format!("read the whole branch at {head} without earlier findings and {found}")
        }
        PassKind::Full if pass.partial => format!("read only {range} and {found}"),
        PassKind::Full => format!("read the whole branch at {head} and {found}"),
        PassKind::FixCheck => format!("checked the fixes in {range} and {found}"),
        PassKind::Reveal => match run.reveal {
            Some(reveal) => format!(
                "compared its findings with the earlier ones: {} agree, {} disagree",
                reveal.agree, reveal.disagree
            ),
            None => "compared its findings with the earlier ones".to_owned(),
        },
    };
    let secs = |time: &str| clock::parse(time).ok();
    let time = match (
        secs(&pass.started_at),
        pass.ended_at.as_deref().and_then(secs),
    ) {
        (Some(start), Some(end)) => format!(" ({})", duration((end - start).max(0))),
        _ => String::new(),
    };
    format!("{} {who} {action}{time}.", pass_label(pass).0)
}

/// `reviewer agent a0b5ad82`, `main session dcec84d5`, or the model when the id is unknown.
fn reviewer_name(reviewer: &Reviewer) -> String {
    let agent = session_role(Some(&reviewer.id)) == "agent";
    match (&reviewer.session, agent) {
        (Some(session), true) => format!("reviewer agent {}", short_id(session)),
        (Some(session), false) => format!("main session {}", short_id(session)),
        (None, true) => format!("reviewer {} ({})", reviewer.id, reviewer.model),
        (None, false) => "the main session".to_owned(),
    }
}

/// What became of the findings a pass raised: fixes by who made them, with their commits in
/// PR/MR order, then the rest by outcome.
fn outcome_steps(steps: &mut Vec<String>, commits: &[CommitInfo], raised: &[&Thread]) {
    let mut fixes: Vec<(Option<String>, Vec<&str>)> = Vec::new();
    for thread in raised {
        if let (Some(Resolution::Fixed { sha }), true) = (&thread.resolution, thread.resolved) {
            let who = actor(thread);
            match fixes.iter_mut().find(|(known, _)| *known == who) {
                Some((_, shas)) => shas.push(short(sha)),
                None => fixes.push((who, vec![short(sha)])),
            }
        }
    }
    for (who, shas) in fixes {
        let mut fixes_in: Vec<&str> = Vec::new();
        for sha in &shas {
            if !fixes_in.contains(sha) {
                fixes_in.push(sha);
            }
        }
        fixes_in.sort_by_key(|sha| {
            commits
                .iter()
                .position(|commit| commit.sha.starts_with(*sha))
                .unwrap_or(usize::MAX)
        });
        let fixes_in = and_list(&fixes_in);
        steps.push(match who {
            Some(who) => format!("🛠️ {who} fixed {}, in {fixes_in}.", shas.len()),
            None => format!("🛠️ Fixed {}, in {fixes_in}.", shas.len()),
        });
    }
    let rejected: Vec<&&Thread> = raised
        .iter()
        .filter(|thread| {
            thread.resolved && matches!(thread.resolution, Some(Resolution::Rejected { .. }))
        })
        .collect();
    if let Some(first) = rejected.first() {
        steps.push(match actor(first) {
            Some(who) => format!("🚫 {who} rejected {}.", rejected.len()),
            None => format!("🚫 {} rejected.", rejected.len()),
        });
    }
    let tallied = |wanted: &str| {
        raised
            .iter()
            .filter(|thread| category(thread) == wanted)
            .count()
    };
    for (wanted, emoji, phrase) in [
        ("answered", "💬", "answered by the author"),
        ("stale", "➖", "went stale"),
        ("resolved", "✅", "resolved"),
        ("not fixed", "❌", "not fixed yet"),
        ("held", "⏸️", "held for the author"),
        ("open", "⚠️", "still open"),
    ] {
        match tallied(wanted) {
            0 => {}
            number => steps.push(format!("{emoji} {number} {phrase}.")),
        }
    }
}

/// The main session that settled `thread`: the author of its last reply from the main
/// session, as the steps name it.
fn actor(thread: &Thread) -> Option<String> {
    thread
        .comments
        .iter()
        .skip(1)
        .rev()
        .find_map(|comment| match &comment.author {
            Author::Agent {
                session, reviewer, ..
            } if session_role(reviewer.as_deref()) == "session" => Some(match session {
                Some(session) => format!("Main session {}", short_id(session)),
                None => "The main session".to_owned(),
            }),
            _ => None,
        })
}

/// The grey quoted table of the agents that took part, and the footer.
fn agents(text: &mut String, covered: &[Covered], findings: &[&Thread]) {
    let mut rows: Vec<[String; 4]> = Vec::new();
    let mut main: Vec<[String; 4]> = Vec::new();
    let mut add_main = |row: [String; 4]| {
        if !main.iter().any(|known| known[1..] == row[1..]) {
            main.push(row);
        }
    };
    let main_label = || "🛠️ Main session: fixes, replies".to_owned();
    let id = |role: &str, session: Option<&String>| {
        session.map_or_else(|| "—".to_owned(), |session| format!("{role} {session}"))
    };
    for covered in covered {
        let run = covered.run;
        let mut seen: Vec<&str> = Vec::new();
        for reviewer in run.passes.iter().flat_map(|pass| &pass.reviewers) {
            let model = model_label(&reviewer.model, &reviewer.effort);
            let harness = harness_label(&reviewer.harness, reviewer.harness_version.as_deref());
            if reviewer.id == "fix" {
                add_main([
                    main_label(),
                    model,
                    harness,
                    id("session", reviewer.session.as_ref()),
                ]);
                continue;
            }
            if seen.contains(&reviewer.id.as_str()) {
                continue;
            }
            seen.push(&reviewer.id);
            let blind = run.passes.iter().any(|pass| {
                pass.locked
                    && pass
                        .reviewers
                        .iter()
                        .any(|candidate| candidate.id == reviewer.id)
            });
            let who = match reviewer.id.as_str() {
                "manual" => format!("🧑 Main session, own review, {}", run.id),
                id if blind => format!("🙈 Blind reviewer {id}, {}", run.id),
                id => format!("🔍 Reviewer {id}, {}", run.id),
            };
            let role = session_role(Some(&reviewer.id));
            rows.push([who, model, harness, id(role, reviewer.session.as_ref())]);
        }
    }
    for comment in findings
        .iter()
        .flat_map(|thread| thread.comments.iter().skip(1))
    {
        if let Author::Agent {
            model,
            effort,
            harness,
            harness_version,
            reviewer,
            session,
            ..
        } = &comment.author
            && session_role(reviewer.as_deref()) == "session"
        {
            add_main([
                main_label(),
                model_label(model, effort),
                harness_label(harness, harness_version.as_deref()),
                id("session", session.as_ref()),
            ]);
        }
    }
    rows.append(&mut main);
    text.push_str("\n\n");
    if !rows.is_empty() {
        text.push_str("> | Agent | Model | Harness | ID |\n> |---|---|---|---|\n");
        for [who, model, harness, id] in &rows {
            let _ = writeln!(text, "> | {who} | {model} | {harness} | {id} |");
        }
        text.push_str(">\n");
    }
    let last = covered.last().map_or("", |covered| covered.run.id.as_str());
    let _ = write!(
        text,
        "> <sub>Posted by agt {}. Edited on each publish; last run {last}.</sub>",
        env!("CARGO_PKG_VERSION")
    );
}

/// How a commit was reviewed, with whether it was: the strongest pass that read it, or none,
/// naming the run it fixed findings for.
fn commit_review(covered: &[Covered], commit: &CommitInfo) -> (bool, String) {
    let same = |patch: &CommitPatch| {
        patch.sha == commit.sha || (patch.patch_id.is_some() && patch.patch_id == commit.patch_id)
    };
    let mut best: Option<(u8, String)> = None;
    for covered in covered {
        for pass in &covered.run.passes {
            if pass.kind == PassKind::Reveal || !pass.commits.iter().any(same) {
                continue;
            }
            let rank = match pass.kind {
                PassKind::Full if !pass.locked && !pass.partial => 3,
                PassKind::Full if pass.locked => 2,
                PassKind::Full => 1,
                PassKind::FixCheck | PassKind::Reveal => 0,
            };
            if best.as_ref().is_none_or(|(known, _)| rank >= *known) {
                let (emoji, label) = pass_label(pass);
                best = Some((rank, format!("{emoji} {label}, {}", covered.run.id)));
            }
        }
    }
    if let Some((_, label)) = best {
        return (true, label);
    }
    match covered
        .iter()
        .rev()
        .find(|covered| covered.run.repair.iter().any(same))
    {
        Some(covered) => (false, format!("⚠️ none; a fix from {}", covered.run.id)),
        None => (false, "⚠️ none".to_owned()),
    }
}

/// A finding's outcome as the counts name it.
fn category(thread: &Thread) -> &'static str {
    match (&thread.resolution, thread.resolved) {
        (Some(Resolution::Fixed { .. }), true) => "fixed",
        (Some(Resolution::Answered), true) => "answered",
        (Some(Resolution::Rejected { .. }), true) => "rejected",
        (Some(Resolution::Stale), true) => "stale",
        (_, true) => "resolved",
        (Some(Resolution::NotFixed { .. }), false) => "not fixed",
        (_, false) if thread.held => "held",
        (_, false) => "open",
    }
}

/// `1 fixed, 1 open`, in a fixed order.
fn tally(findings: &[&Thread]) -> String {
    [
        "fixed",
        "answered",
        "rejected",
        "stale",
        "resolved",
        "not fixed",
        "held",
        "open",
    ]
    .iter()
    .filter_map(|wanted| {
        let number = findings
            .iter()
            .filter(|thread| category(thread) == *wanted)
            .count();
        (number > 0).then(|| format!("{number} {wanted}"))
    })
    .collect::<Vec<_>>()
    .join(", ")
}

/// `release.yml:156-164`: the file name and lines, short enough for a table.
fn short_location(anchor: &Anchor) -> String {
    let file = anchor.path.rsplit('/').next().unwrap_or(&anchor.path);
    match (anchor.side, anchor.start_line, anchor.line, anchor.old_line) {
        (Side::New, Some(start), Some(line), _) if start < line => {
            format!("{file}:{start}-{line}")
        }
        (Side::New, _, Some(line), _) => format!("{file}:{line}"),
        (Side::Old, _, _, Some(line)) => format!("{file} (old):{line}"),
        _ => file.to_owned(),
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

/// `a`, `a and b`, `a, b and c`.
fn and_list<T: AsRef<str>>(items: &[T]) -> String {
    match items {
        [] => String::new(),
        [one] => one.as_ref().to_owned(),
        [rest @ .., last] => format!(
            "{} and {}",
            rest.iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(", "),
            last.as_ref()
        ),
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// `¹`, `²`, ... `¹⁰`: footnote marks.
fn superscript(number: usize) -> String {
    number
        .to_string()
        .chars()
        .map(|digit| match digit {
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4' => '⁴',
            '5' => '⁵',
            '6' => '⁶',
            '7' => '⁷',
            '8' => '⁸',
            _ => '⁹',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_and_cells_read_as_text() {
        assert_eq!(and_list(&["a"]), "a");
        assert_eq!(and_list(&["a", "b"]), "a and b");
        assert_eq!(and_list(&["a", "b", "c"]), "a, b and c");
        assert_eq!(cell("a | b\nc"), "a \\| b c");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(gist("\nFirst one. Second one.\nMore."), "First one.");
        assert_eq!(
            gist("`docs/a.md:3`\n\n**❓ question:** Why so? More.\n\n> <sub>m</sub>"),
            "Why so?"
        );
        assert_eq!(gist("**question:** Old heading."), "Old heading.");
        assert_eq!(superscript(12), "¹²");
        assert_eq!(count(1, "commit"), "1 commit");
        assert_eq!(count(3, "commit"), "3 commits");
        assert_eq!(capitalize("reviewer agent x"), "Reviewer agent x");
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
