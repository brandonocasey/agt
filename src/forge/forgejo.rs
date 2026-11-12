//! Forgejo and Gitea pull requests over the REST API at `/api/v1`, through `tea api`. The two
//! share one API, so they are one service.

pub mod ci;
mod issues;

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::core::clock;
use crate::core::error::{Error, Result};
use crate::forge::service::{
    ApproveCap, BatchPoster, Capabilities, Inline, Kind, Method, Pagination, Placement, Position,
    Posted, ReplyCap, Request, ReviewService, ReviewSubject, Service, Wanted,
};
use crate::forge::transport::{CliTransport, Transport, TransportError, tea_login_for, tea_logins};
use crate::forge::{self, CommentData, Fetched, PrInfo, PullNode, ThreadData, anchors};
use crate::git::Repo;
use crate::store::model::{
    Forge, RemoteComment, RemoteKind, RemoteThread, Review, ReviewState, Side,
};

/// Items per page: the most a default Forgejo or Gitea answers with (`MAX_RESPONSE_ITEMS`).
pub const PAGE_SIZE: usize = 50;

pub const KIND: Kind = Kind {
    forge: Forge::Forgejo,
    name: "Forgejo",
    head_ref_prefix: "refs/pull/",
    key_prefix: "fg",
    tool: "tea",
    url_marker: "/pulls/",
    pipeline_marker: Some("/actions/runs/"),
    commit_marker: "/commit/",
    well_known: |host| host == "codeberg.org",
    configured: has_login,
    probe: has_login,
    connect: |host| Box::new(connect(host)),
    issue_caps: &issues::ISSUE_CAPS,
    connect_issue: |host| Box::new(connect(host)),
};

/// Whether tea is logged in to `host`; its logins are all the configuration agt reads.
fn has_login(host: &str) -> bool {
    tea_logins().is_some_and(|logins| tea_login_for(&logins, host).is_some())
}

pub fn connect(host: &str) -> Forgejo {
    Forgejo::new(Box::new(CliTransport::tea(host)))
}

/// The shared fields of a REST pull request node.
pub fn pull_node(node: &Value) -> PullNode {
    let text = |pointer: &str| forge::text(node, pointer).unwrap_or_default();
    let title = text("/title");
    PullNode {
        number: node["number"].as_u64(),
        url: text("/html_url"),
        state: state_of(node),
        merged: node["merged"].as_bool() == Some(true),
        closed_at: forge::text(node, "/closed_at").or_else(|| forge::text(node, "/merged_at")),
        base: text("/base/ref"),
        head_branch: text("/head/ref"),
        head_sha: text("/head/sha"),
        same_repo: match (
            forge::text(node, "/head/repo/full_name"),
            forge::text(node, "/base/repo/full_name"),
        ) {
            (Some(head), Some(base)) => Some(head.eq_ignore_ascii_case(&base)),
            _ => None,
        },
        created_at: forge::text(node, "/created_at"),
        author: forge::text(node, "/user/login"),
        draft: node["draft"].as_bool() == Some(true) || plain(&title) != title,
        title,
    }
}

/// The title without a draft marker such as `WIP: `.
pub fn plain(title: &str) -> &str {
    let lower = title.to_lowercase();
    for marker in ["wip: ", "wip:", "[wip] ", "[wip]"] {
        if lower.starts_with(marker) {
            return title[marker.len()..].trim_start();
        }
    }
    title
}

/// The state of a pull request node: a merged one is `closed` on the host, with `merged` set.
pub fn state_of(node: &Value) -> Option<ReviewState> {
    if node["merged"].as_bool() == Some(true) {
        return Some(ReviewState::Merged);
    }
    match node["state"].as_str()? {
        "open" => Some(ReviewState::Open),
        "closed" => Some(ReviewState::Closed),
        _ => None,
    }
}

// The host has no route to reply to or resolve a review comment, so a reply is a general
// comment and a resolve stays local.
const CAPS: Capabilities = Capabilities {
    pagination: Pagination::PageNumbers(PAGE_SIZE),
    ranges: false,
    reply: ReplyCap::None,
    resolve: false,
    unresolve: false,
    general_kind: RemoteKind::General,
    general_resolvable: false,
    approve: ApproveCap::Review {
        self_allowed: false,
    },
    edits_attributed: false,
    notes_become_discussions: false,
};

pub struct Forgejo {
    transport: Box<dyn Transport>,
}

impl Forgejo {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self { transport }
    }
}

fn base(subject: ReviewSubject) -> String {
    format!("repos/{}/pulls/{}", subject.repo, subject.number)
}

fn comments_path(subject: ReviewSubject) -> String {
    format!("repos/{}/issues/{}/comments", subject.repo, subject.number)
}

fn reviews_path(subject: ReviewSubject) -> String {
    format!("{}/reviews", base(subject))
}

fn comment_path(subject: ReviewSubject, id: &str) -> String {
    format!(
        "repos/{}/issues/comments/{}",
        subject.repo,
        forge::encode(id)
    )
}

impl Service for Forgejo {
    fn forge(&self) -> Forge {
        Forge::Forgejo
    }

    fn caps(&self) -> &Capabilities {
        &CAPS
    }

    fn transport(&self) -> &dyn Transport {
        self.transport.as_ref()
    }

    fn login(&self) -> Result<String> {
        forge::required(
            &self.transport().json(&Request::get("user"))?,
            "/login",
            "tea api user",
        )
    }
}

impl ReviewService for Forgejo {
    fn find_review(&self, origin: &str, branch: &str) -> Result<Option<(String, u64)>> {
        find_pull(self.transport(), origin, branch)
    }

    fn fetch(&self, subject: ReviewSubject) -> Result<Fetched> {
        fetch(self.transport(), subject.repo, subject.number)
    }

    fn imported_id(&self, kind: RemoteKind, first: &str) -> String {
        match kind {
            RemoteKind::ReviewThread => format!("fg-t{first}"),
            RemoteKind::Review => format!("fg-r{first}"),
            _ => format!("fg-c{first}"),
        }
    }

    fn place(
        &self,
        repo: &Repo,
        review: &Review,
        Wanted {
            anchor,
            commit,
            line,
        }: Wanted,
    ) -> Option<Placement> {
        // The file and the line number on the new side, or on the old side for a removed line,
        // as the host takes them.
        let base = review.base_sha.as_deref()?;
        if !repo.is_ancestor(commit, &review.head_sha).ok()? {
            return None;
        }
        let merge_base = repo.merge_base(base, commit).ok()??;
        let diff = repo.diff_context(&merge_base, commit).ok()?;
        let found = anchors::diff_line(&diff, &anchor.path, anchor.side, line)?;
        let data = match found.new_line {
            Some(new) => json!({ "path": found.new_path, "new_position": new }),
            None => json!({ "path": found.old_path, "old_position": found.old_line? }),
        };
        Some(Placement::InDiff(Position {
            commit: commit.to_owned(),
            data,
        }))
    }

    fn inline(&self) -> Inline<'_> {
        Inline::ReviewBatch(self)
    }

    fn thread(&self, subject: ReviewSubject, text: &str) -> Request {
        self.note(subject, text)
    }

    fn note(&self, subject: ReviewSubject, text: &str) -> Request {
        Request::rest(
            Method::Post,
            comments_path(subject),
            Some(json!({ "body": text })),
        )
    }

    fn edit_note(&self, subject: ReviewSubject, id: &str, text: &str) -> Request {
        Request::rest(
            Method::Patch,
            comment_path(subject, id),
            Some(json!({ "body": text })),
        )
    }

    // Forgejo edits a comment on a diff line through the general route too. A review's own id
    // names the review, not a comment.
    fn edit_comment(
        &self,
        subject: ReviewSubject,
        kind: RemoteKind,
        comment: &RemoteComment,
        text: &str,
    ) -> Option<Request> {
        (kind != RemoteKind::Review).then(|| self.edit_note(subject, &comment.id, text))
    }

    // The general route answers 204 but keeps a diff comment. Only its review removes it.
    fn delete_comment(
        &self,
        subject: ReviewSubject,
        kind: RemoteKind,
        comment: &RemoteComment,
    ) -> Option<Request> {
        let path = match kind {
            RemoteKind::Review => return None,
            RemoteKind::ReviewThread => format!(
                "{}/{}/comments/{}",
                reviews_path(subject),
                forge::encode(comment.review_id.as_deref()?),
                forge::encode(&comment.id)
            ),
            RemoteKind::General | RemoteKind::Discussion => comment_path(subject, &comment.id),
        };
        Some(Request::rest(Method::Delete, path, None))
    }

    // `caps().reply` is `None`, so the workflow answers with a general comment instead.
    fn reply(&self, subject: ReviewSubject, _remote: &RemoteThread, text: &str) -> Request {
        self.note(subject, text)
    }

    // `caps().resolve` is false, so the workflow never sends a resolve; a read stands in for one
    // that is built anyway.
    fn resolve(&self, subject: ReviewSubject, _remote: &RemoteThread, _resolved: bool) -> Request {
        Request::get(base(subject))
    }

    fn approve(&self, subject: ReviewSubject, head: &str, comment: Option<&str>) -> Request {
        let mut body = json!({ "commit_id": head, "event": "APPROVED" });
        if let Some(comment) = comment {
            body["body"] = json!(comment);
        }
        Request::rest(Method::Post, reviews_path(subject), Some(body))
    }

    fn comment_created(
        &self,
        _review_url: Option<&str>,
        response: &Value,
    ) -> Option<(String, Option<String>)> {
        Some((
            forge::id(response, "/id")?,
            forge::text(response, "/html_url"),
        ))
    }

    fn draft_created(&self, _response: &Value) -> Option<String> {
        None
    }

    fn review_posted(
        &self,
        subject: ReviewSubject,
        response: &Value,
    ) -> Result<(String, Vec<Posted>)> {
        let review = forge::id(response, "/id")
            .ok_or_else(|| Error("Forgejo returned no review id".into()))?;
        let path = format!("{}/{review}/comments", reviews_path(subject));
        let posted = unpaged(self.transport(), path)?
            .iter()
            .filter_map(|item| {
                Some(Posted {
                    id: forge::id(item, "/id")?,
                    url: forge::text(item, "/html_url"),
                    body: item["body"].as_str()?.to_owned(),
                })
            })
            .collect();
        Ok((review, posted))
    }
}

impl BatchPoster for Forgejo {
    fn review_batch(
        &self,
        subject: ReviewSubject,
        commit: &str,
        summary: &str,
        comments: &[(Position, String)],
    ) -> Request {
        let comments: Vec<Value> = comments
            .iter()
            .map(|(position, body)| {
                let mut comment = position.data.clone();
                comment["body"] = json!(body);
                comment
            })
            .collect();
        Request::rest(
            Method::Post,
            reviews_path(subject),
            Some(json!({
                "commit_id": commit,
                "event": "COMMENT",
                "body": summary,
                "comments": comments,
            })),
        )
    }
}

/// Every item of a list the host does not page: the comment endpoints of an issue and of a
/// review take no `page` or `limit`, so a paged read would get the same items again and again.
fn unpaged(transport: &dyn Transport, path: String) -> Result<Vec<Value>> {
    match transport.json(&Request::get(path))? {
        Value::Array(items) => Ok(items),
        other => Err(Error(format!(
            "the host sent a list that is not an array: {other}"
        ))),
    }
}

/// The PRs of `repo` in `state` (`open`, `closed` or `all`), newest first.
pub fn pulls(
    transport: &dyn Transport,
    repo: &str,
    state: &str,
) -> std::result::Result<Vec<Value>, TransportError> {
    transport.items(
        &Request::get(format!("repos/{repo}/pulls?state={state}")),
        &CAPS.pagination,
    )
}

/// The PRs of `repo` whose head branch is `branch` and that `wanted` keeps: the open ones, and
/// every state only when none of them qualifies. agt does not send the host's `head` filter, so
/// each state is listed whole and matched on its head branch.
pub fn pulls_of_branch(
    transport: &dyn Transport,
    repo: &str,
    branch: &str,
    wanted: impl Fn(&Value) -> bool,
) -> std::result::Result<Vec<Value>, TransportError> {
    for state in ["open", "all"] {
        let mut found = pulls(transport, repo, state)?;
        found.retain(|pull| pull["head"]["ref"] == branch && wanted(pull));
        if !found.is_empty() {
            return Ok(found);
        }
    }
    Ok(Vec::new())
}

/// Finds the PR whose head branch is `branch` of `origin`, in `origin` or, for a fork, its
/// parent.
fn find_pull(forgejo: &dyn Transport, origin: &str, branch: &str) -> Result<Option<(String, u64)>> {
    let search = |repo: &str| -> Result<Option<u64>> {
        let from_origin = |pull: &Value| {
            forge::text(pull, "/head/repo/full_name")
                .is_some_and(|head| head.eq_ignore_ascii_case(origin))
        };
        let found = pulls_of_branch(forgejo, repo, branch, from_origin)?;
        Ok(found.first().and_then(|pull| pull["number"].as_u64()))
    };
    forge::in_origin_or_parent(origin, search, || {
        let repo = forgejo.json(&Request::get(format!("repos/{origin}")))?;
        Ok(forge::text(&repo, "/parent/full_name"))
    })
}

fn comment(node: &Value) -> Result<CommentData> {
    let what = "Forgejo comment";
    Ok(CommentData {
        id: forge::id(node, "/id").ok_or_else(|| Error("Forgejo comment has no id".into()))?,
        url: forge::text(node, "/html_url"),
        body: forge::text(node, "/body").unwrap_or_default(),
        created_at: clock::normalize(&forge::required(node, "/created_at", what)?)?,
        login: forge::text(node, "/user/login"),
        editor: None,
        review_id: forge::id(node, "/pull_request_review_id"),
    })
}

/// A submitted review with a body, as a thread of its own.
fn review_body(node: &Value) -> Result<Option<ThreadData>> {
    let body = forge::text(node, "/body").unwrap_or_default();
    let Some(submitted) = forge::text(node, "/submitted_at") else {
        return Ok(None);
    };
    if body.trim().is_empty() {
        return Ok(None);
    }
    let review = CommentData {
        id: forge::id(node, "/id").ok_or_else(|| Error("Forgejo review has no id".into()))?,
        url: forge::text(node, "/html_url"),
        body,
        created_at: clock::normalize(&submitted)?,
        login: forge::text(node, "/user/login"),
        editor: None,
        review_id: None,
    };
    Ok(Some(forge::general(
        RemoteKind::Review,
        review,
        forge::text(node, "/state"),
    )))
}

/// The in-diff comments as threads: comments on one line of one file are one thread, the first
/// as its root. The host has no thread object, only a resolver on a resolved conversation.
fn code_threads(nodes: &[Value]) -> Result<Vec<ThreadData>> {
    let mut threads: Vec<ThreadData> = Vec::new();
    let mut at: HashMap<(String, u32, u32), usize> = HashMap::new();
    for node in nodes {
        let path = forge::text(node, "/path").unwrap_or_default();
        let line = forge::number(node, "/position").filter(|line| *line > 0);
        let old_line = forge::number(node, "/original_position").filter(|line| *line > 0);
        let data = comment(node)?;
        let resolved = !node["resolver"].is_null();
        let key = (
            path.clone(),
            line.unwrap_or_default(),
            old_line.unwrap_or_default(),
        );
        if let Some(index) = at.get(&key) {
            threads[*index].resolved |= resolved;
            threads[*index].comments.push(data);
            continue;
        }
        let side = match (line, old_line) {
            (Some(_), _) => Some(Side::New),
            (None, Some(_)) => Some(Side::Old),
            (None, None) => None,
        };
        at.insert(key, threads.len());
        threads.push(ThreadData {
            info: RemoteThread {
                kind: RemoteKind::ReviewThread,
                id: data.id.clone(),
                url: data.url.clone(),
                resolvable: true,
                outdated: None,
                path: Some(path),
                old_path: None,
                line,
                old_line,
                original_line: line,
                start_line: None,
                original_start_line: None,
                side,
                commit: forge::text(node, "/commit_id"),
                review_state: None,
            },
            resolved,
            comments: vec![data],
            position_head: None,
        });
    }
    Ok(threads)
}

fn fetch(forgejo: &dyn Transport, repo: &str, number: u64) -> Result<Fetched> {
    let path = format!("repos/{repo}/pulls/{number}");
    let pull = forgejo.json(&Request::get(path.clone()))?;
    let normalized = |pointer: &str| {
        forge::text(&pull, pointer)
            .map(|time| clock::normalize(&time))
            .transpose()
    };
    let pr = PrInfo {
        repo: forge::text(&pull, "/base/repo/full_name").unwrap_or_else(|| repo.to_owned()),
        url: forge::required(&pull, "/html_url", "Forgejo PR")?,
        state: state_of(&pull).unwrap_or(ReviewState::Open),
        closed_at: match normalized("/merged_at")? {
            Some(merged) => Some(merged),
            None => normalized("/closed_at")?,
        },
        head_branch: forge::required(&pull, "/head/ref", "Forgejo PR")?,
        head_sha: forge::required(&pull, "/head/sha", "Forgejo PR")?,
        base_sha: forge::text(&pull, "/base/sha"),
        start_sha: None,
        author: forge::text(&pull, "/user/login"),
        created_at: normalized("/created_at")?,
    };
    let reviews = forgejo.items(&Request::get(format!("{path}/reviews")), &CAPS.pagination)?;
    let mut inline = Vec::new();
    let mut threads = Vec::new();
    for review in reviews.iter().filter(|review| review["state"] != "PENDING") {
        threads.extend(review_body(review)?);
        if review["comments_count"].as_u64() == Some(0) {
            continue;
        }
        let id =
            forge::id(review, "/id").ok_or_else(|| Error("Forgejo review has no id".into()))?;
        inline.extend(unpaged(forgejo, format!("{path}/reviews/{id}/comments"))?);
    }
    let mut all = code_threads(&inline)?;
    all.append(&mut threads);
    let notes = unpaged(forgejo, format!("repos/{repo}/issues/{number}/comments"))?;
    for node in &notes {
        all.push(forge::general(RemoteKind::General, comment(node)?, None));
    }
    Ok(Fetched {
        pr,
        threads: all,
        drafts: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::contract::{self, Replay};
    use crate::forge::transport::tea_args;

    const VARS: [(&str, &str); 6] = [
        ("HEAD", "1111111111111111111111111111111111111111"),
        ("BASE", "2222222222222222222222222222222222222222"),
        ("HOST", "codeberg.org"),
        ("REPO", "me/proj"),
        ("NUMBER", "5"),
        ("AUTHOR", "me"),
    ];
    const PULL: &str = "repos/me/proj/pulls/5";

    fn subject() -> ReviewSubject<'static> {
        ReviewSubject {
            repo: "me/proj",
            number: 5,
        }
    }

    fn tea(request: &Request) -> Vec<String> {
        tea_args(request, "codeberg")
    }

    fn forgejo() -> Forgejo {
        let get = ["api", "--login", "codeberg", "-i", "-X", "GET"];
        let replay = [
            (PULL.to_owned(), "pr"),
            (format!("{PULL}/reviews?limit=50&page=1"), "reviews"),
            (format!("{PULL}/reviews/5004/comments"), "review-comments"),
            ("repos/me/proj/issues/5/comments".to_owned(), "comments"),
        ]
        .into_iter()
        .fold(Replay::new(tea), |replay, (path, name)| {
            let fixture = contract::fixture(&format!("forgejo/{name}.json"), &VARS);
            replay.answer(&[&get[..], &[path.as_str()]].concat(), fixture)
        });
        Forgejo::new(Box::new(replay))
    }

    #[test]
    fn capabilities_match_the_contract() {
        contract::golden(
            "forgejo/capabilities.json",
            &contract::capabilities(&forgejo()),
        );
    }

    #[test]
    fn fetch_reads_the_contract_fixtures() {
        let fetched = forgejo().fetch(subject()).unwrap();
        contract::golden("forgejo/fetched.golden.json", &contract::dump(&fetched));
    }

    #[test]
    fn a_comment_in_a_review_body_is_neither_edited_nor_deleted_alone() {
        let forgejo = forgejo();
        assert!(
            forgejo
                .edit_comment(
                    subject(),
                    RemoteKind::Review,
                    &contract::posted("8000", None),
                    "text"
                )
                .is_none()
        );
        assert!(
            forgejo
                .delete_comment(
                    subject(),
                    RemoteKind::Review,
                    &contract::posted("8000", None)
                )
                .is_none()
        );
    }

    #[test]
    fn a_diff_comment_with_no_known_review_is_not_deleted() {
        let forgejo = forgejo();
        let unknown = contract::posted("8000", None);
        assert!(
            forgejo
                .delete_comment(subject(), RemoteKind::ReviewThread, &unknown)
                .is_none()
        );
        assert!(
            forgejo
                .delete_comment(subject(), RemoteKind::General, &unknown)
                .is_some()
        );
    }

    #[test]
    fn requests_render_to_the_canonical_argv() {
        let forgejo = forgejo();
        let thread = RemoteThread::new(RemoteKind::General, "8000".to_owned(), false);
        let position = Position {
            commit: "c".to_owned(),
            data: json!({ "path": "a.txt", "new_position": 3 }),
        };
        let all = [
            (
                "review_batch",
                forgejo.review_batch(subject(), "c", "summary", &[(position, "text".to_owned())]),
            ),
            ("thread", forgejo.thread(subject(), "text")),
            ("note", forgejo.note(subject(), "text")),
            ("edit_note", forgejo.edit_note(subject(), "8000", "text")),
            (
                "edit_comment",
                forgejo
                    .edit_comment(
                        subject(),
                        RemoteKind::ReviewThread,
                        &contract::posted("8000", Some("700")),
                        "text",
                    )
                    .unwrap(),
            ),
            (
                "delete_comment",
                forgejo
                    .delete_comment(
                        subject(),
                        RemoteKind::ReviewThread,
                        &contract::posted("8000", Some("700")),
                    )
                    .unwrap(),
            ),
            (
                "delete_general_comment",
                forgejo
                    .delete_comment(
                        subject(),
                        RemoteKind::General,
                        &contract::posted("8000", None),
                    )
                    .unwrap(),
            ),
            ("reply", forgejo.reply(subject(), &thread, "text")),
            ("approve", forgejo.approve(subject(), "c", Some("ok"))),
        ];
        contract::golden(
            "forgejo/requests.golden.json",
            &contract::requests(tea, &all),
        );
    }

    #[test]
    fn a_merged_pull_request_is_merged_though_the_host_calls_it_closed() {
        let node = json!({
            "number": 7, "html_url": "https://h/o/r/pulls/7", "title": "t", "state": "closed",
            "merged": true, "merged_at": "2026-10-02T00:00:00Z", "closed_at": null,
            "base": {"ref": "main", "repo": {"full_name": "o/r"}},
            "head": {"ref": "topic", "sha": "abc", "repo": {"full_name": "O/R"}},
            "user": {"login": "me"}, "draft": false
        });
        let pull = pull_node(&node);
        assert_eq!(pull.state, Some(ReviewState::Merged));
        assert!(pull.merged);
        assert_eq!(pull.closed_at.as_deref(), Some("2026-10-02T00:00:00Z"));
        assert_eq!(pull.same_repo, Some(true));
        assert_eq!(pull.head_branch, "topic");
        assert!(!pull.draft);
        for title in ["WIP: t", "[wip] t"] {
            assert!(
                pull_node(&json!({"title": title, "draft": false})).draft,
                "{title}"
            );
        }
        let closed = json!({"state": "closed", "merged": false, "head": {"repo": null}});
        assert_eq!(pull_node(&closed).state, Some(ReviewState::Closed));
        assert_eq!(pull_node(&closed).same_repo, None);
        assert_eq!(state_of(&json!({"state": "open"})), Some(ReviewState::Open));
        assert_eq!(state_of(&json!({"state": "weird"})), None);
    }

    fn code_comment(id: u64, path: &str, position: u64, resolved: bool) -> Value {
        json!({
            "id": id, "body": format!("c{id}"), "path": path, "position": position,
            "original_position": 0, "commit_id": "abc", "pull_request_review_id": 5,
            "html_url": format!("https://h/o/r/pulls/7#issuecomment-{id}"),
            "created_at": "2026-10-01T00:00:00Z", "user": {"login": "rev"},
            "resolver": if resolved { json!({"login": "rev"}) } else { Value::Null },
        })
    }

    #[test]
    fn comments_on_one_line_are_one_thread_and_a_resolver_resolves_it() {
        let threads = code_threads(&[
            code_comment(1, "a.rs", 4, false),
            code_comment(2, "b.rs", 4, false),
            code_comment(3, "a.rs", 4, true),
        ])
        .unwrap();
        assert_eq!(threads.len(), 2);
        let first = &threads[0];
        assert_eq!(first.info.id, "1");
        assert_eq!(first.info.line, Some(4));
        assert!(first.info.side == Some(Side::New));
        assert_eq!(first.comments.len(), 2);
        assert!(first.resolved);
        assert!(!threads[1].resolved);
    }

    #[test]
    fn a_comment_on_a_removed_line_is_on_the_old_side() {
        let mut node = code_comment(1, "a.rs", 0, false);
        node["original_position"] = json!(9);
        let thread = &code_threads(&[node]).unwrap()[0];
        assert_eq!(thread.info.line, None);
        assert_eq!(thread.info.old_line, Some(9));
        assert!(thread.info.side == Some(Side::Old));
    }

    #[test]
    fn a_review_batch_names_the_commit_and_each_comment() {
        let forgejo = Forgejo::new(Box::new(Unused));
        let subject = ReviewSubject {
            repo: "o/r",
            number: 7,
        };
        let comments = [(
            Position {
                commit: "abc".to_owned(),
                data: json!({"path": "a.rs", "new_position": 4}),
            },
            "text".to_owned(),
        )];
        let request = forgejo.review_batch(subject, "abc", "sum", &comments);
        assert_eq!(
            request.endpoint,
            crate::forge::service::Endpoint::Rest("repos/o/r/pulls/7/reviews".into())
        );
        assert_eq!(
            request.body.unwrap(),
            json!({"commit_id": "abc", "event": "COMMENT", "body": "sum",
                "comments": [{"path": "a.rs", "new_position": 4, "body": "text"}]})
        );
    }

    struct Unused;

    impl Transport for Unused {
        fn send(
            &self,
            _request: &Request,
        ) -> std::result::Result<
            crate::forge::transport::Response,
            crate::forge::transport::TransportError,
        > {
            unreachable!("a builder sends nothing")
        }

        fn render(&self, _request: &Request) -> Vec<String> {
            Vec::new()
        }
    }
}
