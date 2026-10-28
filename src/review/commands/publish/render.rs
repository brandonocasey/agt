use std::fmt::Write as _;

use crate::forge::markers;
use crate::store::model::{
    Anchor, Author, Comment, FindingInfo, Pass, Resolution, Run, Side, Target, Thread, ThreadType,
};

pub(super) use crate::core::text::short;

/// `2m30s`, `45s`, `14m`: exact, so the parts add up to the total.
pub(super) fn duration(secs: i64) -> String {
    match (secs / 60, secs % 60) {
        (0, secs) => format!("{secs}s"),
        (minutes, 0) => format!("{minutes}m"),
        (minutes, secs) => format!("{minutes}m{secs}s"),
    }
}

/// `path:line`, `path (old):line`, or the path of a whole-file finding.
pub(super) fn location(anchor: &Anchor) -> String {
    match (anchor.side, anchor.line, anchor.old_line) {
        (Side::New, Some(line), _) => format!("{}:{line}", anchor.path),
        (Side::Old, _, Some(line)) => format!("{} (old):{line}", anchor.path),
        _ => anchor.path.clone(),
    }
}

/// The emoji that leads a finding of this kind.
pub(super) fn kind_emoji(kind: ThreadType) -> &'static str {
    match kind {
        ThreadType::Bug => "🐛",
        ThreadType::Question => "❓",
        ThreadType::Requirement => "🎯",
        ThreadType::Nit => "✏️",
        ThreadType::Comment => "💬",
    }
}

/// `🐛 bug (high)`: a finding's kind as its heading and the summary name it.
pub(super) fn kind_heading(thread: &Thread) -> String {
    let (emoji, kind) = (kind_emoji(thread.kind), thread.kind.as_str());
    match thread.severity {
        Some(severity) => format!("{emoji} {kind} ({})", severity.as_str()),
        None => format!("{emoji} {kind}"),
    }
}

/// The pass that raised a finding.
pub(super) fn finding_pass<'a>(target: &'a Target, finding: &FindingInfo) -> Option<&'a Pass> {
    pass_of(target, &finding.run, &finding.pass)
}

fn pass_of<'a>(target: &'a Target, run: &str, pass: &str) -> Option<&'a Pass> {
    target
        .runs
        .iter()
        .filter(|candidate| candidate.id == run)
        .flat_map(|run| &run.passes)
        .find(|candidate| candidate.id == pass)
}

/// The outcome of a finding with its emoji, as the summary shows it.
pub(super) fn outcome(thread: &Thread) -> String {
    match (&thread.resolution, thread.resolved) {
        (Some(Resolution::Fixed { sha }), true) => format!("✅ Fixed in {}", short(sha)),
        (Some(Resolution::Answered), true) => "💬 Answered".to_owned(),
        (Some(Resolution::Rejected { reason }), true) => format!("🚫 Won't fix: {reason}"),
        (Some(Resolution::Stale), true) => "➖ No longer applies".to_owned(),
        (Some(Resolution::Moved { issue }), true) => format!("📋 Moved to {issue}"),
        (_, true) => "✅ Resolved".to_owned(),
        (Some(Resolution::NotFixed { reason }), false) => format!("❌ Not fixed: {reason}"),
        (_, false) if thread.held => "⏸️ Waiting on the PR author".to_owned(),
        (_, false) => "⚠️ Open".to_owned(),
    }
}

/// The posted text: the location when it is outside the diff, the kind as a title, the text
/// and the footer, then the markers.
pub(super) fn body(
    target: &Target,
    thread: &Thread,
    comment: &Comment,
    location: Option<&str>,
    attempt: &str,
) -> String {
    let mut text = paragraph(target, thread, comment, location);
    let first = is_first(thread, comment);
    if let Some(finding) = thread.finding.as_ref().filter(|_| first) {
        let sha = thread
            .anchor
            .as_ref()
            .and_then(|anchor| anchor.commit.clone())
            .or_else(|| finding_pass(target, finding).map(|pass| pass.head.clone()))
            .unwrap_or_default();
        let _ = write!(text, "\n{}", markers::finding(&thread.id, &sha));
    }
    let _ = write!(text, "\n{}", markers::agent(&comment.id, Some(attempt)));
    text
}

fn is_first(thread: &Thread, comment: &Comment) -> bool {
    thread
        .comments
        .first()
        .is_some_and(|first| first.id == comment.id)
}

/// The visible part of a post, which is also what a review body holds for each finding.
pub(super) fn paragraph(
    target: &Target,
    thread: &Thread,
    comment: &Comment,
    location: Option<&str>,
) -> String {
    let first = is_first(thread, comment);
    let mut text = String::new();
    if let Some(location) = location {
        let _ = write!(text, "`{location}`\n\n");
    }
    if (thread.finding.is_some() || thread.check.is_some()) && first {
        let _ = write!(text, "**{}**\n\n", kind_heading(thread));
    }
    text.push_str(comment.body.trim_end());
    if let Some(issue) = thread.issue.as_ref().filter(|_| first) {
        let _ = write!(text, "\n\nRelated: {issue}");
    }
    if let Author::Agent { model, .. } = &comment.author {
        let _ = write!(
            text,
            "\n\n> <sub>{}</sub>",
            footer(target, thread, comment, model)
        );
    }
    text
}

/// `🔍 agt review · model`, or `🧪 agt QA · check · model` for a QA thread or comment.
fn footer(target: &Target, thread: &Thread, comment: &Comment, model: &str) -> String {
    if !comment.qa && thread.check.is_none() {
        return format!("🔍 agt review · {model}");
    }
    let name = thread.check.as_deref().map(|id| {
        target
            .qa_runs
            .iter()
            .flat_map(|run| &run.checks)
            .find(|check| check.id == id)
            .map_or(id, |check| check.name.as_str())
    });
    match name {
        Some(name) => format!("🧪 agt QA · {name} · {model}"),
        None => format!("🧪 agt QA · {model}"),
    }
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
    fn short_names() {
        assert_eq!(short("0123456789"), "0123456");
    }
}
