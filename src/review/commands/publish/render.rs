use std::fmt::Write as _;

use crate::forge::markers;
use crate::review::commands::run::SPECIAL_REVIEWERS;
use crate::store::model::{
    Anchor, Author, Comment, FindingInfo, Pass, PassKind, Resolution, Run, Side, Target, Thread,
    ThreadType,
};

pub(super) fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// The first 8 characters of a harness id, as signatures name it.
pub(super) fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

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
        ThreadType::Requirement => "📋",
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

/// What a pass did, with its emoji: `🔍 full review`, `🔹 partial review`, `🙈 blind review`,
/// `🔁 fix check` or `👀 reveal`.
pub(super) fn pass_label(pass: &Pass) -> (&'static str, &'static str) {
    match pass.kind {
        PassKind::Full if pass.locked => ("🙈", "blind review"),
        PassKind::Full if pass.partial => ("🔹", "partial review"),
        PassKind::Full => ("🔍", "full review"),
        PassKind::FixCheck => ("🔁", "fix check"),
        PassKind::Reveal => ("👀", "reveal"),
    }
}

/// The outcome of a finding with its emoji, as the summary shows it.
pub(super) fn outcome(thread: &Thread) -> String {
    match (&thread.resolution, thread.resolved) {
        (Some(Resolution::Fixed { sha }), true) => format!("✅ Fixed in {}", short(sha)),
        (Some(Resolution::Answered), true) => "💬 Answered".to_owned(),
        (Some(Resolution::Rejected { reason }), true) => format!("🚫 Rejected: {reason}"),
        (Some(Resolution::Stale), true) => "➖ Stale".to_owned(),
        (_, true) => "✅ Resolved".to_owned(),
        (Some(Resolution::NotFixed { reason }), false) => format!("❌ Not fixed: {reason}"),
        (_, false) if thread.held => "⏸️ Held for the author".to_owned(),
        (_, false) => "⚠️ Open".to_owned(),
    }
}

/// The posted text: a finding heading, the text, the signature and the markers. An agent signs
/// in a grey quote under its text; a one-line agent reply is all quote:
/// `> ✅ Fixed in ... <sub>signature</sub>`.
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
        let _ = write!(text, "**{}:** ", kind_heading(thread));
    }
    let said = comment.body.trim_end();
    let mut sign = signature(&comment.author);
    if let (Some(sign), Some(finding)) = (&mut sign, finding) {
        if finding_pass(target, finding).is_some_and(|pass| pass.locked) {
            sign.push_str(" · 🙈 blind review");
        }
        if finding.post_reveal {
            sign.push_str(" · post-reveal");
        }
    }
    match sign {
        Some(sign) if !first && !said.contains('\n') => {
            let _ = write!(text, "> {said} <sub>{sign}</sub>");
        }
        Some(sign) => {
            let _ = write!(text, "{said}\n\n> <sub>{sign}</sub>");
        }
        None => text.push_str(said),
    }
    if let Some(finding) = finding {
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

/// `model effort · Harness version · agent a0b5ad82`: a reviewer names its agent id, the main
/// session its session id. People sign nothing; the host shows who posted.
pub(super) fn signature(author: &Author) -> Option<String> {
    let Author::Agent {
        model,
        effort,
        harness,
        harness_version,
        reviewer,
        session,
        ..
    } = author
    else {
        return None;
    };
    let mut text = tool(model, effort, harness, harness_version.as_deref());
    if let Some(session) = session {
        let _ = write!(
            text,
            " · {} {}",
            session_role(reviewer.as_deref()),
            short_id(session)
        );
    }
    Some(text)
}

/// `agent` for a reviewer, `session` for the main session (`manual`, `fix` or no reviewer).
pub(super) fn session_role(reviewer: Option<&str>) -> &'static str {
    match reviewer {
        Some(id) if !SPECIAL_REVIEWERS.contains(&id) => "agent",
        _ => "session",
    }
}

pub(super) fn harness_name(harness: &str) -> &str {
    match harness {
        "claude-code" => "Claude Code",
        "codex" | "codex-cli" => "codex-cli",
        other => other,
    }
}

/// `claude-opus-5-5 xhigh`; an unknown or default effort is left out.
pub(super) fn model_label(model: &str, effort: &str) -> String {
    match effort.trim() {
        "" | "unknown" | "default" => model.to_owned(),
        effort => format!("{model} {effort}"),
    }
}

/// `Claude Code 2.1.302`.
pub(super) fn harness_label(harness: &str, version: Option<&str>) -> String {
    match version {
        Some(version) => format!("{} {version}", harness_name(harness)),
        None => harness_name(harness).to_owned(),
    }
}

pub(super) fn tool(model: &str, effort: &str, harness: &str, version: Option<&str>) -> String {
    format!(
        "{} · {}",
        model_label(model, effort),
        harness_label(harness, version)
    )
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

/// The body of the review that carries the in-diff findings `threads` of `commit`:
/// `**🔍 Agent review rv-rtom** of f48de58: 1 of 2 findings is inline here. ...`. `summary`
/// says whether a summary comment lists the rest.
pub(super) fn review_body(
    target: &Target,
    commit: &str,
    threads: &[&Thread],
    summary: bool,
) -> String {
    let mut passes: Vec<(&str, &str)> = threads
        .iter()
        .filter_map(|thread| thread.finding.as_ref())
        .map(|finding| (finding.run.as_str(), finding.pass.as_str()))
        .collect();
    passes.sort_unstable();
    passes.dedup();
    let mut text = String::new();
    match passes.as_slice() {
        [(run, pass)] => {
            let pass = pass_of(target, run, pass);
            let blind = pass.is_some_and(|pass| pass.locked);
            let emoji = if blind { "🙈" } else { "🔍" };
            let _ = write!(text, "**{emoji} Agent review {run}**");
            if let Some(cycle) = pass.map(|pass| pass.cycle).filter(|cycle| *cycle > 1) {
                let _ = write!(text, ", round {cycle}");
            }
            if blind {
                text.push_str(", a blind review");
            }
        }
        _ => text.push_str("**🔍 Agent review**"),
    }
    let here = threads.len();
    // Every finding of these passes that may reach the host, here or elsewhere.
    let total = target
        .threads
        .iter()
        .filter(|thread| {
            thread.finding.as_ref().is_some_and(|finding| {
                passes.contains(&(finding.run.as_str(), finding.pass.as_str()))
            })
        })
        .filter(|thread| {
            thread
                .comments
                .first()
                .is_some_and(|first| first.remote.is_some())
                || !thread
                    .resolution
                    .as_ref()
                    .is_some_and(Resolution::withdrawn)
        })
        .count()
        .max(here);
    let _ = write!(text, " of {}: ", short(commit));
    match (here, total) {
        (1, 1) => text.push_str("1 finding."),
        (here, total) if here == total => {
            let _ = write!(text, "{here} findings.");
        }
        (here, total) => {
            let verb = if here == 1 { "is" } else { "are" };
            let _ = write!(text, "{here} of {total} findings {verb} inline here.");
            if summary {
                let all = if total == 2 {
                    "both".to_owned()
                } else {
                    format!("all {total}")
                };
                let _ = write!(text, " The summary comment lists {all}.");
            }
        }
    }
    let _ = write!(
        text,
        "\n{}",
        markers::agent(&markers::review_id(commit), None)
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
    fn signatures_name_the_tool_and_session() {
        let agent = |effort: &str, reviewer: Option<&str>, session: Option<&str>| Author::Agent {
            model: "claude-opus-5-5".into(),
            effort: effort.into(),
            harness: "claude-code".into(),
            harness_version: Some("2.1.302".into()),
            run: None,
            pass: None,
            reviewer: reviewer.map(str::to_owned),
            session: session.map(str::to_owned),
        };
        assert_eq!(
            signature(&agent("xhigh", Some("r1"), Some("a0b5ad828155531d2"))).as_deref(),
            Some("claude-opus-5-5 xhigh · Claude Code 2.1.302 · agent a0b5ad82")
        );
        assert_eq!(
            signature(&agent("default", Some("fix"), Some("dcec84d5-4c7a"))).as_deref(),
            Some("claude-opus-5-5 · Claude Code 2.1.302 · session dcec84d5")
        );
        assert_eq!(
            signature(&agent("unknown", None, None)).as_deref(),
            Some("claude-opus-5-5 · Claude Code 2.1.302")
        );
        let human = Author::Human {
            login: "me".into(),
            git_name: false,
        };
        assert_eq!(signature(&human), None);
    }

    #[test]
    fn short_names() {
        assert_eq!(short("0123456789"), "0123456");
        assert_eq!(short_id("0199f2c4-7e1a"), "0199f2c4");
        assert_eq!(harness_name("claude-code"), "Claude Code");
    }
}
