mod issues;

use serde_json::Value;

use crate::core::clock;
use crate::core::error::{Error, Result, fail};
use crate::forge::anchors;
use crate::forge::service::{
    ApproveCap, Capabilities, Inline, Kind, Method, Pagination, Placement, Position, Posted,
    ReplyCap, Request, ReviewService, ReviewSubject, Service, Var,
};
use crate::forge::transport::{Cli, CliTransport, Transport};
use crate::forge::{self, CommentData, Fetched, PrInfo, ThreadData};
use crate::git::Repo;
use crate::store::model::{Anchor, Forge, RemoteKind, RemoteThread, Review, ReviewState, Side};
use serde_json::json;

const COMMENT_FIELDS: &str = "databaseId url body createdAt author{login} editor{login} originalCommit{oid} pullRequestReview{databaseId}";

/// The GraphQL field a PR is read through; an issue would use `issue`, with the same comment
/// connections.
const PULL: &str = "pullRequest";

fn subject_query(root: &str, name: &str, selection: &str) -> String {
    format!(
        "query {name}($owner:String!,$name:String!,$number:Int!,$cursor:String){{repository(owner:$owner,name:$name){{{root}(number:$number){{{selection}}}}}}}"
    )
}

const REPLY_MUTATION: &str = "mutation AddReply($threadId:ID!,$body:String!){addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$threadId,body:$body}){comment{databaseId url}}}";
const RESOLVE_MUTATION: &str = "mutation ResolveThread($threadId:ID!){resolveReviewThread(input:{threadId:$threadId}){thread{isResolved}}}";
const UNRESOLVE_MUTATION: &str = "mutation UnresolveThread($threadId:ID!){unresolveReviewThread(input:{threadId:$threadId}){thread{isResolved}}}";

pub const KIND: Kind = Kind {
    forge: Forge::Github,
    key_prefix: "gh",
    url_marker: "/pull/",
    well_known: |host| host == "github.com",
    configured: |_| false,
    probe: |host| {
        Cli::gh(host)
            .text(&["auth", "status", "--hostname", host])
            .is_some()
    },
    connect: |host| Box::new(Github::new(Box::new(CliTransport::gh(host)))),
    issue_caps: &issues::ISSUE_CAPS,
    connect_issue: |host| Box::new(Github::new(Box::new(CliTransport::gh(host)))),
};

const CAPS: Capabilities = Capabilities {
    pagination: Pagination::GraphqlCursor,
    inline: Inline::ReviewBatch,
    ranges: true,
    reply: ReplyCap::ReviewThreads,
    resolve: true,
    unresolve: true,
    general_kind: RemoteKind::General,
    general_resolvable: false,
    approve: ApproveCap::Review {
        self_allowed: false,
    },
    edits_attributed: true,
    notes_become_discussions: false,
};

pub struct Github {
    transport: Box<dyn Transport>,
}

impl Github {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        Self { transport }
    }
}

fn base(subject: ReviewSubject) -> String {
    format!("repos/{}", subject.repo)
}

fn comments_path(subject: ReviewSubject) -> String {
    format!("{}/issues/{}/comments", base(subject), subject.number)
}

fn reviews_path(subject: ReviewSubject) -> String {
    format!("{}/pulls/{}/reviews", base(subject), subject.number)
}

impl Service for Github {
    fn forge(&self) -> Forge {
        Forge::Github
    }

    fn name(&self) -> &'static str {
        "GitHub"
    }

    fn caps(&self) -> &Capabilities {
        &CAPS
    }

    fn transport(&self) -> &dyn Transport {
        self.transport.as_ref()
    }

    fn login(&self) -> Result<String> {
        login(self.transport())
    }
}

impl ReviewService for Github {
    fn find_review(&self, origin: &str, branch: &str) -> Result<Option<(String, u64)>> {
        find_pr(self.transport(), origin, branch)
    }

    fn fetch(&self, subject: ReviewSubject) -> Result<Fetched> {
        fetch(self.transport(), subject.repo, subject.number)
    }

    fn imported_id(&self, kind: RemoteKind, first: &str) -> String {
        match kind {
            RemoteKind::ReviewThread => format!("gh-t{first}"),
            RemoteKind::Review => format!("gh-r{first}"),
            _ => format!("gh-c{first}"),
        }
    }

    fn place(
        &self,
        repo: &Repo,
        review: &Review,
        anchor: &Anchor,
        commit: &str,
        line: u32,
    ) -> Option<Placement> {
        place(repo, review, anchor, commit, line)
    }

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

    fn reply(&self, _subject: ReviewSubject, remote: &RemoteThread, text: &str) -> Request {
        Request::graphql(vec![
            Var::text("threadId", remote.id.clone()),
            Var::text("body", text),
            Var::text("query", REPLY_MUTATION),
        ])
    }

    fn resolve(&self, _subject: ReviewSubject, remote: &RemoteThread, resolved: bool) -> Request {
        let mutation = if resolved {
            RESOLVE_MUTATION
        } else {
            UNRESOLVE_MUTATION
        };
        Request::graphql(vec![
            Var::text("threadId", remote.id.clone()),
            Var::text("query", mutation),
        ])
    }

    fn approve(&self, subject: ReviewSubject, head: &str, comment: Option<&str>) -> Request {
        let mut body = json!({ "commit_id": head, "event": "APPROVE" });
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
        const REPLY: &str = "/data/addPullRequestReviewThreadReply/comment";
        match forge::id(response, &format!("{REPLY}/databaseId")) {
            Some(id) => Some((id, forge::text(response, &format!("{REPLY}/url")))),
            None => Some((
                forge::id(response, "/id")?,
                forge::text(response, "/html_url"),
            )),
        }
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
            .ok_or_else(|| Error("GitHub returned no review id".into()))?;
        let path = format!("{}/{review}/comments", reviews_path(subject));
        let listed = self.transport().json(&Request::get(path))?;
        let posted = listed
            .as_array()
            .into_iter()
            .flatten()
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

/// The place of a comment in the PR diff: both ends on one hunk for a range, as GitHub
/// requires, and the commit it reviewed.
fn place(
    repo: &Repo,
    review: &Review,
    anchor: &Anchor,
    commit: &str,
    line: u32,
) -> Option<Placement> {
    let base = review.base_sha.as_deref()?;
    if !repo.is_ancestor(commit, &review.head_sha).ok()? {
        return None;
    }
    let merge_base = repo.merge_base(base, commit).ok()??;
    let diff = repo.diff_context(&merge_base, commit).ok()?;
    let found = anchors::diff_line(&diff, &anchor.path, anchor.side, line)?;
    let (line, side) = match found.new_line {
        Some(new) => (new, "RIGHT"),
        None => (found.old_line?, "LEFT"),
    };
    let start_line = match anchor.start_line.filter(|start| *start < line) {
        Some(start) => {
            let first = anchors::diff_line(&diff, &anchor.path, Side::New, start)?;
            if side != "RIGHT" || first.hunk != found.hunk {
                return None;
            }
            first.new_line
        }
        None => None,
    };
    let path = if side == "LEFT" {
        found.old_path
    } else {
        found.new_path
    };
    let mut data = json!({ "path": path, "line": line, "side": side });
    if let Some(start) = start_line {
        data["start_line"] = json!(start);
        data["start_side"] = json!(side);
    }
    Some(Placement::InDiff(Position {
        commit: commit.to_owned(),
        data,
    }))
}

fn login(gh: &dyn Transport) -> Result<String> {
    forge::required(&gh.json(&Request::get("user"))?, "/login", "gh api user")
}

/// Finds the PR whose head is `<origin owner>:<branch>`, in `origin` or, for a fork, its parent.
fn find_pr(gh: &dyn Transport, origin: &str, branch: &str) -> Result<Option<(String, u64)>> {
    let owner = origin.split('/').next().unwrap_or_default();
    let head = forge::encode(&format!("{owner}:{branch}"));
    let search = |repo: &str| -> Result<Option<u64>> {
        let path = format!("repos/{repo}/pulls?head={head}&state=all&per_page=20");
        let pulls = gh.json(&Request::get(path))?;
        let pulls = pulls.as_array().cloned().unwrap_or_default();
        let open = pulls
            .iter()
            .find(|pull| pull["state"] == "open")
            .or(pulls.first());
        Ok(open.and_then(|pull| pull["number"].as_u64()))
    };
    forge::in_origin_or_parent(origin, search, || {
        let repo = gh.json(&Request::get(format!("repos/{origin}")))?;
        Ok(forge::text(&repo, "/parent/full_name"))
    })
}

struct Pager<'a> {
    gh: &'a dyn Transport,
    /// The GraphQL field of the subject, such as [`PULL`].
    root: &'static str,
    owner: String,
    name: String,
    number: String,
}

impl Pager<'_> {
    /// Fetches every node of one connection; returns the first response and all nodes.
    fn pages(&self, query: &str, connection: &str) -> Result<(Value, Vec<Value>)> {
        let mut nodes = Vec::new();
        let mut first = None;
        let mut cursor: Option<String> = None;
        loop {
            let mut vars = vec![
                Var::text("owner", self.owner.clone()),
                Var::text("name", self.name.clone()),
                Var::typed("number", self.number.clone()),
                Var::text("query", query),
            ];
            if let Some(cursor) = &cursor {
                vars.push(Var::text("cursor", cursor.clone()));
            }
            let response = self.gh.json(&Request::graphql(vars))?;
            let pointer = format!("/data/repository/{}/{connection}", self.root);
            let page = response
                .pointer(&pointer)
                .ok_or_else(|| Error(format!("GitHub returned no {connection}")))?;
            nodes.extend(page["nodes"].as_array().cloned().unwrap_or_default());
            cursor = next_cursor(page);
            if first.is_none() {
                first = Some(response.clone());
            }
            if cursor.is_none() {
                break;
            }
        }
        Ok((first.unwrap_or_default(), nodes))
    }

    fn thread_comments(&self, thread: &str, cursor: String) -> Result<Vec<Value>> {
        let query = format!(
            "query ThreadComments($id:ID!,$cursor:String){{node(id:$id){{... on PullRequestReviewThread{{comments(first:100,after:$cursor){{pageInfo{{hasNextPage endCursor}} nodes{{{COMMENT_FIELDS}}}}}}}}}}}"
        );
        let mut nodes = Vec::new();
        let mut cursor = Some(cursor);
        while let Some(after) = cursor {
            let response = self.gh.json(&Request::graphql(vec![
                Var::text("id", thread),
                Var::text("query", query.clone()),
                Var::text("cursor", after),
            ]))?;
            let page = response
                .pointer("/data/node/comments")
                .ok_or_else(|| Error(format!("GitHub returned no comments for {thread}")))?;
            nodes.extend(page["nodes"].as_array().cloned().unwrap_or_default());
            cursor = next_cursor(page);
        }
        Ok(nodes)
    }
}

fn next_cursor(page: &Value) -> Option<String> {
    if page.pointer("/pageInfo/hasNextPage") == Some(&Value::Bool(true)) {
        forge::text(page, "/pageInfo/endCursor")
    } else {
        None
    }
}

fn comment(node: &Value) -> Result<CommentData> {
    let created = forge::required(node, "/createdAt", "GitHub comment")?;
    Ok(CommentData {
        id: forge::id(node, "/databaseId")
            .ok_or_else(|| Error("GitHub comment has no databaseId".into()))?,
        url: forge::text(node, "/url"),
        body: forge::text(node, "/body").unwrap_or_default(),
        created_at: clock::normalize(&created)?,
        login: forge::text(node, "/author/login"),
        editor: forge::text(node, "/editor/login"),
        review_id: forge::id(node, "/pullRequestReview/databaseId"),
    })
}

fn state(text: &str) -> ReviewState {
    match text {
        "MERGED" => ReviewState::Merged,
        "CLOSED" => ReviewState::Closed,
        _ => ReviewState::Open,
    }
}

fn review_thread(pager: &Pager, node: &Value) -> Result<ThreadData> {
    let id = forge::required(node, "/id", "GitHub review thread")?;
    let mut nodes = node
        .pointer("/comments/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(cursor) = next_cursor(&node["comments"]) {
        nodes.extend(pager.thread_comments(&id, cursor)?);
    }
    let comments = nodes.iter().map(comment).collect::<Result<Vec<_>>>()?;
    let side = match node["diffSide"].as_str() {
        Some("LEFT") => Some(Side::Old),
        Some("RIGHT") => Some(Side::New),
        _ => None,
    };
    let line = forge::number(node, "/line");
    let file_level = node["subjectType"] == "FILE";
    let commit = nodes
        .first()
        .and_then(|first| forge::text(first, "/originalCommit/oid"));
    Ok(ThreadData {
        info: RemoteThread {
            kind: RemoteKind::ReviewThread,
            id,
            url: comments.first().and_then(|first| first.url.clone()),
            resolvable: true,
            outdated: node["isOutdated"].as_bool(),
            path: forge::text(node, "/path"),
            old_path: None,
            line: if side == Some(Side::Old) || file_level {
                None
            } else {
                line
            },
            old_line: if side == Some(Side::Old) { line } else { None },
            original_line: forge::number(node, "/originalLine"),
            start_line: forge::number(node, "/startLine"),
            original_start_line: forge::number(node, "/originalStartLine"),
            side,
            commit,
            review_state: None,
        },
        resolved: node["isResolved"].as_bool().unwrap_or(false),
        comments,
        position_head: None,
    })
}

fn general(kind: RemoteKind, comment: CommentData, review_state: Option<String>) -> ThreadData {
    ThreadData {
        info: RemoteThread {
            url: comment.url.clone(),
            review_state,
            ..RemoteThread::new(kind, comment.id.clone(), false)
        },
        resolved: false,
        comments: vec![comment],
        position_head: None,
    }
}

fn fetch(gh: &dyn Transport, repo: &str, number: u64) -> Result<Fetched> {
    let Some((owner, name)) = repo.split_once('/') else {
        return fail(format!("GitHub repository must be owner/name: {repo}"));
    };
    let pager = Pager {
        gh,
        root: PULL,
        owner: owner.to_owned(),
        name: name.to_owned(),
        number: number.to_string(),
    };
    let threads_query = subject_query(
        PULL,
        "PullRequest",
        &format!(
            "number url state closedAt author{{login}} headRefName headRefOid baseRefOid baseRepository{{nameWithOwner}} reviewThreads(first:50,after:$cursor){{pageInfo{{hasNextPage endCursor}} nodes{{id isResolved isOutdated path line originalLine startLine originalStartLine diffSide subjectType comments(first:50){{pageInfo{{hasNextPage endCursor}} nodes{{{COMMENT_FIELDS}}}}}}}}}"
        ),
    );
    let (first, thread_nodes) = pager.pages(&threads_query, "reviewThreads")?;
    let pull = first
        .pointer(&format!("/data/repository/{}", pager.root))
        .ok_or_else(|| Error(format!("GitHub has no PR {repo}#{number}")))?;
    let pr = PrInfo {
        repo: forge::text(pull, "/baseRepository/nameWithOwner").unwrap_or_else(|| repo.to_owned()),
        url: forge::required(pull, "/url", "GitHub PR")?,
        state: state(pull["state"].as_str().unwrap_or_default()),
        closed_at: forge::text(pull, "/closedAt")
            .map(|time| clock::normalize(&time))
            .transpose()?,
        head_branch: forge::required(pull, "/headRefName", "GitHub PR")?,
        head_sha: forge::required(pull, "/headRefOid", "GitHub PR")?,
        base_sha: forge::text(pull, "/baseRefOid"),
        start_sha: None,
        author: forge::text(pull, "/author/login"),
    };
    let mut threads = thread_nodes
        .iter()
        .map(|node| review_thread(&pager, node))
        .collect::<Result<Vec<_>>>()?;

    let reviews_query = subject_query(
        pager.root,
        "PullReviews",
        "reviews(first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{databaseId url body state submittedAt author{login} editor{login}}}",
    );
    for node in pager.pages(&reviews_query, "reviews")?.1 {
        let body = forge::text(&node, "/body").unwrap_or_default();
        let Some(submitted) = forge::text(&node, "/submittedAt") else {
            continue;
        };
        if body.trim().is_empty() {
            continue;
        }
        let review = CommentData {
            id: forge::id(&node, "/databaseId")
                .ok_or_else(|| Error("GitHub review has no databaseId".into()))?,
            url: forge::text(&node, "/url"),
            body,
            created_at: clock::normalize(&submitted)?,
            login: forge::text(&node, "/author/login"),
            editor: forge::text(&node, "/editor/login"),
            review_id: None,
        };
        threads.push(general(
            RemoteKind::Review,
            review,
            forge::text(&node, "/state"),
        ));
    }

    let comments_query = subject_query(
        pager.root,
        "PullComments",
        "comments(first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{databaseId url body createdAt author{login} editor{login}}}",
    );
    for node in pager.pages(&comments_query, "comments")?.1 {
        threads.push(general(RemoteKind::General, comment(&node)?, None));
    }
    Ok(Fetched {
        pr,
        threads,
        drafts: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::forge::contract::{self, Replay};
    use crate::forge::transport::gh_args;

    const VARS: [(&str, &str); 5] = [
        ("RUN", "rv-test"),
        ("HEAD", "1111111111111111111111111111111111111111"),
        ("BASE", "2222222222222222222222222222222222222222"),
        ("REPO", "me/repo"),
        ("NUMBER", "42"),
    ];

    fn query(name: &str, extra: &[&str]) -> Vec<String> {
        let mut args: Vec<String> = [
            "api",
            "graphql",
            "-f",
            "owner=me",
            "-f",
            "name=repo",
            "-F",
            "number=42",
            "-f",
        ]
        .map(str::to_owned)
        .to_vec();
        args.push(format!("query={name}"));
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        args
    }

    fn answer(replay: Replay, args: Vec<String>, fixture: &str) -> Replay {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        replay.answer(&args, contract::fixture(fixture, &VARS))
    }

    fn github() -> Github {
        let replay = Replay::new(gh_args);
        let replay = answer(replay, query("PullRequest", &[]), "github/threads-1.json");
        let replay = answer(
            replay,
            query("PullRequest", &["-f", "cursor=t-2"]),
            "github/threads-2.json",
        );
        let replay = replay.answer(
            &[
                "api",
                "graphql",
                "-f",
                "id=PRRT_a",
                "-f",
                "query=ThreadComments",
                "-f",
                "cursor=c-a1",
            ],
            contract::fixture("github/thread-comments.json", &VARS),
        );
        let replay = answer(replay, query("PullReviews", &[]), "github/reviews.json");
        let replay = answer(replay, query("PullComments", &[]), "github/comments-1.json");
        let replay = answer(
            replay,
            query("PullComments", &["-f", "cursor=ic-2"]),
            "github/comments-2.json",
        );
        Github::new(Box::new(replay))
    }

    fn subject() -> ReviewSubject<'static> {
        ReviewSubject {
            repo: "me/repo",
            number: 42,
        }
    }

    #[test]
    fn a_service_without_native_replies_replies_to_nothing_natively() {
        let none = Capabilities {
            reply: ReplyCap::None,
            ..CAPS
        };
        for kind in [
            RemoteKind::ReviewThread,
            RemoteKind::Review,
            RemoteKind::General,
            RemoteKind::Discussion,
        ] {
            assert!(!none.replies_natively(kind));
        }
        assert!(CAPS.replies_natively(RemoteKind::ReviewThread));
        assert!(!CAPS.replies_natively(RemoteKind::General));
    }

    #[test]
    fn capabilities_match_the_contract() {
        contract::golden("github/capabilities.json", &contract::capabilities(&CAPS));
    }

    /// Two pages of threads, an inner page of comments, reviews (pending and empty ones
    /// dropped) and two pages of general comments, as the contract fixtures hold them.
    #[test]
    fn fetch_reads_the_contract_fixtures() {
        let fetched = github().fetch(subject()).unwrap();
        contract::golden("github/fetched.golden.json", &contract::dump(&fetched));
    }

    #[test]
    fn requests_render_to_the_canonical_argv() {
        let github = github();
        let thread = RemoteThread::new(RemoteKind::ReviewThread, "PRRT_f1".to_owned(), true);
        let position = Position {
            commit: "c".to_owned(),
            data: json!({ "path": "a.txt", "line": 3, "side": "RIGHT" }),
        };
        let all = [
            (
                "review_batch",
                github.review_batch(subject(), "c", "summary", &[(position, "text".to_owned())]),
            ),
            ("thread", github.thread(subject(), "text")),
            ("note", github.note(subject(), "text")),
            ("reply", github.reply(subject(), &thread, "text")),
            ("resolve", github.resolve(subject(), &thread, true)),
            ("unresolve", github.resolve(subject(), &thread, false)),
            ("approve", github.approve(subject(), "c", Some("ok"))),
        ];
        contract::golden(
            "github/requests.golden.json",
            &contract::requests(gh_args, &all),
        );
    }

    #[test]
    fn responses_to_writes_name_the_comment_they_created() {
        let github = github();
        let created = |response: Value| github.comment_created(None, &response);
        assert_eq!(
            created(json!({ "id": 604, "html_url": "https://h/pull/42#c604" })),
            Some(("604".to_owned(), Some("https://h/pull/42#c604".to_owned())))
        );
        assert_eq!(
            created(
                json!({ "data": { "addPullRequestReviewThreadReply": { "comment": {
                "databaseId": 801,
                "url": "https://h/pull/42#discussion_r801",
            } } } })
            ),
            Some((
                "801".to_owned(),
                Some("https://h/pull/42#discussion_r801".to_owned())
            ))
        );
        assert_eq!(created(json!({})), None);
        assert_eq!(github.draft_created(&json!({ "id": 7 })), None);
    }
}
