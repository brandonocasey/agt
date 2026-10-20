//! Reading what agt wrote into a forge comment: review records, answers and finding kinds.

use crate::forge::CommentData;
use crate::forge::markers::{self, agent_id};
use crate::store::model::{FindingInfo, ImportedRecord, Severity, ThreadType};

pub(super) fn parse_records(comment: &CommentData) -> Vec<ImportedRecord> {
    markers::parse_records(&comment.body)
        .into_iter()
        .map(|marker| ImportedRecord {
            run: marker.run,
            head: marker.head,
            reviews_at_head: marker.reviews_at_head,
            open: marker.open,
            comment_id: comment.id.clone(),
            url: comment.url.clone(),
            created_at: comment.created_at.clone(),
        })
        .collect()
}

/// The thread id a general comment answers: a `Fixed in ... Re: <id>` comment, or any
/// `... Re: <id>` reply that `publish` marked.
pub(super) fn answered_id(body: &str, marked: bool) -> Option<String> {
    let body = body.trim_end();
    let (before, last) = body.rsplit_once('\n').unwrap_or(("", body));
    let id = last.trim().strip_prefix("Re: ")?.trim();
    ((marked || before.contains("Fixed in")) && !id.is_empty()).then(|| id.to_owned())
}

/// The first line of an outcome reply without the quote mark and emoji that lead it:
/// `> ✅ Fixed in ...` reads `Fixed in ...`.
fn outcome_line(body: &str) -> &str {
    let line = body.lines().next().unwrap_or_default().trim_start();
    let line = line.strip_prefix('>').unwrap_or(line);
    line.trim_start_matches(|char: char| !char.is_ascii_alphanumeric())
}

/// The reason of a marked `> 🚫 Rejected: <reason>. <sub>signature</sub>` reply, or of the
/// plain `Rejected: <reason>.` earlier versions posted.
pub(super) fn rejected_reason(body: &str) -> Option<String> {
    agent_id(body)?;
    let rest = outcome_line(body).strip_prefix("Rejected: ")?;
    let reason = rest
        .split_once(" <sub>")
        .map_or(rest, |(reason, _)| reason)
        .trim()
        .trim_end_matches('.');
    (!reason.is_empty()).then(|| reason.to_owned())
}

/// The commit of a `Fixed in <sha>` reply, also when the sha opens a link:
/// `Fixed in [<sha> <subject>](<url>)`.
pub(super) fn fixed_sha(body: &str) -> Option<String> {
    let rest = body.split_once("Fixed in ")?.1;
    let sha = rest
        .split_whitespace()
        .next()?
        .trim_start_matches('[')
        .split(']')
        .next()?
        .trim_end_matches(['.', ',']);
    (!sha.is_empty()).then(|| sha.to_owned())
}

/// Type and severity from a `**🐛 bug (medium):**` heading, after the location line that opens
/// a finding on a file outside the diff.
pub(super) fn finding_kind(body: &str) -> (ThreadType, Option<Severity>) {
    let body = body.trim_start();
    let body = match body.split_once('\n') {
        Some((first, rest)) if first.starts_with('`') => rest.trim_start(),
        _ => body,
    };
    let heading = body
        .strip_prefix("**")
        .and_then(|rest| rest.split_once(":**"))
        .map(|(heading, _)| heading.trim_start_matches(|char: char| !char.is_ascii_alphanumeric()));
    let Some(heading) = heading else {
        return (ThreadType::Comment, None);
    };
    let (kind, severity) = match heading.split_once(" (") {
        Some((kind, severity)) => (kind, severity.strip_suffix(')')),
        None => (heading, None),
    };
    (ThreadType::parse(kind), severity.and_then(Severity::parse))
}

pub(super) fn finding_info(id: &str) -> Option<FindingInfo> {
    let mut parts = id.split('.');
    let (run, pass, reviewer, number) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !number.starts_with('f') {
        return None;
    }
    Some(FindingInfo {
        run: run.to_owned(),
        pass: pass.to_owned(),
        reviewer: reviewer.to_owned(),
        evidence: None,
        tests_run: Vec::new(),
        post_reveal: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_markers_and_records() {
        let body = "**bug (medium):** x\n<!-- review-finding v1 id=rv-17kj.c1p0.r1.f1 sha=abc -->";
        assert_eq!(
            markers::finding_id(body).as_deref(),
            Some("rv-17kj.c1p0.r1.f1")
        );
        assert!(matches!(
            finding_kind(body),
            (ThreadType::Bug, Some(Severity::Medium))
        ));
        let comment = CommentData {
            id: "1".into(),
            url: None,
            body: "**Review rv-7k2m:** ...\n<!-- review-record v2 id=rv-7k2m head=abc reviews-at-head=2 open=1 -->".into(),
            created_at: "2026-10-16T12:00:00Z".into(),
            login: Some("me".into()),
            editor: None,
            review_id: None,
        };
        let records = parse_records(&comment);
        let [record] = records.as_slice() else {
            panic!("one record expected");
        };
        assert_eq!(
            (record.run.as_str(), record.reviews_at_head, record.open),
            ("rv-7k2m", 2, Some(1))
        );
        assert_eq!(
            answered_id("Fixed in 9e8d7c6.\n\nsig\nRe: rv-7k2m.c1p0.r1.f2", false).as_deref(),
            Some("rv-7k2m.c1p0.r1.f2")
        );
        assert_eq!(answered_id("Re: rv-7k2m.c1p0.r1.f2", false), None);
        assert_eq!(
            answered_id("Not fixed: x\n<!-- agent-review id=h1/c2 -->\nRe: h1", true).as_deref(),
            Some("h1")
        );
        assert_eq!(
            fixed_sha("Fixed in 9e8d7c6.\nRe: x").as_deref(),
            Some("9e8d7c6")
        );
    }

    #[test]
    fn parses_the_emoji_format() {
        let located = "`docs/a.md:3`\n\n**🐛 bug (high):** x\n\n> <sub>m1 xhigh · codex-cli</sub>";
        assert!(matches!(
            finding_kind(located),
            (ThreadType::Bug, Some(Severity::High))
        ));
        assert!(matches!(
            finding_kind("**❓ question:** why?"),
            (ThreadType::Question, None)
        ));
        let fixed = "> ✅ Fixed in [9e8d7c6 fix(a): b \\[c\\]](https://h/o/r/commit/9e8d7c6). \
                     <sub>m · Claude Code · session 6f1d2b9a</sub>\n\
                     <!-- agent-review id=h1/c2 attempt=at-1 -->";
        assert_eq!(fixed_sha(fixed).as_deref(), Some("9e8d7c6"));
        let rejected = "> 🚫 Rejected: capped at 64 KiB. <sub>m · Claude Code · session 6f1d2b9a</sub>\n\
                        <!-- agent-review id=h1/c2 attempt=at-1 -->";
        assert_eq!(
            rejected_reason(rejected).as_deref(),
            Some("capped at 64 KiB")
        );
        assert_eq!(
            rejected_reason("Rejected: by design.\n<!-- agent-review id=h1/c2 -->").as_deref(),
            Some("by design")
        );
    }
}
