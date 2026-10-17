//! The hidden `<!-- ... -->` markers `publish` writes into posted bodies and `sync` reads back:
//! the local id and attempt of a comment, the id and head of a finding, and the run record.
//! Writers and parsers live together so the format has one definition.

const AGENT: &str = "<!-- agent-review id=";
const FINDING: &str = "<!-- review-finding v1 id=";
const RECORD: &str = "<!-- review-record v2 ";
const CLOSE: &str = "-->";
/// Local id prefix of the body of a GitHub review that `publish` posts; sync skips those.
const REVIEW_ID: &str = "review/";
const RECORD_SUFFIX: &str = "/record";

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

/// The local id of the body of the review that carries a batch of comments at `commit`.
pub fn review_id(commit: &str) -> String {
    format!("{REVIEW_ID}{commit}")
}

pub fn is_review_id(id: &str) -> bool {
    id.starts_with(REVIEW_ID)
}

/// The local id of the record comment of `run`.
pub fn record_id(run: &str) -> String {
    format!("{run}{RECORD_SUFFIX}")
}

/// The run of a record comment's local id.
pub fn record_run(id: &str) -> Option<&str> {
    id.strip_suffix(RECORD_SUFFIX)
}

fn first_token(body: &str, marker: &str) -> Option<String> {
    let rest = body.split_once(marker)?.1;
    let id = rest.split_whitespace().next()?.trim_end_matches(CLOSE);
    (!id.is_empty()).then(|| id.to_owned())
}

/// The local id in an `<!-- agent-review id=<id> -->` marker, which `publish` adds to every body.
pub fn agent_id(body: &str) -> Option<String> {
    first_token(body, AGENT)
}

/// The attempt id in an `<!-- agent-review id=<id> attempt=<attempt> -->` marker.
pub fn agent_attempt(body: &str) -> Option<&str> {
    let inner = body.split_once(AGENT)?.1.split_once(CLOSE)?.0;
    inner
        .split_whitespace()
        .find_map(|pair| pair.strip_prefix("attempt="))
}

/// `body` without its `agent-review` marker and the blank space before it, as a person reads it.
pub fn strip_agent(body: &str) -> String {
    let Some(start) = body.find(AGENT) else {
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
    first_token(body, FINDING)
}

/// The fields of a `review-record` marker.
pub struct RecordMarker {
    pub run: String,
    pub head: String,
    pub reviews_at_head: u32,
    pub open: Option<u32>,
}

pub fn parse_record(body: &str) -> Option<RecordMarker> {
    let inner = body.split_once(RECORD)?.1.split_once(CLOSE)?.0;
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
        assert_eq!(record_id("rv-7k2m"), "rv-7k2m/record");
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
        let marker = parse_record(&record("rv-7k2m", "abc", 2, 1)).unwrap();
        assert_eq!(
            (
                marker.run.as_str(),
                marker.head.as_str(),
                marker.reviews_at_head,
                marker.open
            ),
            ("rv-7k2m", "abc", 2, Some(1))
        );
        assert_eq!(record_run("rv-7k2m/record"), Some("rv-7k2m"));
        assert!(is_review_id(&review_id("abc")));
        assert_eq!(
            agent_id(&agent(&review_id("abc"), None)).as_deref(),
            Some("review/abc")
        );
        assert_eq!(agent_attempt("no marker"), None);
    }
}
