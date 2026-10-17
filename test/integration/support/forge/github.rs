//! The fake GitHub: GraphQL pull request, review and comment queries, REST writes, and the
//! `gh api` argument lists the CLI transport renders for them.

use serde_json::{Value, json};

use super::super::scenario::{Setup, args, ids, marked, strs, thread};
use super::super::{Env, fixture};
use super::{Call, Comment, Expect, FakeService, Item, Remote, TIME};

pub const REVIEWS: &str = "repos/me/repo/pulls/42/reviews";
pub const COMMENTS: &str = "repos/me/repo/issues/42/comments";
pub const ORIGIN: &str = "git@github.com:me/repo.git";
const URL: &str = "https://github.com/me/repo/pull/42";

pub struct Github {
    expect: Expect,
}

impl Github {
    pub fn new() -> Self {
        Self {
            expect: Expect::load("github"),
        }
    }
}

pub fn post(path: &str) -> Vec<String> {
    args(&["api", "--method", "POST", path, "--input", "-"])
}

/// A GraphQL write on a review thread; `body` is whether it sends text.
pub fn mutation(thread: &str, query: &str, body: bool) -> Vec<String> {
    let mut list = args(&["api", "graphql", "-f", &format!("threadId={thread}")]);
    if body {
        list.extend(args(&["-f", "body="]));
    }
    list.extend(args(&["-f", &format!("query={query}")]));
    list
}

/// The three GraphQL reads of a sync of PR `number` of `owner/name`.
pub fn queries(owner: &str, name: &str, number: &str) -> Vec<Vec<String>> {
    ["PullRequest", "PullReviews", "PullComments"]
        .iter()
        .map(|query| graphql(owner, name, number, query))
        .collect()
}

pub fn graphql(owner: &str, name: &str, number: &str, query: &str) -> Vec<String> {
    [
        "api",
        "graphql",
        "-f",
        &format!("owner={owner}"),
        "-f",
        &format!("name={name}"),
        "-F",
        &format!("number={number}"),
        "-f",
        &format!("query={query}"),
    ]
    .map(str::to_owned)
    .to_vec()
}

pub fn with_cursor(mut list: Vec<String>, cursor: &str) -> Vec<String> {
    list.extend(["-f".to_owned(), format!("cursor={cursor}")]);
    list
}

pub fn comment(id: u64, login: &str, body: &str) -> Value {
    json!({
        "databaseId": id,
        "url": format!("{URL}#c{id}"),
        "body": body,
        "createdAt": TIME,
        "author": { "login": login },
        "editor": null,
    })
}

pub fn review_thread(
    id: &str,
    resolved: bool,
    line: u32,
    comments: &[Comment],
    head: &str,
) -> Value {
    let nodes: Vec<Value> = comments
        .iter()
        .map(|c| {
            let mut node = comment(c.id, &c.login, &c.body);
            node["originalCommit"] = json!({ "oid": head });
            node["pullRequestReview"] = json!({ "databaseId": 700 });
            node
        })
        .collect();
    json!({
        "id": id,
        "isResolved": resolved,
        "isOutdated": false,
        "path": "a.txt",
        "line": line,
        "originalLine": line,
        "startLine": null,
        "originalStartLine": null,
        "diffSide": "RIGHT",
        "subjectType": "LINE",
        "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": nodes },
    })
}

/// A submitted review by `login` with `state` and `body`.
pub fn review(id: u64, login: &str, state: &str, body: &str) -> Value {
    json!({
        "databaseId": id,
        "url": format!("{URL}#pullrequestreview-{id}"),
        "body": body,
        "state": state,
        "submittedAt": TIME,
        "author": { "login": login },
        "editor": null,
    })
}

/// The review this store posted for `head` and read back.
pub fn own_review(head: &str) -> Value {
    review(
        700,
        "me",
        "COMMENTED",
        &marked("Review findings.", &format!("review/{head}")),
    )
}

/// The three PR queries' one response.
fn render(setup: &Setup, remote: &Remote) -> String {
    let head = remote.head.as_deref().unwrap_or(&setup.head);
    let page = |nodes: &[Value]| json!({ "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": nodes });
    let mut threads = Vec::new();
    let mut comments = Vec::new();
    for item in &remote.items {
        match item {
            Item::Thread {
                id,
                resolved,
                line,
                comments,
            } => threads.push(review_thread(id, *resolved, *line, comments, &setup.head)),
            Item::General { comments: list, .. } => {
                comments.extend(list.iter().map(|c| comment(c.id, &c.login, &c.body)))
            }
            Item::RawThread(raw) => threads.push(raw.clone()),
            Item::RawGeneral(raw) => comments.push(raw.clone()),
        }
    }
    json!({ "data": { "repository": { "pullRequest": {
        "number": 42,
        "url": URL,
        "state": "OPEN",
        "closedAt": null,
        "author": { "login": remote.author },
        "headRefName": "feat",
        "headRefOid": head,
        "baseRefOid": setup.base,
        "baseRepository": { "nameWithOwner": "me/repo" },
        "reviewThreads": page(&threads),
        "reviews": page(&remote.reviews),
        "comments": page(&comments),
    } } } })
    .to_string()
}

impl FakeService for Github {
    fn name(&self) -> &'static str {
        "github"
    }

    fn origin(&self) -> &'static str {
        ORIGIN
    }

    fn number(&self) -> &'static str {
        "42"
    }

    fn target(&self) -> &'static str {
        "gh-42"
    }

    fn expect(&self) -> &Expect {
        &self.expect
    }

    fn sync_with(&self, setup: &Setup, call: Option<usize>, remote: &Remote) {
        let env = &setup.env;
        env.respond(&["api", "--method", "GET", "user"], r#"{"login":"me"}"#);
        let body = render(setup, remote);
        for query in queries("me", "repo", "42") {
            match call {
                Some(call) => env.respond_nth(&strs(&query), call, &body),
                None => env.respond(&strs(&query), &body),
            }
        }
    }

    fn is_read(&self, args: &str) -> bool {
        args.starts_with("api --method GET ")
            || (args.starts_with("api graphql ") && args.contains("query="))
    }

    fn call(&self, call: Call) -> Vec<String> {
        match call {
            Call::PostInline | Call::Approve => post(REVIEWS),
            Call::PostThread | Call::PostNote => post(COMMENTS),
            Call::Reply(thread) => mutation(thread, "AddReply", true),
            Call::Resolve(thread) => mutation(thread, "ResolveThread", false),
            Call::Unresolve(thread) => mutation(thread, "UnresolveThread", false),
            Call::PublishDraft => unreachable!("GitHub has no drafts"),
        }
    }

    fn created(&self, call: Call, id: u64) -> String {
        match call {
            Call::PostInline | Call::Approve => json!({
                "id": id,
                "html_url": format!("{URL}#pullrequestreview-{id}"),
            }),
            Call::PostThread | Call::PostNote => json!({ "id": id }),
            Call::Reply(_) => json!({ "data": { "addPullRequestReviewThreadReply": { "comment": {
                "databaseId": id,
                "url": format!("{URL}#discussion_r{id}"),
            } } } }),
            Call::Resolve(_) => {
                json!({ "data": { "resolveReviewThread": { "thread": { "isResolved": true } } } })
            }
            Call::Unresolve(_) => {
                json!({ "data": { "unresolveReviewThread": { "thread": { "isResolved": false } } } })
            }
            Call::PublishDraft => unreachable!("GitHub has no drafts"),
        }
        .to_string()
    }

    fn alice_thread(&self) -> u64 {
        301
    }

    fn alice_general(&self) -> u64 {
        900
    }

    fn local_thread(&self, id: u64) -> String {
        format!("gh-t{id}")
    }

    fn local_general(&self, id: u64) -> String {
        format!("gh-c{id}")
    }

    fn alice_remote(&self) -> &'static str {
        "PRRT_o"
    }

    fn before(&self, _setup: &Setup) -> Remote {
        let mut remote = Remote::new();
        remote.thread(
            "PRRT_o",
            true,
            5,
            vec![Comment::new(301, "alice", "Alice thread text.")],
        );
        remote.general("g", Comment::new(900, "alice", "Alice general text."));
        remote
    }

    fn stages(&self, setup: &Setup) -> [Remote; 3] {
        let f1 = setup.finding(1);
        let f2 = setup.finding(2);
        let mut posted = self.before(setup);
        posted.thread(
            "PRRT_f1",
            false,
            3,
            vec![Comment::new(501, "me", &marked("f1", &format!("{f1}/c1")))],
        );
        posted.reviews.push(own_review(&setup.head));
        posted.general(
            "601",
            Comment::new(601, "me", &marked("f2", &format!("{f2}/c1"))),
        );
        posted.general("602", Comment::new(602, "me", &marked("h1", "h1/c1")));
        let mut done = self.before(setup);
        done.set_resolved("PRRT_o", false);
        done.thread(
            "PRRT_f1",
            true,
            3,
            vec![
                Comment::new(501, "me", &marked("f1", &format!("{f1}/c1"))),
                Comment::new(801, "me", &marked("Fixed.", &format!("{f1}/c2"))),
            ],
        );
        done.reviews.push(own_review(&setup.head));
        done.general(
            "601",
            Comment::new(601, "me", &marked("f2", &format!("{f2}/c1"))),
        );
        done.general("602", Comment::new(602, "me", &marked("h1", "h1/c1")));
        done.general(
            "603",
            Comment::new(
                603,
                "me",
                &format!("{}\nRe: gh-c900", marked("Thanks", "gh-c900/c1")),
            ),
        );
        let record = format!(
            "<!-- review-record v2 id={run} head={head} reviews-at-head=1 open=1 -->\n<!-- agent-review id={run}/record -->",
            run = setup.run,
            head = setup.head
        );
        done.general("604", Comment::new(604, "me", &record));
        [self.before(setup), posted, done]
    }

    fn publish_writes(&self, env: &Env, setup: &Setup) {
        let f1 = setup.finding(1);
        env.respond(
            &strs(&post(REVIEWS)),
            r#"{"id":700,"html_url":"https://github.com/me/repo/pull/42#pullrequestreview-700"}"#,
        );
        let listed = json!([{
            "id": 501,
            "body": marked("f1", &format!("{f1}/c1")),
            "html_url": "https://github.com/me/repo/pull/42#discussion_r501",
        }]);
        env.respond(
            &["api", "--method", "GET", &format!("{REVIEWS}/700/comments")],
            &listed.to_string(),
        );
        for (call, id) in [(1, 601), (2, 602), (3, 603), (4, 604)] {
            env.respond_nth(
                &strs(&post(COMMENTS)),
                call,
                &json!({ "id": id, "html_url": format!("{URL}#c{id}") }).to_string(),
            );
        }
        env.respond(
            &strs(&mutation("PRRT_f1", "AddReply", true)),
            &self.created(Call::Reply("PRRT_f1"), 801),
        );
        env.respond(
            &strs(&mutation("PRRT_f1", "ResolveThread", false)),
            &self.created(Call::Resolve("PRRT_f1"), 0),
        );
        env.respond(
            &strs(&mutation("PRRT_o", "UnresolveThread", false)),
            &self.created(Call::Unresolve("PRRT_o"), 0),
        );
    }

    fn assert_dry_reads(&self, env: &Env, before: usize) {
        for call in &env.forge_calls()[before..] {
            assert!(
                !call.contains("POST") && !call.contains("mutation"),
                "{call}"
            );
        }
        assert_eq!(
            env.forge_calls().len() - before,
            4,
            "login and three queries"
        );
    }

    fn assert_dry_run(&self, setup: &Setup, dry: &Value) {
        let f1 = setup.finding(1);
        let writes = dry["writes"].as_array().unwrap();
        let review = &writes[0]["input"];
        assert_eq!(review["commit_id"], setup.head);
        assert_eq!(review["event"], "COMMENT");
        let comment = &review["comments"][0];
        assert_eq!(
            (&comment["path"], &comment["line"], &comment["side"]),
            (&json!("a.txt"), &json!(3), &json!("RIGHT"))
        );
        let body = comment["body"].as_str().unwrap();
        assert!(body.starts_with("**bug (high):** In-diff text of f1.\n\nm1 xhigh claude-code "));
        assert!(body.contains(&format!(
            "{}.c1p0.r1\n<!-- review-finding v1 id={f1} sha={} -->",
            setup.run, setup.head
        )));
        assert!(body.ends_with(&format!(
            "<!-- agent-review id={f1}/c1 attempt=<attempt> -->"
        )));
        let outside = writes[1]["input"]["body"].as_str().unwrap();
        assert!(outside.starts_with("**nit:** General text of f2."));
        assert_eq!(writes[1]["call"], json!(post(COMMENTS)));
        assert!(writes[2]["input"]["body"].as_str().unwrap().starts_with(
            "General note from me.\n\nme\n<!-- agent-review id=h1/c1 attempt=<attempt> -->"
        ));
        assert_eq!(dry["skipped"][0]["item"], "h2");
        super::super::scenario::assert_record(setup, writes[7]["input"]["body"].as_str().unwrap());
    }

    fn assert_posted(&self, setup: &Setup) {
        let env = &setup.env;
        let inputs = env.forge_inputs();
        let reply = inputs
            .iter()
            .find(|(call, _)| call.contains("query=AddReply"))
            .unwrap();
        assert!(
            reply
                .1
                .starts_with("Fixed in the same commit.\n\nm2 high claude-code ")
        );
        let answer = inputs
            .iter()
            .find(|(_, input)| input.contains("Thanks, done."))
            .unwrap();
        let answer: Value = serde_json::from_str(&answer.1).unwrap();
        let answer = answer["body"].as_str().unwrap();
        assert!(
            answer.contains("<!-- agent-review id=gh-c900/c1 attempt=at-"),
            "{answer}"
        );
        assert!(answer.ends_with(" -->\nRe: gh-c900"), "{answer}");
        super::super::scenario::assert_never_posted(env);
        let list = env.run(&["list"]);
        let f1 = thread(&list, &setup.finding(1));
        assert_eq!(f1["remote"]["id"], "PRRT_f1");
        assert_eq!(f1["comments"][0]["remote"]["id"], "501");
        assert_eq!(f1["comments"][1]["remote"]["id"], "801");
        assert_eq!(f1["resolve_pending"], Value::Null);
        assert_eq!(thread(&list, "gh-t301")["resolved"], false);
        assert_eq!(thread(&list, "h2")["comments"][0]["post"], "pending");
        assert!(
            !ids(&list).contains(&"gh-r700".to_owned()),
            "our review body is not a thread"
        );
        let status = env.run(&["status", "--full"]);
        assert_eq!(
            status["reviews_at_head"], 1,
            "the synced-back record is ours"
        );
        assert_eq!(status["runs"][0]["record"]["post"], "posted");
        assert_eq!(status["runs"][0]["record"]["remote_id"], "604");
    }

    fn assert_counts_once(&self, env: &Env) {
        assert_eq!(self.count(env, Call::PostInline), 1);
        assert_eq!(self.count(env, Call::PostNote), 4);
    }

    fn assert_range(&self, dry: &Value, start: u32, end: u32, text: &str) {
        let comments = dry["writes"][0]["input"]["comments"].as_array().unwrap();
        let multi = comments
            .iter()
            .find(|comment| comment["body"].as_str().unwrap().contains(text))
            .unwrap_or_else(|| panic!("{dry}"));
        assert_eq!(
            (&multi["start_line"], &multi["line"], &multi["start_side"]),
            (&json!(start), &json!(end), &json!("RIGHT")),
        );
    }
}

/// The rich sync fixtures: threads over two pages, reviews and comments over two pages.
pub fn sync_fixtures(setup: &Setup) {
    let env = &setup.env;
    let vars = [
        ("RUN", setup.run.as_str()),
        ("HEAD", setup.head.as_str()),
        ("BASE", setup.base.as_str()),
        ("REPO", "me/repo"),
        ("NUMBER", "42"),
    ];
    env.respond(&["api", "--method", "GET", "user"], r#"{"login":"me"}"#);
    env.respond(
        &[
            "api",
            "--method",
            "GET",
            "repos/me/repo/pulls?head=me%3Afeat&state=all&per_page=20",
        ],
        r#"[{"number":42,"state":"open"}]"#,
    );
    let pull = graphql("me", "repo", "42", "PullRequest");
    env.respond(&strs(&pull), &fixture("github/threads-1.json", &vars));
    env.respond(
        &strs(&with_cursor(pull, "t-2")),
        &fixture("github/threads-2.json", &vars),
    );
    env.respond(
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
        &fixture("github/thread-comments.json", &vars),
    );
    env.respond(
        &strs(&graphql("me", "repo", "42", "PullReviews")),
        &fixture("github/reviews.json", &vars),
    );
    let comments = graphql("me", "repo", "42", "PullComments");
    env.respond(&strs(&comments), &fixture("github/comments-1.json", &vars));
    env.respond(
        &strs(&with_cursor(comments, "ic-2")),
        &fixture("github/comments-2.json", &vars),
    );
}

/// An otherwise empty PR `number` of `repo` in `state`, closed at `closed` (a JSON value), whose
/// head is `head`, answered to every PR query.
pub fn empty_pr(
    env: &Env,
    repo: &str,
    number: &str,
    state: &str,
    closed: &str,
    head: &str,
    base: &str,
) {
    let (owner, name) = repo.split_once('/').unwrap();
    let vars = [
        ("HEAD", head),
        ("BASE", base),
        ("REPO", repo),
        ("NUMBER", number),
        ("STATE", state),
        ("CLOSED", closed),
    ];
    for query in queries(owner, name, number) {
        env.respond(&strs(&query), &fixture("github/empty-pr.json", &vars));
    }
}
