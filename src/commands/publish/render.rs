use std::fmt::Write as _;

use serde_json::Value;

use crate::commands::{author_label, is_open};
use crate::markers;
use crate::model::{Anchor, Author, Comment, PassKind, Resolution, Run, Side, Target, Thread};

pub(super) fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// `2m30s`, `45s`, `14m`: exact, so the parts add up to the total.
pub(super) fn duration(secs: i64) -> String {
    match (secs / 60, secs % 60) {
        (0, secs) => format!("{secs}s"),
        (minutes, 0) => format!("{minutes}m"),
        (minutes, secs) => format!("{minutes}m{secs}s"),
    }
}

/// `Time: 14m (reviewing 9m, fixing 5m). Tokens: reviewers 120k, main session 80k.`
pub(super) fn spend_line(spent: &Value) -> Option<String> {
    let minutes = |key: &str| spent[key].as_i64().map(duration);
    let thousands = |key: &str| {
        spent[key]
            .as_u64()
            .map(|tokens| format!("{}k", (tokens + 500) / 1000))
    };
    let mut line = String::new();
    if let Some(total) = minutes("total_secs") {
        let _ = write!(line, "Time: {total}");
        if let (Some(review), Some(fix)) = (minutes("reviewing_secs"), minutes("fixing_secs")) {
            let _ = write!(line, " (reviewing {review}, fixing {fix})");
        }
        line.push('.');
    }
    let tokens: Vec<String> = [
        ("reviewers", "reviewer_tokens"),
        ("main session", "main_tokens"),
    ]
    .iter()
    .filter_map(|(label, key)| thousands(key).map(|value| format!("{label} {value}")))
    .collect();
    if !tokens.is_empty() {
        if !line.is_empty() {
            line.push(' ');
        }
        let _ = write!(line, "Tokens: {}.", tokens.join(", "));
    }
    (!line.is_empty()).then_some(line)
}

/// `path:line`, `path (old):line`, or the path of a whole-file finding.
pub(super) fn location(anchor: &Anchor) -> String {
    match (anchor.side, anchor.line, anchor.old_line) {
        (Side::New, Some(line), _) => format!("{}:{line}", anchor.path),
        (Side::Old, _, Some(line)) => format!("{} (old):{line}", anchor.path),
        _ => anchor.path.clone(),
    }
}

/// The posted text: a finding heading, the text, the signature and the markers.
pub(super) fn body(
    target: &Target,
    thread: &Thread,
    comment: &Comment,
    location: Option<&str>,
    attempt: &str,
) -> String {
    let first = thread
        .comments
        .first()
        .is_some_and(|first| first.id == comment.id);
    let finding = thread.finding.as_ref().filter(|_| first);
    let mut text = String::new();
    if let Some(location) = location {
        let _ = write!(text, "`{location}`\n\n");
    }
    if finding.is_some() {
        let kind = thread.kind.as_str();
        match thread.severity {
            Some(severity) => {
                let _ = write!(text, "**{kind} ({}):** ", severity.as_str());
            }
            None => {
                let _ = write!(text, "**{kind}:** ");
            }
        }
    }
    text.push_str(comment.body.trim_end());
    text.push_str("\n\n");
    text.push_str(&signature(&comment.author));
    if let Some(finding) = finding {
        if finding.post_reveal {
            text.push_str(" post-reveal");
        }
        let sha = thread
            .anchor
            .as_ref()
            .and_then(|anchor| anchor.commit.clone())
            .or_else(|| {
                target
                    .runs
                    .iter()
                    .filter(|run| run.id == finding.run)
                    .flat_map(|run| &run.passes)
                    .find(|pass| pass.id == finding.pass)
                    .map(|pass| pass.head.clone())
            })
            .unwrap_or_default();
        let _ = write!(text, "\n{}", markers::finding(&thread.id, &sha));
    }
    let _ = write!(text, "\n{}", markers::agent(&comment.id, Some(attempt)));
    text
}

pub(super) fn signature(author: &Author) -> String {
    match author {
        Author::Agent { effort, .. } if effort.trim().is_empty() || effort == "unknown" => {
            let mut author = author.clone();
            if let Author::Agent { effort, .. } = &mut author {
                "default".clone_into(effort);
            }
            author_label(&author)
        }
        _ => author_label(author),
    }
}

pub(super) fn harness_name(harness: &str) -> &str {
    match harness {
        "claude-code" => "Claude Code",
        "codex" | "codex-cli" => "codex-cli",
        other => other,
    }
}

pub(super) fn tool(model: &str, effort: &str, harness: &str, version: Option<&str>) -> String {
    let mut text = format!("{model} {effort} · {}", harness_name(harness));
    if let Some(version) = version {
        let _ = write!(text, " {version}");
    }
    text
}

/// The head a run's final record is about: the repaired head, else the last pass's.
pub(super) fn record_head(run: &Run) -> String {
    let last = run
        .passes
        .last()
        .map(|pass| pass.head.clone())
        .unwrap_or_default();
    run.repaired_head.clone().unwrap_or(last)
}

/// The run's final record per the review skill; `at_head` is how many passes reviewed `head`.
pub(super) fn record_body(
    target: &Target,
    run: &Run,
    head: &str,
    at_head: usize,
    attempt: &str,
) -> String {
    let findings: Vec<&Thread> = target
        .threads
        .iter()
        .filter(|thread| {
            thread
                .finding
                .as_ref()
                .is_some_and(|finding| finding.run == run.id)
        })
        .collect();
    let fixed = findings
        .iter()
        .filter(|thread| {
            thread.resolved && matches!(thread.resolution, Some(Resolution::Fixed { .. }))
        })
        .count();
    let rejected = findings
        .iter()
        .filter(|thread| {
            thread.resolved && matches!(thread.resolution, Some(Resolution::Rejected { .. }))
        })
        .count();
    let rejected = match rejected {
        0 => String::new(),
        count => format!(", {count} rejected"),
    };
    let open = findings.iter().filter(|thread| is_open(thread)).count();
    let mut cycles: Vec<u32> = run.passes.iter().map(|pass| pass.cycle).collect();
    cycles.dedup();
    // Fixes after the last pass make the head "review-fixed"; a repair a pass still covers,
    // such as committing the reviewed dirty tree, leaves it "reviewed".
    let verdict = if run.repair.is_empty() || at_head > 0 {
        "reviewed"
    } else {
        "review-fixed"
    };
    let mut text = format!(
        "**Review {}:** {verdict} at {}. {} cycles, {} found, {fixed} fixed{rejected}, {open} open.\n\n",
        run.id,
        short(head),
        cycles.len(),
        findings.len(),
    );
    text.push_str("| Pass | Reviewed | Starting context | Reviewer |\n|---|---|---|---|\n");
    let mut seen: Vec<&str> = Vec::new();
    for pass in &run.passes {
        let kind = match pass.kind {
            PassKind::Full => "full",
            PassKind::FixCheck => "fix check",
            PassKind::Reveal => "reveal",
        };
        let reviewed = match (&pass.base, pass.kind) {
            (Some(base), PassKind::FixCheck) => format!("{}..{}", short(base), short(&pass.head)),
            _ => short(&pass.head).to_owned(),
        };
        // The fixer reviews nothing; it shows on the `Fixed by:` line.
        for reviewer in pass
            .reviewers
            .iter()
            .filter(|reviewer| reviewer.id != "fix")
        {
            let label = if seen.contains(&reviewer.id.as_str()) {
                reviewer.id.clone()
            } else {
                seen.push(&reviewer.id);
                format!(
                    "{} {}",
                    reviewer.id,
                    tool(
                        &reviewer.model,
                        &reviewer.effort,
                        &reviewer.harness,
                        reviewer.harness_version.as_deref()
                    )
                )
            };
            let _ = writeln!(
                text,
                "| {} {kind} | {reviewed} | {} | {label} |",
                pass.id, reviewer.starting_context
            );
        }
    }
    text.push('\n');
    if let Some(reveal) = run.reveal {
        let _ = writeln!(
            text,
            "Reveal: {} agree, {} disagree.",
            reveal.agree, reveal.disagree
        );
    }
    let mut fixers: Vec<String> = Vec::new();
    for comment in findings.iter().flat_map(|thread| &thread.comments) {
        if let Author::Agent {
            model,
            effort,
            harness,
            harness_version,
            reviewer: Some(reviewer),
            ..
        } = &comment.author
            && reviewer == "fix"
        {
            let fixer = tool(model, effort, harness, harness_version.as_deref());
            if !fixers.contains(&fixer) {
                fixers.push(fixer);
            }
        }
    }
    if !fixers.is_empty() {
        let _ = writeln!(text, "Fixed by: {}", fixers.join("; "));
    }
    if let Some(line) = spend_line(&crate::commands::run_spend(run)) {
        let _ = writeln!(text, "{line}");
    }
    let _ = writeln!(text, "{}", markers::record(&run.id, head, at_head, open));
    let _ = write!(
        text,
        "{}",
        markers::agent(&markers::record_id(&run.id), Some(attempt))
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_name_the_side() {
        let anchor = Anchor {
            path: "a.txt".into(),
            line: None,
            old_line: Some(4),
            side: Side::Old,
            start_line: None,
            commit: None,
            tree: String::new(),
        };
        assert_eq!(location(&anchor), "a.txt (old):4");
    }

    #[test]
    fn durations_add_up() {
        assert_eq!(duration(45), "45s");
        assert_eq!(duration(840), "14m");
        assert_eq!(duration(150), "2m30s");
    }

    #[test]
    fn spend_lines_name_time_and_tokens() {
        let spent = serde_json::json!({
            "total_secs": 840, "reviewing_secs": 540, "fixing_secs": 300,
            "reviewer_tokens": 120_000, "main_tokens": 80_000,
        });
        assert_eq!(
            spend_line(&spent).as_deref(),
            Some("Time: 14m (reviewing 9m, fixing 5m). Tokens: reviewers 120k, main session 80k.")
        );
        assert_eq!(spend_line(&serde_json::json!({})), None);
    }

    #[test]
    fn short_names() {
        assert_eq!(short("0123456789"), "0123456");
        assert_eq!(harness_name("claude-code"), "Claude Code");
    }
}
