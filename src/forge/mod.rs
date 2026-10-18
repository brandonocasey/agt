pub mod anchors;
pub mod auto;
#[cfg(test)]
mod contract;
pub mod github;
pub mod gitlab;
pub mod issue;
pub mod landed;
pub mod markers;
pub mod service;
pub mod transport;

use serde_json::Value;

use crate::core::error::{Error, Result};
use crate::forge::service::SERVICES;
use crate::store::model::{RemoteThread, Review, ReviewState};

/// A `--pr` argument: a number, or a PR/MR URL that also names the host and repository.
pub struct PrRef {
    pub number: u64,
    pub location: Option<(String, String)>,
}

pub fn parse_pr(arg: &str) -> Result<PrRef> {
    if let Ok(number) = arg.trim_start_matches(['#', '!']).parse() {
        return Ok(PrRef {
            number,
            location: None,
        });
    }
    let invalid = || Error(format!("--pr must be a number or a PR/MR URL: {arg}"));
    let (_, rest) = arg.split_once("://").ok_or_else(invalid)?;
    let (host, path) = rest.split_once('/').ok_or_else(invalid)?;
    let (repo, number) = SERVICES
        .iter()
        .filter(|kind| path.contains(kind.url_marker))
        // The longest marker is the most specific, so a project named `pull` on GitLab is not
        // mistaken for a GitHub PR path.
        .max_by_key(|kind| kind.url_marker.len())
        .and_then(|kind| path.split_once(kind.url_marker))
        .ok_or_else(invalid)?;
    let number = number
        .split(['/', '#', '?'])
        .next()
        .and_then(|number| number.parse().ok())
        .ok_or_else(invalid)?;
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    Ok(PrRef {
        number,
        location: Some((host.to_ascii_lowercase(), repo.trim_matches('/').to_owned())),
    })
}

/// The web page of `sha` in the repository of the PR/MR `review`.
pub fn commit_url(review: &Review, sha: &str) -> Option<String> {
    let kind = service::kind(review.forge);
    let (repo, _) = review.url.rsplit_once(kind.url_marker)?;
    Some(format!("{repo}{}{sha}", kind.commit_marker))
}

/// Percent-encodes `text` for a URL path segment, keeping `~` as RFC 3986 does.
pub fn encode(text: &str) -> String {
    percent_encode(text, b"~")
}

/// Percent-encodes every byte except letters, digits, `-`, `_`, `.` and those in `keep`.
pub fn percent_encode(text: &str, keep: &[u8]) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.')
            || keep.contains(&byte)
        {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub struct PrInfo {
    pub repo: String,
    pub url: String,
    pub state: ReviewState,
    pub closed_at: Option<String>,
    pub head_branch: String,
    pub head_sha: String,
    pub base_sha: Option<String>,
    pub start_sha: Option<String>,
    pub author: Option<String>,
}

pub struct CommentData {
    pub id: String,
    pub url: Option<String>,
    pub body: String,
    pub created_at: String,
    pub login: Option<String>,
    pub editor: Option<String>,
    pub review_id: Option<String>,
}

pub struct ThreadData {
    pub info: RemoteThread,
    pub resolved: bool,
    pub comments: Vec<CommentData>,
    /// GitLab position head, used to decide whether the line still maps to the latest diff.
    pub position_head: Option<String>,
}

pub struct Draft {
    pub id: String,
    pub body: String,
}

pub struct Fetched {
    pub pr: PrInfo,
    pub threads: Vec<ThreadData>,
    pub drafts: Vec<Draft>,
}

/// The review `search` finds in `origin` or, failing that, in the repository it was forked from;
/// returns that repository and the number.
pub fn in_origin_or_parent(
    origin: &str,
    search: impl Fn(&str) -> Result<Option<u64>>,
    parent: impl FnOnce() -> Result<Option<String>>,
) -> Result<Option<(String, u64)>> {
    if let Some(number) = search(origin)? {
        return Ok(Some((origin.to_owned(), number)));
    }
    let Some(parent) = parent()? else {
        return Ok(None);
    };
    Ok(search(&parent)?.map(|number| (parent, number)))
}

pub fn text(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

pub fn number(value: &Value, pointer: &str) -> Option<u32> {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
}

/// A JSON id that may be a number or a string.
pub fn id(value: &Value, pointer: &str) -> Option<String> {
    match value.pointer(pointer)? {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) => Some(text.clone()),
        _ => None,
    }
}

pub fn required(value: &Value, pointer: &str, what: &str) -> Result<String> {
    text(value, pointer).ok_or_else(|| Error(format!("{what} is missing {pointer}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pr_arguments() {
        assert_eq!(parse_pr("42").unwrap().number, 42);
        let github = parse_pr("https://github.com/o/r/pull/1235/files").unwrap();
        assert_eq!(github.number, 1235);
        assert_eq!(github.location, Some(("github.com".into(), "o/r".into())));
        let gitlab =
            parse_pr("https://gitlab.example.com/g/s/p/-/merge_requests/3#note_1").unwrap();
        assert_eq!(gitlab.number, 3);
        assert_eq!(
            gitlab.location,
            Some(("gitlab.example.com".into(), "g/s/p".into()))
        );
        assert!(parse_pr("nope").is_err());
    }

    #[test]
    fn a_gitlab_project_named_pull_is_still_a_merge_request() {
        let pr = parse_pr("https://gitlab.com/acme/pull/-/merge_requests/7").unwrap();
        assert_eq!(pr.number, 7);
        assert_eq!(
            pr.location,
            Some(("gitlab.com".to_owned(), "acme/pull".to_owned()))
        );
        assert_eq!(encode("feat/a b"), "feat%2Fa%20b");
    }
}
