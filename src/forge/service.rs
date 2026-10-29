//! The seam between the workflows and a git hosting service. `sync` and `publish` speak only to
//! [`ReviewService`] and [`Capabilities`], issue sync to [`IssueService`]; everything that is specific to GitHub, GitLab or a
//! later service (REST paths, tool flags, GraphQL, response shapes) lives behind it.

use serde_json::Value;

use crate::core::error::Result;
use crate::forge::Fetched;
use crate::forge::issue::{
    Changes, IssueCaps, IssueData, IssueFetched, IssueHierarchy, ListQuery, NewIssue,
};
use crate::forge::transport::{Response, Transport, TransportError};
use crate::git::Repo;
use crate::store::model::{Anchor, Forge, RemoteKind, RemoteThread, Review};

/// The PR/MR a write or read is about.
#[derive(Clone, Copy)]
pub struct ReviewSubject<'a> {
    pub repo: &'a str,
    pub number: u64,
}

/// The issue a write or read is about. A different type from [`ReviewSubject`], so an issue
/// number cannot reach a review call.
#[derive(Clone, Copy)]
pub struct IssueSubject<'a> {
    pub repo: &'a str,
    pub number: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// One `name=value` GraphQL variable, or the `query` itself; `typed` values are sent as numbers
/// or booleans, not strings.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Var {
    pub name: String,
    pub value: String,
    pub typed: bool,
}

impl Var {
    pub fn text(name: &str, value: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            value: value.into(),
            typed: false,
        }
    }

    pub fn typed(name: &str, value: impl Into<String>) -> Self {
        Self {
            typed: true,
            ..Self::text(name, value)
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Endpoint {
    Rest(String),
    Graphql(Vec<Var>),
}

/// A service call, independent of how it is carried out.
#[derive(Clone, PartialEq, Debug)]
pub struct Request {
    pub method: Method,
    pub endpoint: Endpoint,
    pub body: Option<Value>,
    /// Read every page of a list; see [`Pagination::CliPaginate`].
    pub paginate: bool,
    /// The reply is text, not JSON; the response body is a string.
    pub raw: bool,
}

impl Request {
    pub fn rest(method: Method, path: impl Into<String>, body: Option<Value>) -> Self {
        Self {
            method,
            endpoint: Endpoint::Rest(path.into()),
            body,
            paginate: false,
            raw: false,
        }
    }

    pub fn get(path: impl Into<String>) -> Self {
        Self::rest(Method::Get, path, None)
    }

    pub fn graphql(vars: Vec<Var>) -> Self {
        Self {
            method: Method::Post,
            endpoint: Endpoint::Graphql(vars),
            body: None,
            paginate: false,
            raw: false,
        }
    }
}

/// How a service pages a list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pagination {
    /// GraphQL `pageInfo` cursors, which the service's own fetch follows.
    GraphqlCursor,
    /// The CLI joins the pages (`--paginate`).
    CliPaginate,
    /// A `Link: <url>; rel="next"` header.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "no service uses it yet; the pager tests build it")
    )]
    LinkHeader,
    /// A JSON pointer to the next page's URL in the body.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "no service uses it yet; the pager tests build it")
    )]
    NextUrlInBody(&'static str),
    /// A response header holding the token for the next page.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "no service uses it yet; the pager tests build it")
    )]
    ContinuationHeader(&'static str),
}

/// How a service takes in-diff comments, with the calls that fit it.
#[derive(Clone, Copy)]
pub enum Inline<'a> {
    /// One review holds every in-diff comment of a commit (GitHub).
    ReviewBatch(&'a dyn BatchPoster),
    /// A draft note per comment, published right after it is created (GitLab).
    DraftThenPublish(&'a dyn DraftPoster),
}

/// A service that holds the in-diff comments of a commit in one review.
pub trait BatchPoster {
    /// A review with a summary and in-diff comments of one commit.
    fn review_batch(
        &self,
        subject: ReviewSubject,
        commit: &str,
        summary: &str,
        comments: &[(Position, String)],
    ) -> Request;
}

/// A service that takes an in-diff comment as a draft note, published right after.
pub trait DraftPoster {
    /// A draft note holding an in-diff comment.
    fn draft(&self, subject: ReviewSubject, body: &str, position: &Position) -> Request;
    fn publish_draft(&self, subject: ReviewSubject, draft: &str) -> Request;
    /// Drops a draft the host refused to publish.
    fn delete_draft(&self, subject: ReviewSubject, draft: &str) -> Request;
}

/// Which remote thread kinds take a native reply; the rest get a general comment ending in
/// `Re: <thread id>`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReplyCap {
    All,
    ReviewThreads,
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "no service lacks native replies yet; a unit test builds it"
        )
    )]
    None,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ApproveCap {
    /// No formal approval; an approval is only a comment.
    None,
    /// The approval is a review carrying the comment (GitHub). `self_allowed` is whether the
    /// author may approve their own PR.
    Review { self_allowed: bool },
    /// A separate approve call next to a note (GitLab).
    Call,
}

pub struct Capabilities {
    pub pagination: Pagination,
    /// Multi-line comments.
    pub ranges: bool,
    pub reply: ReplyCap,
    /// A thread can be resolved; without it a resolve is only recorded locally.
    pub resolve: bool,
    pub unresolve: bool,
    /// The kind of remote thread a new general comment becomes.
    pub general_kind: RemoteKind,
    /// A new general thread can be resolved on the host.
    pub general_resolvable: bool,
    pub approve: ApproveCap,
    /// Fetched comments name who edited them, so an edit by someone else is detectable.
    pub edits_attributed: bool,
    /// A general note that gets a reply is returned as a discussion with the same id.
    pub notes_become_discussions: bool,
}

impl Capabilities {
    pub fn replies_natively(&self, kind: RemoteKind) -> bool {
        match self.reply {
            ReplyCap::All => true,
            ReplyCap::ReviewThreads => kind == RemoteKind::ReviewThread,
            ReplyCap::None => false,
        }
    }
}

/// Where an in-diff comment goes, opaque to the core. Comments with the same `commit` can be
/// batched in one review.
pub struct Position {
    pub commit: String,
    pub data: Value,
}

pub enum Placement {
    InDiff(Position),
    /// The lines moved on since review, so a general comment names where they were.
    OtherFile,
}

/// A comment the host holds, as listed back after a batched review.
pub struct Posted {
    pub id: String,
    pub url: Option<String>,
    pub body: String,
}

/// What every service offers, whatever the workflow: who it is, what it can do, how to reach it
/// and who the user is on it. Builders are pure; the methods that take `&self` and talk to the
/// host are `login` and `send`.
pub trait Service {
    fn forge(&self) -> Forge;
    /// For messages: `GitHub`, `GitLab`.
    fn name(&self) -> &'static str {
        kind(self.forge()).name
    }
    fn caps(&self) -> &Capabilities;

    fn login(&self) -> Result<String>;

    fn transport(&self) -> &dyn Transport;
    /// What a dry run shows for a request.
    fn render(&self, request: &Request) -> Vec<String> {
        self.transport().render(request)
    }
    /// Runs a write; an empty reply is `null`.
    fn send(&self, request: &Request) -> std::result::Result<Response, TransportError> {
        self.transport().send(request)
    }
}

/// A service as the review workflow uses it. The methods that take `&self` and talk to the host
/// are `find_review`, `fetch` and `review_posted`.
pub trait ReviewService: Service {
    fn find_review(&self, origin: &str, branch: &str) -> Result<Option<(String, u64)>>;
    fn fetch(&self, subject: ReviewSubject) -> Result<Fetched>;
    /// The local id a thread with no agent marker gets, from the host's first comment id.
    fn imported_id(&self, kind: RemoteKind, first: &str) -> String;

    /// Where `anchor`, at `line` of `commit`, goes in the PR/MR diff, or `None` when it does
    /// not fit.
    fn place(
        &self,
        repo: &Repo,
        review: &Review,
        anchor: &Anchor,
        commit: &str,
        line: u32,
    ) -> Option<Placement>;
    /// How in-diff comments are posted, and the calls for it.
    fn inline(&self) -> Inline<'_>;
    /// A new general thread.
    fn thread(&self, subject: ReviewSubject, text: &str) -> Request;
    /// A resolvable thread on the whole of `path` at the head of `review`, for a finding the
    /// diff cannot place on a line; `None` when the host has no such thread. `old_path` is the
    /// path the diff renames `path` from, else `path`.
    fn file_thread(
        &self,
        _subject: ReviewSubject,
        _review: &Review,
        _old_path: &str,
        _path: &str,
        _text: &str,
    ) -> Option<Request> {
        None
    }
    /// A general comment that does not block merging.
    fn note(&self, subject: ReviewSubject, text: &str) -> Request;
    /// New text for the posted general comment `id`, such as the summary.
    fn edit_note(&self, subject: ReviewSubject, id: &str, text: &str) -> Request;
    /// A native reply; only for kinds [`Capabilities::replies_natively`] accepts.
    fn reply(&self, subject: ReviewSubject, remote: &RemoteThread, text: &str) -> Request;
    /// Only for threads of a service with `caps().resolve`; `resolved == false` reopens.
    fn resolve(&self, subject: ReviewSubject, remote: &RemoteThread, resolved: bool) -> Request;
    /// The formal approval of `head`; the review carries `comment` when the service approves
    /// by review.
    fn approve(&self, subject: ReviewSubject, head: &str, comment: Option<&str>) -> Request;

    /// The id and URL of a comment from the response to a thread, reply, note or approval.
    fn comment_created(
        &self,
        review_url: Option<&str>,
        response: &Value,
    ) -> Option<(String, Option<String>)>;
    fn draft_created(&self, response: &Value) -> Option<String>;
    /// The id of a posted review and the comments it holds.
    fn review_posted(
        &self,
        subject: ReviewSubject,
        response: &Value,
    ) -> Result<(String, Vec<Posted>)>;
}

/// A service as issue sync uses it. Builders are pure unless they return a `Result`.
pub trait IssueService: Service {
    /// The issues of `query.repo` updated since `query.since`, pull requests left out.
    fn list_changed(&self, query: &ListQuery) -> Result<Vec<IssueData>>;
    /// The issue, its comments and the PRs/MRs the host links to it.
    fn fetch_issue(&self, subject: IssueSubject) -> Result<IssueFetched>;
    fn hierarchy(&self, subject: IssueSubject) -> Result<IssueHierarchy>;
    fn set_parent(
        &self,
        subject: IssueSubject,
        parent: Option<&str>,
        previous: Option<&str>,
    ) -> Result<Request>;
    fn parent_updated(&self, response: &Value) -> Result<()>;
    /// A new comment on the issue.
    fn comment(&self, subject: IssueSubject, text: &str) -> Request;
    /// One request that applies `changes`. May read from the host, such as a user id lookup.
    fn update(&self, subject: IssueSubject, changes: &Changes) -> Result<Request>;
    fn create(&self, repo: &str, new: &NewIssue) -> Result<Request>;
    /// The issue from the response to [`IssueService::create`].
    fn created(&self, response: &Value) -> Result<IssueData>;
    /// The id and URL of a comment from the response to [`IssueService::comment`].
    fn issue_comment_created(&self, response: &Value) -> Option<(String, Option<String>)>;
}

/// One registered service: what is needed to find it for a host and build it. Adding a service
/// means adding its entry to [`SERVICES`].
pub struct Kind {
    pub forge: Forge,
    /// The service's name in messages: `GitHub`, `GitLab`.
    pub name: &'static str,
    /// Start of the ref on `origin` that holds the head of PR/MR `n`: `<prefix>n/head`.
    pub head_ref_prefix: &'static str,
    /// Prefix of the keys and imported ids: `gh-42`, `gl-7`.
    pub key_prefix: &'static str,
    /// The command line tool that talks to the service: `gh`, `glab`.
    pub tool: &'static str,
    /// Path segment of a PR/MR URL between the repository and the number.
    pub url_marker: &'static str,
    /// Path segment of a pipeline URL between the repository and the number, if it has pipelines.
    pub pipeline_marker: Option<&'static str>,
    /// Path segment of a commit URL between the repository and the sha.
    pub commit_marker: &'static str,
    /// Hosts known without asking anything.
    pub well_known: fn(&str) -> bool,
    /// The service's own configuration names this host.
    pub configured: fn(&str) -> bool,
    /// The service's CLI is logged in to this host.
    pub probe: fn(&str) -> bool,
    pub connect: fn(&str) -> Box<dyn ReviewService>,
    pub issue_caps: &'static IssueCaps,
    pub connect_issue: fn(&str) -> Box<dyn IssueService>,
}

pub const SERVICES: &[Kind] = &[crate::forge::github::KIND, crate::forge::gitlab::KIND];

fn by(host: &str, check: fn(&Kind) -> fn(&str) -> bool) -> Option<Forge> {
    SERVICES
        .iter()
        .find(|kind| check(kind)(host))
        .map(|kind| kind.forge)
}

/// The forge of `host` from what is known without asking a CLI whether it is logged in. In
/// order: `named`, a forge the user named for the host, hosts every service knows by name,
/// `cached`, a forge this repository last used the host with, then each service's own
/// configuration.
pub fn detect_known(host: &str, named: Option<Forge>, cached: Option<Forge>) -> Option<Forge> {
    named
        .or_else(|| by(host, |kind| kind.well_known))
        .or(cached)
        .or_else(|| by(host, |kind| kind.configured))
}

/// The forge of `host`, never guessed: [`detect_known`], then each service's login, which is the
/// slowest to ask.
pub fn detect(host: &str, named: Option<Forge>, cached: Option<Forge>) -> Option<Forge> {
    detect_known(host, named, cached).or_else(|| by(host, |kind| kind.probe))
}

pub fn kind(forge: Forge) -> &'static Kind {
    SERVICES
        .iter()
        .find(|kind| kind.forge == forge)
        .unwrap_or_else(|| unreachable!("every forge is registered in SERVICES"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_detected_in_a_fixed_order() {
        // The named forge wins over a name every service knows, which wins over the cache.
        let forge = |result: Option<Forge>| result.unwrap();
        assert!(forge(detect("github.com", Some(Forge::Gitlab), None)) == Forge::Gitlab);
        assert!(forge(detect("github.com", None, Some(Forge::Gitlab))) == Forge::Github);
        assert!(forge(detect("gitlab.com", None, None)) == Forge::Gitlab);
        assert!(forge(detect_known("gitlab.com", None, None)) == Forge::Gitlab);
        assert!(detect_known("git.example.invalid", None, None).is_none());
        // A cached host needs no probe of any CLI.
        assert!(forge(detect("git.example.invalid", None, Some(Forge::Gitlab))) == Forge::Gitlab);
        assert!(
            forge(detect(
                "git.example.invalid",
                Some(Forge::Github),
                Some(Forge::Gitlab)
            )) == Forge::Github
        );
    }
}
