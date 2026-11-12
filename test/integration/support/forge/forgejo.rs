//! The fake Forgejo: pull request, review and comment reads, REST writes, and the `tea api`
//! argument lists the CLI transport renders for them.
//!
//! Nothing here has met a real tea or a real Forgejo. The flags (`--login`, `-i`, `-X`, `-H`,
//! `-d @-` before the endpoint) follow tea's `api` source; the `tea logins list --output json`
//! shape and the REST routes and JSON are the assumptions marked `unverified:` in
//! `src/forge/transport.rs` and the Forgejo service files. This fake mirrors them, so a passing
//! test shows agt is consistent with them, not that tea or Forgejo behave so.

use std::fs;

use serde_json::{Value, json};

use super::super::scenario::{Setup, ids, marked, sent, thread};
use super::super::{Env, fixture, strs};
use super::argv::{TEA_LOGIN, tea_bodyless, tea_get, tea_get_as, tea_is_read, tea_write};
use super::{Call, Comment, Expect, FakeService, Item, Remote, TIME};

pub const HOST: &str = "codeberg.org";
pub const REPO: &str = "me/proj";
pub const NUMBER: &str = "5";
pub const ORIGIN: &str = "git@codeberg.org:me/proj.git";
pub const PULL: &str = "repos/me/proj/pulls/5";
pub const REVIEWS: &str = "repos/me/proj/pulls/5/reviews";
pub const COMMENTS: &str = "repos/me/proj/issues/5/comments";
const URL: &str = "https://codeberg.org/me/proj/pulls/5";
/// The review every in-diff comment of the fake belongs to.
const REVIEW: u64 = 700;

pub struct Forgejo {
    expect: Expect,
}

impl Forgejo {
    pub fn new() -> Self {
        Self {
            expect: Expect::load("forgejo"),
        }
    }
}

pub fn post(path: &str) -> Vec<String> {
    tea_write("POST", path)
}

/// Makes `tea logins list` name `logins`, each as `(name, url)`; `tea` knows none by default.
pub fn logins_of(env: &Env, logins: &[(&str, &str)]) {
    let list: Vec<Value> = logins
        .iter()
        .map(|(name, url)| json!({ "name": name, "url": url, "user": "me", "default": true }))
        .collect();
    fs::write(
        env.path("forge").join("tea-logins.json"),
        Value::Array(list).to_string(),
    )
    .unwrap();
}

/// Logs tea in to codeberg.org under [`TEA_LOGIN`].
pub fn logins(env: &Env) {
    logins_of(env, &[(TEA_LOGIN, "https://codeberg.org")]);
}

/// Logs tea in and answers `tea api user` as `me`.
pub fn login(env: &Env) {
    logins(env);
    env.respond(&strs(&tea_get("user")), r#"{"login":"me"}"#);
}

/// Answers the list `path` with `items` one by one: `tea api` is asked for pages of its `limit`,
/// and the fake serves the one asked for.
pub fn respond_list(env: &Env, path: &str, items: &[Value]) {
    let lines: Vec<String> = items.iter().map(Value::to_string).collect();
    let text = format!("{}\n", lines.join("\n"));
    let key = env.forge_file(&strs(&tea_get(path)), "items");
    fs::write(key, text).unwrap();
}

pub fn comment(id: u64, login: &str, body: &str) -> Value {
    json!({
        "id": id,
        "html_url": format!("{URL}#issuecomment-{id}"),
        "body": body,
        "created_at": TIME,
        "user": { "login": login },
    })
}

/// A comment on line `line` of `a.txt`, resolved when `resolved`, made at `head`.
pub fn code_comment(c: &Comment, line: u32, resolved: bool, head: &str) -> Value {
    let mut node = comment(c.id, &c.login, &c.body);
    node["path"] = json!("a.txt");
    node["position"] = json!(line);
    node["original_position"] = json!(0);
    node["commit_id"] = json!(head);
    node["pull_request_review_id"] = json!(REVIEW);
    node["resolver"] = if resolved {
        json!({ "login": "alice" })
    } else {
        Value::Null
    };
    node
}

/// A submitted review by `login` with `state` and `body` and no in-diff comments.
pub fn review(id: u64, login: &str, state: &str, body: &str) -> Value {
    json!({
        "id": id,
        "html_url": format!("{URL}#issuecomment-{id}"),
        "body": body,
        "state": state,
        "submitted_at": TIME,
        "user": { "login": login },
        "comments_count": 0,
    })
}

/// The review this store posted for `head` and read back: its body is the marker alone, as sent.
pub fn own_review(head: &str) -> Value {
    review(
        REVIEW,
        "me",
        "COMMENT",
        &format!("<!-- agent-review id=review/{head} -->"),
    )
}

/// The reads of a sync, with their answers: the pull request, its reviews, the comments of
/// the review [`REVIEW`] and the general comments.
fn render(setup: &Setup, remote: &Remote) -> [(Vec<String>, String); 4] {
    let head = remote.head.as_deref().unwrap_or(&setup.head);
    let mut inline = Vec::new();
    let mut general = Vec::new();
    for item in &remote.items {
        match item {
            Item::Thread {
                resolved,
                line,
                comments,
                ..
            } => inline.extend(
                comments
                    .iter()
                    .map(|c| code_comment(c, *line, *resolved, &setup.head)),
            ),
            Item::General { comments, .. } => {
                general.extend(comments.iter().map(|c| comment(c.id, &c.login, &c.body)));
            }
        }
    }
    let mut reviews = remote.reviews.clone();
    if !inline.is_empty() {
        let comments = inline.len();
        match reviews.iter_mut().find(|node| node["id"] == REVIEW) {
            Some(node) => node["comments_count"] = json!(comments),
            None => {
                let mut own = review(REVIEW, "me", "COMMENT", "");
                own["comments_count"] = json!(comments);
                reviews.push(own);
            }
        }
    }
    let vars = [
        ("HOST", HOST),
        ("REPO", REPO),
        ("NUMBER", NUMBER),
        ("HEAD", head),
        ("BASE", setup.base.as_str()),
        ("AUTHOR", remote.author),
    ];
    [
        (tea_get(PULL), fixture("forgejo/pr.json", &vars)),
        (tea_get(REVIEWS), Value::Array(reviews).to_string()),
        (
            tea_get(&format!("{REVIEWS}/{REVIEW}/comments")),
            Value::Array(inline).to_string(),
        ),
        (tea_get(COMMENTS), Value::Array(general).to_string()),
    ]
}

impl FakeService for Forgejo {
    fn origin(&self) -> &'static str {
        ORIGIN
    }

    fn number(&self) -> &'static str {
        NUMBER
    }

    fn target(&self) -> &'static str {
        "fg-5"
    }

    fn expect(&self) -> &Expect {
        &self.expect
    }

    fn sync_with(&self, setup: &Setup, call: Option<usize>, remote: &Remote) {
        let env = &setup.env;
        login(env);
        for (request, body) in render(setup, remote) {
            match call {
                Some(call) => env.respond_nth(&strs(&request), call, &body),
                None => env.respond(&strs(&request), &body),
            }
        }
    }

    fn rich_origin(&self) -> &'static str {
        ORIGIN
    }

    fn rich_env(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }

    fn rich_sync(&self, setup: &Setup) {
        sync_fixtures(setup);
    }

    fn is_read(&self, args: &str) -> bool {
        tea_is_read(args)
    }

    fn call(&self, call: Call) -> Vec<String> {
        match call {
            Call::PostInline | Call::Approve => post(REVIEWS),
            Call::PostThread | Call::PostNote | Call::Reply(_) => post(COMMENTS),
            // No route resolves a thread, so a resolve is never sent; its builder is a read.
            Call::Resolve(_) | Call::Unresolve(_) => tea_get(PULL),
            Call::EditComment(id) => {
                tea_write("PATCH", &format!("repos/me/proj/issues/comments/{id}"))
            }
            // The fake deletes only a diff comment, which a review holds.
            Call::DeleteComment(id) => tea_bodyless(
                "DELETE",
                &format!("repos/me/proj/pulls/5/reviews/{REVIEW}/comments/{id}"),
            ),
            Call::PublishDraft => unreachable!("Forgejo has no drafts"),
        }
    }

    /// A delete through the general route answers 204 and keeps a diff comment.
    /// Only the route of the review counts as the delete.
    fn accept(&self, env: &Env, call: Call, id: u64) {
        env.respond(&strs(&self.call(call)), &self.created(call, id));
        if let Call::DeleteComment(comment) = call {
            let general = format!("repos/me/proj/issues/comments/{comment}");
            env.respond(&strs(&tea_bodyless("DELETE", &general)), "");
        }
    }

    fn created(&self, call: Call, id: u64) -> String {
        match call {
            Call::PostInline | Call::Approve => json!({
                "id": id,
                "html_url": format!("{URL}#issuecomment-{id}"),
            }),
            Call::PostThread | Call::PostNote | Call::Reply(_) => json!({
                "id": id,
                "html_url": format!("{URL}#issuecomment-{id}"),
            }),
            Call::Resolve(_) | Call::Unresolve(_) => json!({}),
            Call::EditComment(_) => json!({ "id": id }),
            Call::DeleteComment(_) => return String::new(),
            Call::PublishDraft => unreachable!("Forgejo has no drafts"),
        }
        .to_string()
    }

    fn alice_thread(&self) -> u64 {
        301
    }

    fn alice_general(&self) -> u64 {
        900
    }

    fn landed_inline(&self) -> u64 {
        501
    }

    fn local_thread(&self, id: u64) -> String {
        format!("fg-t{id}")
    }

    fn local_general(&self, id: u64) -> String {
        format!("fg-c{id}")
    }

    fn before(&self, _setup: &Setup) -> Remote {
        let mut remote = Remote::new();
        remote.thread(
            "301",
            true,
            5,
            vec![Comment::new(301, "alice", "Alice thread text.")],
        );
        remote.general("900", Comment::new(900, "alice", "Alice general text."));
        remote
    }

    fn plain_review(&self, id: u64, login: &str, body: &str) -> Option<Value> {
        Some(review(id, login, "COMMENT", body))
    }

    fn land_inline(&self, setup: &Setup) -> Remote {
        let env = &setup.env;
        let f1 = setup.finding(1);
        self.link(setup);
        self.lose(env, Call::PostInline);
        env.fail(&["publish", "--pr", NUMBER], None);
        let mut remote = Remote::new();
        remote.thread(
            "501",
            false,
            3,
            vec![Comment::new(501, "me", &sent(env, &f1))],
        );
        remote.reviews.push(own_review(&setup.head));
        self.sync(setup, &remote);
        env.pull(&["sync", "--pr", NUMBER]);
        self.restore(env, Call::PostInline);
        remote
    }

    fn land_findings(&self, setup: &Setup) -> (Remote, String) {
        let mut remote = self.land_inline(setup);
        // The general finding f2 is not posted here (a general comment cannot be resolved), so
        // the general comment is one of mine that sync imports.
        remote.general("601", Comment::new(601, "me", "A general remark of mine."));
        self.sync(setup, &remote);
        setup.env.pull(&["sync", "--pr", NUMBER]);
        (remote, "601".to_owned())
    }

    fn stages(&self, setup: &Setup) -> [Remote; 3] {
        let f1 = setup.finding(1);
        let posted_f1 = |remote: &mut Remote, resolved: bool| {
            let comment = Comment::new(501, "me", &marked("f1", &format!("{f1}/c1")));
            remote.thread("501", resolved, 3, vec![comment]);
            remote.reviews.push(own_review(&setup.head));
        };
        let mut start = self.before(setup);
        posted_f1(&mut start, false);
        let mut posted = self.before(setup);
        posted_f1(&mut posted, false);
        posted.general("602", Comment::new(602, "me", &marked("h1", "h1/c1")));
        // The host cannot resolve a thread for agt, so the fixed f1 is resolved there by hand:
        // a sync takes the host's state for a thread.
        let mut done = self.before(setup);
        posted_f1(&mut done, true);
        done.general("602", Comment::new(602, "me", &marked("h1", "h1/c1")));
        // Forgejo shows no edits, so the record must read as this store sent it.
        done.general("605", Comment::new(605, "me", "{{LAST_BODY}}"));
        [start, posted, done]
    }

    fn publish_writes(&self, env: &Env, _setup: &Setup) {
        env.respond(
            &strs(&post(REVIEWS)),
            &self.created(Call::PostInline, REVIEW),
        );
        for call in 1..=4 {
            self.accept_nth(env, Call::PostNote, call, 601 + call as u64);
        }
    }

    fn assert_dry_reads(&self, env: &Env, before: usize) {
        for call in &env.forge_calls()[before..] {
            assert!(!call.contains("-X POST"), "{call}");
        }
    }

    fn assert_dry_run(&self, _setup: &Setup, _dry: &Value) {}

    fn assert_inline_dry(&self, setup: &Setup, dry: &Value) {
        let f1 = setup.finding(1);
        let review = &dry["writes"][0]["input"];
        assert_eq!(review["commit_id"], setup.head);
        assert_eq!(review["event"], "COMMENT");
        let comment = &review["comments"][0];
        assert_eq!(
            (
                &comment["path"],
                &comment["new_position"],
                comment.get("line")
            ),
            (&json!("a.txt"), &json!(3), None)
        );
        let body = comment["body"].as_str().unwrap();
        assert!(
            body.starts_with(
                "**🐛 bug (high)**\n\nIn-diff text of f1.\n\n> <sub>🔍 agt review · m1</sub>\n"
            ),
            "{body}"
        );
        assert!(body.contains(&format!(
            "</sub>\n<!-- review-finding v1 id={f1} sha={} -->",
            setup.head
        )));
        assert!(body.ends_with(&format!(
            "<!-- agent-review id={f1}/c1 attempt=<attempt> -->"
        )));
        assert_eq!(
            review["body"],
            format!("<!-- agent-review id=review/{} -->", setup.head)
        );
    }

    fn assert_posted(&self, setup: &Setup) {
        let env = &setup.env;
        assert_never_posted(env);
        let list = env.run(&["list"]);
        let f1 = thread(&list, &setup.finding(1));
        assert_eq!(f1["comments"][0]["remote"]["id"], "501");
        assert_eq!(thread(&list, "fg-t301")["resolved"], true);
        assert!(
            !ids(&list).contains(&format!("fg-r{REVIEW}")),
            "our review body is not a thread"
        );
        let status = env.run(&["status", "--full"]);
        assert_eq!(status["reviews_at_head"], 1);
        assert_eq!(status["runs"][0]["record"]["post"], "posted");
        assert_eq!(status["runs"][0]["record"]["remote_id"], "605");
    }

    fn assert_counts_once(&self, env: &Env) {
        // The note of the thread h1, the two quoted answers and the summary.
        assert_eq!(self.count(env, Call::PostNote), 4);
        // The one review is the lost write that landed f1; it is not posted again.
        assert_eq!(self.count(env, Call::PostInline), 1);
    }

    fn assert_range(&self, dry: &Value, _start: u32, _end: u32, text: &str) {
        // No ranges: the comment is placed on the end line alone.
        let comments = dry["writes"][0]["input"]["comments"].as_array().unwrap();
        let single = comments
            .iter()
            .find(|comment| comment["body"].as_str().unwrap().contains(text))
            .unwrap_or_else(|| panic!("{dry}"));
        assert!(single.get("start_line").is_none(), "{dry}");
    }
}

/// Nothing of someone else's is posted but as a quote: with no reply in a thread, an answer to
/// Alice's comment is a general comment that quotes her first line.
fn assert_never_posted(env: &Env) {
    for (call, input) in env.forge_inputs() {
        for text in ["Killed text of f3", "Someone else's text"] {
            assert!(!input.contains(text), "{call} posted {text}: {input}");
        }
        let Some(body) = serde_json::from_str::<Value>(&input)
            .ok()
            .and_then(|input| input["body"].as_str().map(str::to_owned))
        else {
            continue;
        };
        for line in body
            .lines()
            .filter(|line| line.contains("Alice thread text"))
        {
            assert!(line.starts_with("> "), "{call} reposted Alice: {body}");
        }
    }
}

/// A repository on a Forgejo host and the tea login that reaches it.
pub struct Site {
    pub host: &'static str,
    pub repo: &'static str,
    pub login: &'static str,
}

pub const CODEBERG: Site = Site {
    host: HOST,
    repo: REPO,
    login: TEA_LOGIN,
};

/// A self-hosted Forgejo that only the hosts file or a tea login names.
pub const SELF_HOSTED: Site = Site {
    host: "git.example.com",
    repo: "group/proj",
    login: "work",
};

impl Site {
    /// The `origin` remote of the repository.
    pub fn origin(&self) -> String {
        format!("git@{}:{}.git", self.host, self.repo)
    }

    /// The read of `path` through the login of the site.
    pub fn get(&self, path: &str) -> Vec<String> {
        tea_get_as(self.login, path)
    }

    /// Logs tea in to the site and answers `tea api user` as `me`.
    pub fn log_in(&self, env: &Env) {
        logins_of(env, &[(self.login, &format!("https://{}", self.host))]);
        env.respond(&strs(&self.get("user")), r#"{"login":"me"}"#);
    }
}

/// The rich sync fixtures of the pull request 5 of `site`: reviews of every state, a thread of
/// two comments and one on a removed line, and general comments.
pub fn sync_fixtures_at(setup: &Setup, site: &Site) {
    let env = &setup.env;
    let vars = [
        ("HOST", site.host),
        ("REPO", site.repo),
        ("NUMBER", NUMBER),
        ("HEAD", setup.head.as_str()),
        ("BASE", setup.base.as_str()),
        ("AUTHOR", "me"),
    ];
    let pull = format!("repos/{}/pulls/{NUMBER}", site.repo);
    site.log_in(env);
    env.respond(&strs(&site.get(&pull)), &fixture("forgejo/pr.json", &vars));
    env.respond(
        &strs(&site.get(&format!("{pull}/reviews"))),
        &fixture("forgejo/reviews.json", &vars),
    );
    env.respond(
        &strs(&site.get(&format!("{pull}/reviews/5004/comments"))),
        &fixture("forgejo/review-comments.json", &vars),
    );
    env.respond(
        &strs(&site.get(&format!("repos/{}/issues/{NUMBER}/comments", site.repo))),
        &fixture("forgejo/comments.json", &vars),
    );
}

/// [`sync_fixtures_at`] for codeberg.org.
pub fn sync_fixtures(setup: &Setup) {
    sync_fixtures_at(setup, &CODEBERG);
}
