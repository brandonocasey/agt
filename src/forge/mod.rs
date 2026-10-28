pub mod anchors;
pub mod auto;
#[cfg(test)]
mod contract;
pub mod github;
pub mod gitlab;
pub mod hosts;
pub mod issue;
pub mod landed;
pub mod markers;
pub mod select;
pub mod service;
pub mod transport;

use serde_json::Value;

use crate::core::error::{Error, Result};
use crate::forge::service::SERVICES;
use crate::forge::transport::TransportError;
use crate::store::model::{
    Author, Comment, Forge, PostState, RemoteComment, RemoteThread, Review, ReviewState,
};

/// A `--pr` argument: a number, or a PR/MR URL that also names the host and repository.
pub struct PrRef {
    pub number: u64,
    pub location: Option<(String, String)>,
    /// The service whose URL it is; `None` for a bare number.
    pub forge: Option<Forge>,
    /// The URL names a pipeline, not a PR/MR; only when pipeline URLs are accepted.
    pub pipeline: bool,
}

pub fn parse_pr(arg: &str) -> Result<PrRef> {
    parse_ref(arg, false)
}

/// Like [`parse_pr`]; `pipelines` also accepts a pipeline URL.
pub fn parse_ref(arg: &str, pipelines: bool) -> Result<PrRef> {
    if let Ok(number) = arg.trim_start_matches(['#', '!']).parse() {
        return Ok(PrRef {
            number,
            location: None,
            forge: None,
            pipeline: false,
        });
    }
    let invalid = || Error(format!("--pr must be a number or a PR/MR URL: {arg}"));
    let (_, rest) = arg.split_once("://").ok_or_else(invalid)?;
    let (host, path) = rest.split_once('/').ok_or_else(invalid)?;
    let (kind, marker, pipeline) = SERVICES
        .iter()
        .flat_map(|kind| {
            let pipeline = kind.pipeline_marker.filter(|_| pipelines);
            [(kind, kind.url_marker, false)]
                .into_iter()
                .chain(pipeline.map(|marker| (kind, marker, true)))
        })
        .filter(|(_, marker, _)| path.contains(marker))
        // The longest marker is the most specific, so a project named `pull` on GitLab is not
        // mistaken for a GitHub PR path.
        .max_by_key(|(_, marker, _)| marker.len())
        .ok_or_else(invalid)?;
    let (repo, number) = path.split_once(marker).ok_or_else(invalid)?;
    let number = number
        .split(['/', '#', '?'])
        .next()
        .and_then(|number| number.parse().ok())
        .ok_or_else(invalid)?;
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    Ok(PrRef {
        number,
        location: Some((host.to_ascii_lowercase(), repo.trim_matches('/').to_owned())),
        forge: Some(kind.forge),
        pipeline,
    })
}

/// The state a GitHub `state` (`OPEN`, `CLOSED`, `MERGED`) names; `None` for any other.
pub fn github_state(text: &str) -> Option<ReviewState> {
    match text {
        "OPEN" => Some(ReviewState::Open),
        "CLOSED" => Some(ReviewState::Closed),
        "MERGED" => Some(ReviewState::Merged),
        _ => None,
    }
}

/// The state a GitLab `state` names; `None` for any other. `locked` is an MR being merged, so
/// still open.
pub fn gitlab_state(text: &str) -> Option<ReviewState> {
    match text {
        "opened" | "locked" => Some(ReviewState::Open),
        "closed" => Some(ReviewState::Closed),
        "merged" => Some(ReviewState::Merged),
        _ => None,
    }
}

/// Whether a check run, status or pipeline exists for `head_sha`, the head of PR/MR `number` of
/// `repo` on `host`.
pub fn head_has_pipeline(
    forge: Forge,
    host: &str,
    repo: &str,
    number: u64,
    head_sha: &str,
) -> std::result::Result<bool, TransportError> {
    match forge {
        Forge::Github => {
            let github = github::connect(host);
            Ok(github.pull_checks(repo, number)?.rollup > 0)
        }
        Forge::Gitlab => {
            let gitlab = gitlab::connect(host);
            gitlab.head_pipeline_runs(&gitlab::project_path(repo), number, head_sha)
        }
    }
}

/// The owner and name of `owner/name`; `None` without a slash.
pub fn split_repo_strict(repo: &str) -> Option<(&str, &str)> {
    repo.split_once('/')
}

/// The owner and name of `owner/name`; both empty without a slash.
pub fn split_repo(repo: &str) -> (&str, &str) {
    split_repo_strict(repo).unwrap_or_default()
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

/// The fields of a PR/MR node that every read of one shares, as the forge wrote them. Each reader
/// decides what a missing number, state or fork flag means.
pub struct PullNode {
    pub number: Option<u64>,
    pub url: String,
    pub title: String,
    /// `None` for a state this build does not know.
    pub state: Option<ReviewState>,
    /// The node has a merge timestamp.
    pub merged: bool,
    /// When it was closed, as the forge wrote it.
    pub closed_at: Option<String>,
    pub base: String,
    pub head_branch: String,
    pub head_sha: String,
    /// Whether the head branch lives in the repository itself; `None` when the node does not say.
    pub same_repo: Option<bool>,
    pub created_at: Option<String>,
    pub author: Option<String>,
    pub draft: bool,
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
    pub created_at: Option<String>,
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

impl CommentData {
    /// The host's side of the comment; `me` is the login agt posts as.
    pub fn remote(&self, me: &str) -> RemoteComment {
        RemoteComment {
            id: self.id.clone(),
            url: self.url.clone(),
            login: self.login.clone(),
            is_me: self.login.as_deref() == Some(me),
            editor: self.editor.clone(),
            review_id: self.review_id.clone(),
        }
    }

    /// The comment as a local one with the host's `body`; `missing_login` names an author the
    /// host no longer knows.
    pub fn imported(&self, me: &str, body: String, missing_login: &str) -> Comment {
        let author = Author::Human {
            login: self
                .login
                .clone()
                .unwrap_or_else(|| missing_login.to_owned()),
            git_name: false,
        };
        Comment::new(
            Comment::imported_id(&self.id),
            author,
            body,
            self.created_at.clone(),
            PostState::Posted,
            Some(self.remote(me)),
        )
    }
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
    text(value, pointer).ok_or_else(|| missing(pointer, what))
}

/// The error for a field `what` lacks at `pointer`.
pub fn missing(pointer: &str, what: &str) -> Error {
    Error(format!("{what} is missing {pointer}"))
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
