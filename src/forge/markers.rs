//! The hidden `<!-- ... -->` markers `publish` writes into posted bodies and `sync` reads back:
//! the local id and attempt of a comment, the id and head of a finding, and the run records a
//! summary holds.
//! Writers and parsers live together so the format has one definition.

use crate::store::model::Delivery;

const AGENT: &str = "<!-- agent-review id=";
const FINDING: &str = "<!-- review-finding v1 id=";
const RECORD: &str = "<!-- review-record v2 ";
const QA_RECORD: &str = "<!-- qa-record v1 ";
const CLOSE: &str = "-->";
/// Local id prefix of the body of a GitHub review that `publish` posts; sync skips those.
const REVIEW_ID: &str = "review/";
const RECORD_SUFFIX: &str = "/record";
/// Local id of the one summary comment of a PR/MR, which holds the record of every run.
pub const SUMMARY_ID: &str = "summary";
const PART_INFIX: &str = "/p";

/// Local id of part `number` (2 or more) of a summary that does not fit one comment.
pub fn summary_part_id(number: usize) -> String {
    format!("{SUMMARY_ID}{PART_INFIX}{number}")
}

/// The number of the summary part with local id `id`.
pub fn summary_part(id: &str) -> Option<usize> {
    id.strip_prefix(SUMMARY_ID)?
        .strip_prefix(PART_INFIX)?
        .parse()
        .ok()
        .filter(|number| *number >= 2)
}

/// The marker that ties a posted body to a local comment id and, once sent, its attempt.
pub fn agent(id: &str, attempt: Option<&str>) -> String {
    match attempt {
        Some(attempt) => format!("{AGENT}{id} attempt={attempt} {CLOSE}"),
        None => format!("{AGENT}{id} {CLOSE}"),
    }
}

/// The marker that names the finding a posted thread carries and the commit it was found at.
pub fn finding(id: &str, sha: &str) -> String {
    format!("{FINDING}{id} sha={sha} {CLOSE}")
}

/// The marker of a run record.
pub fn record(run: &str, head: &str, reviews_at_head: usize, open: usize) -> String {
    format!("{RECORD}id={run} head={head} reviews-at-head={reviews_at_head} open={open} {CLOSE}")
}

/// The marker of a QA run record. It differs from a review record so no QA run ever counts as
/// a review.
pub fn qa_record(run: &str, head: &str) -> String {
    format!("{QA_RECORD}id={run} head={head} {CLOSE}")
}

/// The local id of the body of the review that carries a batch of comments at `commit`.
pub fn review_id(commit: &str) -> String {
    format!("{REVIEW_ID}{commit}")
}

pub fn is_review_id(id: &str) -> bool {
    id.starts_with(REVIEW_ID)
}

/// The run of the local id of a run's own record comment, which earlier builds posted.
pub fn record_run(id: &str) -> Option<&str> {
    id.strip_suffix(RECORD_SUFFIX)
}

/// The token after the last `marker`: `publish` writes its markers after the text, so a marker
/// the text quotes comes first.
fn last_token(body: &str, marker: &str) -> Option<String> {
    let rest = body.rsplit_once(marker)?.1;
    let id = rest.split_whitespace().next()?.trim_end_matches(CLOSE);
    (!id.is_empty()).then(|| id.to_owned())
}

/// The local id in an `<!-- agent-review id=<id> -->` marker, which `publish` adds to every body.
pub fn agent_id(body: &str) -> Option<String> {
    last_token(body, AGENT)
}

/// The attempt id in an `<!-- agent-review id=<id> attempt=<attempt> -->` marker.
pub fn agent_attempt(body: &str) -> Option<&str> {
    let inner = body.rsplit_once(AGENT)?.1.split_once(CLOSE)?.0;
    inner
        .split_whitespace()
        .find_map(|pair| pair.strip_prefix("attempt="))
}

/// Whether `body` is the landed write of the item `local_id` whose delivery is `delivery`: one
/// being posted, marked with its id and the same attempt.
pub fn is_landed_as(delivery: &Delivery, local_id: &str, body: &str) -> bool {
    delivery.is_posting()
        && agent_id(body).as_deref() == Some(local_id)
        && delivery.is_attempt(agent_attempt(body))
}

/// `body` without its `agent-review` marker and the blank space before it, as a person reads it.
pub fn strip_agent(body: &str) -> String {
    let Some(start) = body.rfind(AGENT) else {
        return body.to_owned();
    };
    let Some(len) = body[start..].find(CLOSE) else {
        return body.to_owned();
    };
    let after = &body[start + len + CLOSE.len()..];
    format!("{}{after}", body[..start].trim_end())
        .trim_end()
        .to_owned()
}

/// The finding id in a `review-finding` marker.
pub fn finding_id(body: &str) -> Option<String> {
    last_token(body, FINDING)
}

/// The fields of a `review-record` marker.
pub struct RecordMarker {
    pub run: String,
    pub head: String,
    pub reviews_at_head: u32,
    pub open: Option<u32>,
}

/// Every `review-record` marker of `body`: one in a run's own record, one per run in a
/// summary. Only the marker lines that end the body count, not a record the text quotes.
pub fn parse_records(body: &str) -> Vec<RecordMarker> {
    trailer(body)
        .into_iter()
        .filter_map(|line| {
            let inner = line.strip_prefix(RECORD)?.strip_suffix(CLOSE)?;
            let field = |name: &str| {
                inner
                    .split_whitespace()
                    .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
            };
            Some(RecordMarker {
                run: field("id")?.to_owned(),
                head: field("head")?.to_owned(),
                reviews_at_head: field("reviews-at-head")?.parse().ok()?,
                open: field("open").and_then(|open| open.parse().ok()),
            })
        })
        .collect()
}

/// The run ids of every `qa-record` marker that ends `body`.
pub fn parse_qa_records(body: &str) -> Vec<String> {
    trailer(body)
        .into_iter()
        .filter_map(|line| {
            let inner = line.strip_prefix(QA_RECORD)?.strip_suffix(CLOSE)?;
            inner
                .split_whitespace()
                .find_map(|pair| pair.strip_prefix("id="))
                .map(str::to_owned)
        })
        .collect()
}

/// The marker lines that end `body`, in order.
fn trailer(body: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = body
        .lines()
        .rev()
        .map(str::trim)
        .skip_while(|line| line.is_empty())
        .take_while(|line| line.starts_with("<!--") && line.ends_with(CLOSE))
        .collect();
    lines.reverse();
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_agent_removes_only_the_marker() {
        let body = format!("Text\n\n{}", agent("c1", Some("at-1")));
        assert_eq!(strip_agent(&body), "Text");
        assert_eq!(strip_agent("Plain"), "Plain");
        assert_eq!(
            strip_agent("<!-- agent-review id=x"),
            "<!-- agent-review id=x"
        );
    }

    #[test]
    fn writers_match_the_wire_format() {
        assert_eq!(agent("h1/c2", None), "<!-- agent-review id=h1/c2 -->");
        assert_eq!(
            agent("h1/c2", Some("at-1")),
            "<!-- agent-review id=h1/c2 attempt=at-1 -->"
        );
        assert_eq!(
            finding("rv-1.c1p0.r1.f1", "abc"),
            "<!-- review-finding v1 id=rv-1.c1p0.r1.f1 sha=abc -->"
        );
        assert_eq!(
            record("rv-7k2m", "abc", 2, 1),
            "<!-- review-record v2 id=rv-7k2m head=abc reviews-at-head=2 open=1 -->"
        );
        assert_eq!(review_id("abc"), "review/abc");
        assert_eq!(
            qa_record("qa-7k2m", "abc"),
            "<!-- qa-record v1 id=qa-7k2m head=abc -->"
        );
    }

    #[test]
    fn qa_records_are_not_review_records() {
        let body = format!(
            "text\n{}\n{}\n{}",
            qa_record("qa-1", "abc"),
            record("rv-1", "abc", 1, 0),
            agent(SUMMARY_ID, None)
        );
        assert_eq!(parse_qa_records(&body), ["qa-1"]);
        let runs: Vec<String> = parse_records(&body).into_iter().map(|r| r.run).collect();
        assert_eq!(runs, ["rv-1"]);
        assert!(parse_qa_records(&format!("{}\ntext", qa_record("qa-1", "abc"))).is_empty());
    }

    #[test]
    fn parsers_read_what_writers_write() {
        let body = format!(
            "text\n{}\n{}",
            finding("rv-1.c1p0.r1.f1", "abc"),
            agent("rv-1.c1p0.r1.f1/c1", Some("at-9"))
        );
        assert_eq!(finding_id(&body).as_deref(), Some("rv-1.c1p0.r1.f1"));
        assert_eq!(agent_id(&body).as_deref(), Some("rv-1.c1p0.r1.f1/c1"));
        assert_eq!(agent_attempt(&body), Some("at-9"));
        let summary = format!(
            "text\n{}\n{}\n{}",
            record("rv-7k2m", "abc", 2, 1),
            record("rv-9x", "def", 0, 0),
            agent(SUMMARY_ID, Some("at-9"))
        );
        let markers: Vec<_> = parse_records(&summary)
            .into_iter()
            .map(|marker| (marker.run, marker.head, marker.reviews_at_head, marker.open))
            .collect();
        assert_eq!(
            markers,
            [
                ("rv-7k2m".to_owned(), "abc".to_owned(), 2, Some(1)),
                ("rv-9x".to_owned(), "def".to_owned(), 0, Some(0)),
            ]
        );
        assert_eq!(agent_id(&summary).as_deref(), Some(SUMMARY_ID));
        assert!(parse_records("no marker").is_empty());
        assert_eq!(record_run("rv-7k2m/record"), Some("rv-7k2m"));
        assert_eq!(summary_part(&summary_part_id(3)), Some(3));
        assert_eq!(summary_part(SUMMARY_ID), None);
        assert_eq!(summary_part("summary/p1"), None);
        assert!(is_review_id(&review_id("abc")));
        assert_eq!(
            agent_id(&agent(&review_id("abc"), None)).as_deref(),
            Some("review/abc")
        );
        assert_eq!(agent_attempt("no marker"), None);
    }

    #[test]
    fn markers_the_text_quotes_are_not_the_body_s() {
        let quoted = format!(
            "Quotes `{}` and\n{}\n\nmore text.",
            agent("x", Some("at-x")),
            record("rv-quoted", "abc", 1, 0)
        );
        let body = format!(
            "{quoted}\n{}\n{}\n",
            record("rv-7k2m", "def", 2, 1),
            agent(SUMMARY_ID, Some("at-9"))
        );
        assert_eq!(agent_id(&body).as_deref(), Some(SUMMARY_ID));
        assert_eq!(agent_attempt(&body), Some("at-9"));
        let runs: Vec<_> = parse_records(&body)
            .into_iter()
            .map(|marker| marker.run)
            .collect();
        assert_eq!(runs, ["rv-7k2m"]);
        assert!(strip_agent(&body).starts_with(&quoted));
        assert!(parse_records(&quoted).is_empty());
    }
}
