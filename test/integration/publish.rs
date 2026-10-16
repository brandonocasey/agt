use serde_json::{Value, json};

use crate::support::{Env, fixture, numbered, snapshot};

const REVIEWER: &str =
    r#"[{"model":"m1","effort":"xhigh","harness":"claude-code","harness_version":"2.1.296"}]"#;
const FINDINGS: &str = r#"[
  {"type":"bug","severity":"high","path":"a.txt","line":3,"scenario":"In-diff text of f1."},
  {"type":"nit","path":"a.txt","line":9,"scenario":"Out-of-diff text of f2."},
  {"type":"bug","severity":"low","path":"a.txt","line":3,"scenario":"Killed text of f3.","killed":true,"reason":"not reproducible"}
]"#;
/// Text that `publish` must never send: killed findings and other people's comments.
const NEVER_POSTED: [&str; 4] = [
    "Killed text of f3",
    "Alice thread text",
    "Alice general text",
    "Someone else's text",
];
const TIME: &str = "2026-10-16T10:00:00Z";
const GH_REVIEWS: &str = "repos/me/repo/pulls/42/reviews";
const GH_COMMENTS: &str = "repos/me/repo/issues/42/comments";
const GL_MR: &str = "projects/me%2Fproj/merge_requests/3";

struct Setup {
    env: Env,
    run: String,
    head: String,
    base: String,
}

impl Setup {
    fn finding(&self, number: u32) -> String {
        format!("{}.c1p0.r1.f{number}", self.run)
    }
}

/// A `feat` branch changing line 3 of `a.txt`, reviewed by one full pass with an in-diff
/// finding (f1), an out-of-diff one (f2, line 9) and a killed one (f3). The run is still open.
fn reviewed(origin: &str) -> Setup {
    reviewed_with(origin, false)
}

/// Like [`reviewed`]; `lock` ends the pass locked, as a blind review does.
fn reviewed_with(origin: &str, lock: bool) -> Setup {
    let env = Env::with_branch();
    env.git(&["remote", "add", "origin", origin]);
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let head = env.commit_all("change");
    let base = env.git(&["rev-parse", "main"]);
    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let pass = [
        "pass",
        "start",
        "--run",
        &run,
        "--kind",
        "full",
        "--reviewers",
        "-",
    ];
    env.run_stdin(&pass, REVIEWER);
    let add = [
        "finding",
        "add",
        "--run",
        &run,
        "--pass",
        "c1p0",
        "--reviewer",
        "r1",
    ];
    env.run_stdin(&add, FINDINGS);
    let mut end = vec!["pass", "end", "--run", &run, "--pass", "c1p0"];
    if lock {
        end.push("--lock");
    }
    env.run(&end);
    Setup {
        env,
        run,
        head,
        base,
    }
}

/// Links the reviewed branch to PR 42, then ends its run: `publish` posts only runs that end
/// after the target is linked.
fn link(setup: &Setup) {
    let env = &setup.env;
    gh_sync(env, None, &GhPr::new().render(setup));
    env.run(&["sync", "--pr", "42"]);
    env.run(&["run", "end", "--run", &setup.run]);
}

fn marked(text: &str, local: &str) -> String {
    format!("{text}\n<!-- agent-review id={local} -->")
}

/// The text a failed write sent for the first comment of `thread`, as the host shows it.
fn sent(env: &Env, thread_id: &str) -> String {
    let list = env.run(&["list"]);
    thread(&list, thread_id)["comments"][0]["sent"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

fn post(path: &str) -> Vec<String> {
    args(&["api", "--method", "POST", path, "--input", "-"])
}

fn gl_write(method: &str, path: &str) -> Vec<String> {
    args(&[
        "api",
        "-X",
        method,
        path,
        "-H",
        "Content-Type: application/json",
        "--input",
        "-",
    ])
}

fn strs(list: &[String]) -> Vec<&str> {
    list.iter().map(String::as_str).collect()
}

/// Forge calls whose arguments are exactly `list`.
fn count_calls(env: &Env, list: &[String]) -> usize {
    let wanted = list.join(" ");
    env.forge_calls()
        .iter()
        .filter(|call| call.split('\t').nth(1) == Some(wanted.as_str()))
        .count()
}

fn thread<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["id"] == id)
        .unwrap_or_else(|| panic!("no thread {id} in {list}"))
}

fn assert_never_posted(env: &Env) {
    for (call, input) in env.forge_inputs() {
        for text in NEVER_POSTED {
            assert!(!input.contains(text), "{call} posted {text}: {input}");
        }
    }
}

// GitHub

fn gh_comment(id: u64, login: &str, body: &str) -> Value {
    json!({
        "databaseId": id,
        "url": format!("https://github.com/me/repo/pull/42#c{id}"),
        "body": body,
        "createdAt": TIME,
        "author": { "login": login },
        "editor": null,
    })
}

fn gh_thread(id: &str, resolved: bool, line: u32, comments: Vec<Value>, head: &str) -> Value {
    let comments: Vec<Value> = comments
        .into_iter()
        .map(|mut comment| {
            comment["originalCommit"] = json!({ "oid": head });
            comment["pullRequestReview"] = json!({ "databaseId": 700 });
            comment
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
        "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": comments },
    })
}

/// One response for all three PR queries, as GitHub would page them.
struct GhPr {
    author: &'static str,
    /// The PR head when it is not the setup's head, as after a force-push.
    head: Option<String>,
    threads: Vec<Value>,
    reviews: Vec<Value>,
    comments: Vec<Value>,
}

impl GhPr {
    fn new() -> Self {
        Self {
            author: "me",
            head: None,
            threads: Vec::new(),
            reviews: Vec::new(),
            comments: Vec::new(),
        }
    }

    fn render(&self, setup: &Setup) -> String {
        let page = |nodes: &[Value]| json!({ "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": nodes });
        json!({ "data": { "repository": { "pullRequest": {
            "number": 42,
            "url": "https://github.com/me/repo/pull/42",
            "state": "OPEN",
            "closedAt": null,
            "author": { "login": self.author },
            "headRefName": "feat",
            "headRefOid": self.head.as_deref().unwrap_or(&setup.head),
            "baseRefOid": setup.base,
            "baseRepository": { "nameWithOwner": "me/repo" },
            "reviewThreads": page(&self.threads),
            "reviews": page(&self.reviews),
            "comments": page(&self.comments),
        } } } })
        .to_string()
    }

    fn review(&mut self, head: &str) {
        self.reviews.push(json!({
            "databaseId": 700,
            "url": "https://github.com/me/repo/pull/42#pullrequestreview-700",
            "body": marked("Review findings.", &format!("review/{head}")),
            "state": "COMMENTED",
            "submittedAt": TIME,
            "author": { "login": "me" },
            "editor": null,
        }));
    }
}

fn gh_queries() -> Vec<Vec<String>> {
    ["PullRequest", "PullReviews", "PullComments"]
        .iter()
        .map(|query| {
            args(&[
                "api",
                "graphql",
                "-f",
                "owner=me",
                "-f",
                "name=repo",
                "-F",
                "number=42",
                "-f",
                &format!("query={query}"),
            ])
        })
        .collect()
}

/// Sync responses: `pr` for the `call`-th sync, or for every sync without one of its own.
fn gh_sync(env: &Env, call: Option<usize>, pr: &str) {
    env.respond(&["api", "--method", "GET", "user"], r#"{"login":"me"}"#);
    for query in gh_queries() {
        match call {
            Some(call) => env.respond_nth(&strs(&query), call, pr),
            None => env.respond(&strs(&query), pr),
        }
    }
}

fn gh_mutation(thread: &str, query: &str, body: bool) -> Vec<String> {
    let mut list = args(&["api", "graphql", "-f", &format!("threadId={thread}")]);
    if body {
        list.extend(args(&["-f", "body="]));
    }
    list.extend(args(&["-f", &format!("query={query}")]));
    list
}

fn gh_before(setup: &Setup) -> GhPr {
    let mut pr = GhPr::new();
    pr.threads.push(gh_thread(
        "PRRT_o",
        true,
        5,
        vec![gh_comment(301, "alice", "Alice thread text.")],
        &setup.head,
    ));
    pr.comments
        .push(gh_comment(900, "alice", "Alice general text."));
    pr
}

/// The PR after the new threads are posted, and after everything is.
fn gh_after(setup: &Setup) -> (GhPr, GhPr) {
    let f1 = setup.finding(1);
    let f2 = setup.finding(2);
    let mut threads = gh_before(setup);
    threads.threads.push(gh_thread(
        "PRRT_f1",
        false,
        3,
        vec![gh_comment(501, "me", &marked("f1", &format!("{f1}/c1")))],
        &setup.head,
    ));
    threads.review(&setup.head);
    threads
        .comments
        .push(gh_comment(601, "me", &marked("f2", &format!("{f2}/c1"))));
    threads
        .comments
        .push(gh_comment(602, "me", &marked("h1", "h1/c1")));
    let mut done = gh_before(setup);
    done.threads[0]["isResolved"] = json!(false);
    done.threads.push(gh_thread(
        "PRRT_f1",
        true,
        3,
        vec![
            gh_comment(501, "me", &marked("f1", &format!("{f1}/c1"))),
            gh_comment(801, "me", &marked("Fixed.", &format!("{f1}/c2"))),
        ],
        &setup.head,
    ));
    done.review(&setup.head);
    done.comments = threads.comments.clone();
    done.comments.push(gh_comment(
        603,
        "me",
        &format!("{}\nRe: gh-c900", marked("Thanks", "gh-c900/c1")),
    ));
    let record = format!(
        "<!-- review-record v2 id={run} head={head} reviews-at-head=1 open=1 -->\n<!-- agent-review id={run}/record -->",
        run = setup.run,
        head = setup.head
    );
    done.comments.push(gh_comment(604, "me", &record));
    (threads, done)
}

fn gh_writes(env: &Env, setup: &Setup) {
    let f1 = setup.finding(1);
    env.respond(
        &strs(&post(GH_REVIEWS)),
        r#"{"id":700,"html_url":"https://github.com/me/repo/pull/42#pullrequestreview-700"}"#,
    );
    let listed = json!([{
        "id": 501,
        "body": marked("f1", &format!("{f1}/c1")),
        "html_url": "https://github.com/me/repo/pull/42#discussion_r501",
    }]);
    env.respond(
        &[
            "api",
            "--method",
            "GET",
            &format!("{GH_REVIEWS}/700/comments"),
        ],
        &listed.to_string(),
    );
    for (call, id) in [(1, 601), (2, 602), (3, 603), (4, 604)] {
        env.respond_nth(
            &strs(&post(GH_COMMENTS)),
            call,
            &json!({ "id": id, "html_url": format!("https://github.com/me/repo/pull/42#c{id}") })
                .to_string(),
        );
    }
    env.respond(
        &strs(&gh_mutation("PRRT_f1", "AddReply", true)),
        r#"{"data":{"addPullRequestReviewThreadReply":{"comment":{"databaseId":801,"url":"https://github.com/me/repo/pull/42#discussion_r801"}}}}"#,
    );
    env.respond(
        &strs(&gh_mutation("PRRT_f1", "ResolveThread", false)),
        r#"{"data":{"resolveReviewThread":{"thread":{"isResolved":true}}}}"#,
    );
    env.respond(
        &strs(&gh_mutation("PRRT_o", "UnresolveThread", false)),
        r#"{"data":{"unresolveReviewThread":{"thread":{"isResolved":false}}}}"#,
    );
}

/// Local work on top of the review: a fix reply and resolve, a reopened and a replied imported
/// thread, an own general comment and someone else's.
fn local_changes(setup: &Setup, reopen: &str, answer: &str) {
    let env = &setup.env;
    let f1 = setup.finding(1);
    env.run(&[
        "comment",
        "--body",
        "General note from me.",
        "--author",
        "me",
    ]);
    env.run(&[
        "comment",
        "--body",
        "Someone else's text.",
        "--author",
        "alice",
    ]);
    env.run(&[
        "reply",
        &f1,
        "--body",
        "Fixed in the same commit.",
        "--model",
        "m2",
        "--effort",
        "high",
        "--harness",
        "claude-code",
        "--harness-version",
        "2.1.300",
        "--run",
        &setup.run,
        "--pass",
        "c1p0",
        "--reviewer",
        "fix",
    ]);
    env.run(&[
        "resolve",
        &f1,
        "--triage",
        "fixed",
        "--resolution",
        &format!("fixed {}", setup.head),
    ]);
    env.run(&["resolve", reopen, "--reopen"]);
    env.run(&["reply", answer, "--body", "Thanks, done.", "--author", "me"]);
    env.run(&["run", "end", "--run", &setup.run]);
}

fn kinds(output: &Value, field: &str) -> Vec<String> {
    output[field]
        .as_array()
        .unwrap()
        .iter()
        .map(|write| write["kind"].as_str().unwrap().to_owned())
        .collect()
}

fn assert_record(setup: &Setup, record: &str) {
    let short = &setup.head[..7];
    assert!(
        record.starts_with(&format!(
            "**Review {}:** reviewed at {short}. 1 cycles, 2 found, 1 fixed, 1 open.\n",
            setup.run
        )),
        "{record}"
    );
    assert!(record.contains(&format!(
        "| c1p0 full | {short} | fresh | r1 m1 xhigh · Claude Code 2.1.296 |"
    )));
    assert!(record.contains("Fixed by: m2 high · Claude Code 2.1.300\n"));
    assert!(!record.contains("Reveal:"));
    assert!(record.contains(&format!(
        "<!-- review-record v2 id={} head={} reviews-at-head=1 open=1 -->",
        setup.run, setup.head
    )));
    let marker = format!("<!-- agent-review id={}/record attempt=", setup.run);
    assert!(
        record.rsplit_once('\n').unwrap().1.starts_with(&marker),
        "{record}"
    );
    assert!(record.ends_with(" -->"), "{record}");
}

fn assert_gh_dry_run(setup: &Setup, dry: &Value) {
    let f1 = setup.finding(1);
    assert_eq!(
        kinds(dry, "writes"),
        [
            "review",
            "thread",
            "thread",
            "reply",
            "reply",
            "resolve",
            "unresolve",
            "record"
        ]
    );
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
    assert!(outside.starts_with("`a.txt:9`\n\n**nit:** Out-of-diff text of f2."));
    assert_eq!(writes[1]["call"], json!(post(GH_COMMENTS)));
    assert!(writes[2]["input"]["body"].as_str().unwrap().starts_with(
        "General note from me.\n\nme\n<!-- agent-review id=h1/c1 attempt=<attempt> -->"
    ));
    assert_eq!(dry["skipped"][0]["item"], "h2");
    assert_record(setup, writes[7]["input"]["body"].as_str().unwrap());
}

fn assert_gh_posted(setup: &Setup) {
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
    assert_never_posted(env);
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
    let status = env.run(&["status"]);
    assert_eq!(
        status["reviews_at_head"], 1,
        "the synced-back record is ours"
    );
    assert_eq!(status["runs"][0]["record"]["post"], "posted");
    assert_eq!(status["runs"][0]["record"]["remote_id"], "604");
}

#[test]
fn github_publish_posts_every_payload_once() {
    let setup = reviewed("git@github.com:me/repo.git");
    let env = &setup.env;
    gh_sync(env, None, &gh_before(&setup).render(&setup));
    env.run(&["sync", "--pr", "42"]);
    local_changes(&setup, "gh-t301", "gh-c900");

    let state = snapshot(&env.state());
    let calls = env.forge_calls().len();
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert_eq!(snapshot(&env.state()), state, "a dry run writes nothing");
    for call in &env.forge_calls()[calls..] {
        assert!(
            !call.contains("POST") && !call.contains("mutation"),
            "{call}"
        );
    }
    assert_eq!(
        env.forge_calls().len() - calls,
        4,
        "login and three queries"
    );
    assert_gh_dry_run(&setup, &dry);

    env.reset_calls();
    let (threads, done) = gh_after(&setup);
    gh_sync(env, Some(1), &gh_before(&setup).render(&setup));
    gh_sync(env, Some(2), &threads.render(&setup));
    gh_sync(env, None, &done.render(&setup));
    gh_writes(env, &setup);
    let published = env.run(&["publish", "--pr", "42"]);
    assert_eq!(
        kinds(&published, "posted"),
        [
            "review",
            "thread",
            "thread",
            "reply",
            "reply",
            "resolve",
            "unresolve",
            "record"
        ]
    );
    assert_gh_posted(&setup);

    let again = env.run(&["publish", "--pr", "42"]);
    assert_eq!(again["posted"], json!([]), "nothing is posted twice");
    assert_eq!(count_calls(env, &post(GH_REVIEWS)), 1);
    assert_eq!(count_calls(env, &post(GH_COMMENTS)), 4);
}

#[test]
fn publish_refuses_a_pr_by_someone_else() {
    let setup = reviewed("git@github.com:me/repo.git");
    let env = &setup.env;
    let mut pr = GhPr::new();
    pr.author = "alice";
    gh_sync(env, None, &pr.render(&setup));
    env.run(&["sync", "--pr", "42"]);
    env.run(&["run", "end", "--run", &setup.run]);
    let error = env.fail(&["publish", "--pr", "42"], None);
    assert!(error.contains("authored by alice, not me"), "{error}");
    assert!(env.forge_calls().iter().all(|call| !call.contains("POST")));
}

#[test]
fn not_own_publish_posts_only_replies_and_resolves_on_others_threads() {
    let setup = reviewed(GH);
    let env = &setup.env;
    let mut pr = GhPr::new();
    pr.author = "alice";
    pr.threads.push(gh_thread(
        "PRRT_a",
        false,
        3,
        vec![gh_comment(301, "alice", "Alice asks why.")],
        &setup.head,
    ));
    gh_sync(env, None, &pr.render(&setup));
    env.run(&["sync", "--pr", "42"]);
    env.run(&["comment", "--body", "My own note.", "--author", "me"]);
    env.run(&[
        "reply",
        "gh-t301",
        "--body",
        "Because of X.",
        "--author",
        "me",
    ]);
    env.run(&["resolve", "gh-t301", "--resolution", "answered"]);
    env.run(&["run", "end", "--run", &setup.run]);
    let dry = env.run(&["publish", "--pr", "42", "--not-own", "--dry-run"]);
    assert_eq!(kinds(&dry, "writes"), ["reply", "resolve"], "{dry}");
    let skipped: Vec<&str> = dry["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|skip| skip["item"].as_str().unwrap())
        .collect();
    for item in [
        setup.finding(1),
        "h1".to_owned(),
        format!("{}/record", setup.run),
    ] {
        assert!(skipped.contains(&item.as_str()), "{item}: {dry}");
    }
}

/// A write that reached GitHub but lost its response is matched by its marker on the next
/// run, never posted again.
#[test]
fn github_crash_after_write_does_not_duplicate() {
    let setup = reviewed("git@github.com:me/repo.git");
    let env = &setup.env;
    let (f1, f2) = (setup.finding(1), setup.finding(2));
    link(&setup);
    env.fail_call(&strs(&post(GH_REVIEWS)));
    env.fail(&["publish", "--pr", "42"], None);
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f1)["comments"][0]["post"], "posting");
    assert!(thread(&list, &f1)["comments"][0]["attempt"].is_string());

    let mut pr = GhPr::new();
    pr.threads.push(gh_thread(
        "PRRT_f1",
        false,
        3,
        vec![gh_comment(501, "me", &sent(env, &f1))],
        &setup.head,
    ));
    pr.review(&setup.head);
    gh_sync(env, None, &pr.render(&setup));
    env.fail_call(&strs(&post(GH_COMMENTS)));
    env.fail(&["publish", "--pr", "42"], None);
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f1)["comments"][0]["remote"]["id"], "501");
    assert_eq!(thread(&list, &f2)["comments"][0]["post"], "posting");

    pr.comments.push(gh_comment(601, "me", &sent(env, &f2)));
    gh_sync(env, None, &pr.render(&setup));
    env.pass_call(&strs(&post(GH_COMMENTS)));
    env.respond(&strs(&post(GH_COMMENTS)), r#"{"id":604}"#);
    let published = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&published, "posted"), ["record"]);
    assert_eq!(count_calls(env, &post(GH_REVIEWS)), 1);
    assert_eq!(
        count_calls(env, &post(GH_COMMENTS)),
        2,
        "f2 once, the record once"
    );
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f2)["comments"][0]["remote"]["id"], "601");
    assert_never_posted(env);
}

// GitLab

fn gl_note(id: u64, login: &str, body: &str) -> Value {
    json!({
        "id": id,
        "body": body,
        "author": { "username": login },
        "created_at": TIME,
        "system": false,
        "resolvable": true,
        "resolved": false,
    })
}

fn gl_discussion(
    id: &str,
    resolved: bool,
    line: Option<u32>,
    notes: Vec<Value>,
    setup: &Setup,
) -> Value {
    let notes: Vec<Value> = notes
        .into_iter()
        .map(|mut note| {
            note["resolved"] = json!(resolved);
            if let Some(line) = line {
                note["position"] = json!({
                    "base_sha": setup.base,
                    "start_sha": setup.base,
                    "head_sha": setup.head,
                    "old_path": "a.txt",
                    "new_path": "a.txt",
                    "new_line": line,
                    "old_line": null,
                });
            }
            note
        })
        .collect();
    json!({ "id": id, "individual_note": false, "notes": notes })
}

fn gl_general(id: &str, note: Value) -> Value {
    let mut note = note;
    note["resolvable"] = json!(false);
    json!({ "id": id, "individual_note": true, "notes": [note] })
}

fn gl_sync(env: &Env, setup: &Setup, discussions: &[Value], drafts: &[Value]) {
    env.respond(&["api", "-X", "GET", "user"], r#"{"username":"me"}"#);
    let vars = [
        ("HOST", "gitlab.com"),
        ("REPO", "me/proj"),
        ("HEAD", setup.head.as_str()),
        ("BASE", setup.base.as_str()),
    ];
    env.respond(&["api", "-X", "GET", GL_MR], &fixture("gl-mr.json", &vars));
    for (path, items) in [("discussions", discussions), ("draft_notes", drafts)] {
        env.respond(
            &[
                "api",
                "-X",
                "GET",
                "--paginate",
                &format!("{GL_MR}/{path}?per_page=100"),
            ],
            &json!(items).to_string(),
        );
    }
}

fn gl_before(setup: &Setup) -> Vec<Value> {
    vec![
        gl_discussion(
            "d-o",
            true,
            Some(5),
            vec![gl_note(4001, "alice", "Alice thread text.")],
            setup,
        ),
        gl_general("d-g", gl_note(4002, "alice", "Alice general text.")),
    ]
}

fn gl_posted(setup: &Setup) -> Vec<Value> {
    let f2 = setup.finding(2);
    let mut list = gl_before(setup);
    list.push(gl_discussion(
        "d-f1",
        false,
        Some(3),
        // A published draft returns no id; sync knows it by the text this store sent.
        vec![gl_note(5001, "me", "{{LAST_NOTE}}")],
        setup,
    ));
    list.push(gl_discussion(
        "d-f2",
        false,
        None,
        vec![gl_note(5002, "me", &marked("f2", &format!("{f2}/c1")))],
        setup,
    ));
    list
}

fn gl_done(setup: &Setup) -> Vec<Value> {
    let f1 = setup.finding(1);
    let mut list = gl_posted(setup);
    list.push(gl_discussion(
        "d-h1",
        false,
        None,
        vec![gl_note(5003, "me", &marked("h1", "h1/c1"))],
        setup,
    ));
    list[0] = gl_discussion(
        "d-o",
        false,
        Some(5),
        vec![gl_note(4001, "alice", "Alice thread text.")],
        setup,
    );
    // A reply turns an individual note into a discussion.
    list[1]["individual_note"] = json!(false);
    list[1]["notes"].as_array_mut().unwrap().push(gl_note(
        5012,
        "me",
        &marked("Thanks", "gl-n4002/c1"),
    ));
    list[2] = gl_discussion(
        "d-f1",
        true,
        Some(3),
        vec![
            gl_note(5001, "me", "{{LAST_NOTE}}"),
            gl_note(5011, "me", &marked("Fixed.", &format!("{f1}/c2"))),
        ],
        setup,
    );
    // GitLab shows no edits, so the record must read as this store sent it.
    list.push(gl_general("d-rec", gl_note(5020, "me", "{{LAST_BODY}}")));
    list
}

fn gl_writes(env: &Env) {
    env.respond(
        &strs(&gl_write("POST", &format!("{GL_MR}/draft_notes"))),
        r#"{"id":77}"#,
    );
    env.respond(
        &[
            "api",
            "-X",
            "PUT",
            &format!("{GL_MR}/draft_notes/77/publish"),
        ],
        "",
    );
    let discussions = gl_write("POST", &format!("{GL_MR}/discussions"));
    env.respond_nth(
        &strs(&discussions),
        1,
        r#"{"id":"d-f2","notes":[{"id":5002}]}"#,
    );
    env.respond_nth(
        &strs(&discussions),
        2,
        r#"{"id":"d-h1","notes":[{"id":5003}]}"#,
    );
    for (discussion, id) in [("d-f1", 5011), ("d-g", 5012)] {
        env.respond(
            &strs(&gl_write(
                "POST",
                &format!("{GL_MR}/discussions/{discussion}/notes"),
            )),
            &json!({ "id": id }).to_string(),
        );
    }
    for (discussion, resolved) in [("d-f1", true), ("d-o", false)] {
        env.respond(
            &strs(&gl_write(
                "PUT",
                &format!("{GL_MR}/discussions/{discussion}"),
            )),
            &json!({ "id": discussion, "resolved": resolved }).to_string(),
        );
    }
    env.respond(
        &strs(&gl_write("POST", &format!("{GL_MR}/notes"))),
        r#"{"id":5020}"#,
    );
}

fn input_of(inputs: &[(String, String)], call: &[String]) -> Vec<Value> {
    let wanted = call.join(" ");
    inputs
        .iter()
        .filter(|(args, _)| *args == wanted)
        .map(|(_, input)| serde_json::from_str(input).unwrap())
        .collect()
}

fn assert_gl_payloads(setup: &Setup) {
    let env = &setup.env;
    let inputs = env.forge_inputs();
    let draft = &input_of(&inputs, &gl_write("POST", &format!("{GL_MR}/draft_notes")))[0];
    assert_eq!(
        draft["position"],
        json!({
            "position_type": "text",
            "base_sha": setup.base,
            "start_sha": setup.base,
            "head_sha": setup.head,
            "old_path": "a.txt",
            "new_path": "a.txt",
            "new_line": 3,
        })
    );
    assert!(
        draft["note"]
            .as_str()
            .unwrap()
            .starts_with("**bug (high):** In-diff text of f1.")
    );
    let general = input_of(&inputs, &gl_write("POST", &format!("{GL_MR}/discussions")));
    assert!(
        general[0]["body"]
            .as_str()
            .unwrap()
            .starts_with("`a.txt:9`\n\n**nit:**")
    );
    assert_eq!(
        input_of(
            &inputs,
            &gl_write("PUT", &format!("{GL_MR}/discussions/d-o"))
        ),
        [json!({ "resolved": false })]
    );
    assert_eq!(
        input_of(
            &inputs,
            &gl_write("PUT", &format!("{GL_MR}/discussions/d-f1"))
        ),
        [json!({ "resolved": true })]
    );
    let record = &input_of(&inputs, &gl_write("POST", &format!("{GL_MR}/notes")))[0];
    assert_record(setup, record["body"].as_str().unwrap());
    assert_never_posted(env);
}

#[test]
fn gitlab_publish_posts_every_payload_once() {
    let setup = reviewed("git@gitlab.com:me/proj.git");
    let env = &setup.env;
    gl_sync(env, &setup, &gl_before(&setup), &[]);
    env.run(&["sync", "--pr", "3"]);
    local_changes(&setup, "gl-n4001", "gl-n4002");
    let state = snapshot(&env.state());
    let dry = env.run(&["publish", "--pr", "3", "--dry-run"]);
    assert_eq!(snapshot(&env.state()), state, "a dry run writes nothing");
    assert!(env.forge_calls().iter().all(|call| call.contains(" GET ")));
    assert_eq!(
        kinds(&dry, "writes"),
        [
            "draft",
            "publish-draft",
            "thread",
            "thread",
            "reply",
            "reply",
            "resolve",
            "unresolve",
            "record"
        ]
    );

    env.reset_calls();
    gl_sync(env, &setup, &gl_done(&setup), &[]);
    let mut posted = gl_posted(&setup);
    posted.push(gl_discussion(
        "d-h1",
        false,
        None,
        vec![gl_note(5003, "me", &marked("h1", "h1/c1"))],
        &setup,
    ));
    let discussions = [
        "api",
        "-X",
        "GET",
        "--paginate",
        &format!("{GL_MR}/discussions?per_page=100"),
    ];
    env.respond_nth(&discussions, 1, &json!(gl_before(&setup)).to_string());
    env.respond_nth(&discussions, 2, &json!(posted).to_string());
    gl_writes(env);
    let published = env.run(&["publish", "--pr", "3"]);
    assert_eq!(
        kinds(&published, "posted"),
        [
            "draft",
            "publish-draft",
            "thread",
            "thread",
            "reply",
            "reply",
            "resolve",
            "unresolve",
            "record"
        ]
    );
    assert_gl_payloads(&setup);
    let list = env.run(&["list"]);
    let f1 = thread(&list, &setup.finding(1));
    assert_eq!(f1["remote"]["id"], "d-f1");
    assert_eq!(f1["comments"][0]["draft_id"], Value::Null);
    assert_eq!(f1["comments"][1]["remote"]["id"], "5011");
    let status = env.run(&["status"]);
    assert_eq!(status["reviews_at_head"], 1);
    assert_eq!(status["runs"][0]["record"]["remote_id"], "5020");
}

/// A draft created without its id saved is published, not created again; a discussion
/// posted without its id saved is matched, not posted again.
#[test]
fn gitlab_crash_after_write_does_not_duplicate() {
    let setup = reviewed("git@gitlab.com:me/proj.git");
    let env = &setup.env;
    let (f1, f2) = (setup.finding(1), setup.finding(2));
    let drafts = gl_write("POST", &format!("{GL_MR}/draft_notes"));
    let discussions = gl_write("POST", &format!("{GL_MR}/discussions"));
    let publish = args(&[
        "api",
        "-X",
        "PUT",
        &format!("{GL_MR}/draft_notes/77/publish"),
    ]);
    gl_sync(env, &setup, &[], &[]);
    env.run(&["sync", "--pr", "3"]);
    env.run(&["run", "end", "--run", &setup.run]);
    env.fail_call(&strs(&drafts));
    env.fail(&["publish", "--pr", "3"], None);

    let draft = json!({ "id": 77, "note": sent(env, &f1) });
    gl_sync(env, &setup, &[], &[draft]);
    env.respond(&strs(&publish), "");
    env.fail_call(&strs(&discussions));
    env.fail(&["publish", "--pr", "3"], None);
    assert_eq!(count_calls(env, &drafts), 1);
    assert_eq!(count_calls(env, &publish), 1);
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f2)["comments"][0]["post"], "posting");

    let mut posted = gl_posted(&setup);
    posted.drain(..2);
    posted[1]["notes"][0]["body"] = json!(sent(env, &f2));
    gl_sync(env, &setup, &posted, &[]);
    env.respond(
        &strs(&gl_write("POST", &format!("{GL_MR}/notes"))),
        r#"{"id":5020}"#,
    );
    let published = env.run(&["publish", "--pr", "3"]);
    assert_eq!(kinds(&published, "posted"), ["record"]);
    assert_eq!(count_calls(env, &drafts), 1);
    assert_eq!(count_calls(env, &publish), 1);
    assert_eq!(count_calls(env, &discussions), 1);
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f1)["comments"][0]["remote"]["id"], "5001");
    assert_eq!(thread(&list, &f1)["comments"][0]["draft_id"], Value::Null);
    assert_eq!(thread(&list, &f2)["comments"][0]["remote"]["id"], "5002");
    assert_never_posted(env);
}

const GH: &str = "git@github.com:me/repo.git";
const GL: &str = "git@gitlab.com:me/proj.git";

fn ids(list: &Value) -> Vec<String> {
    list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["id"].as_str().unwrap().to_owned())
        .collect()
}

fn open_ids(status: &Value) -> Vec<String> {
    status["open_threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["id"].as_str().unwrap().to_owned())
        .collect()
}

fn writes_for(dry: &Value, item: &str) -> Vec<String> {
    dry["writes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|write| {
            write["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == item)
        })
        .map(|write| write["kind"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn gitlab_reply_on_general_note_keeps_one_thread() {
    let setup = reviewed(GL);
    let env = &setup.env;
    let note = gl_note(4002, "alice", "Alice general text.");
    gl_sync(env, &setup, &[gl_general("d-g", note)], &[]);
    env.run(&["sync", "--pr", "3"]);
    let mut replied = gl_general("d-g", gl_note(4002, "alice", "Alice general text."));
    replied["individual_note"] = json!(false);
    replied["notes"]
        .as_array_mut()
        .unwrap()
        .push(gl_note(4003, "bob", "Bob reply text."));
    gl_sync(env, &setup, &[replied], &[]);
    env.run(&["sync", "--pr", "3"]);
    let list = env.run(&["list"]);
    let same: Vec<String> = ids(&list)
        .into_iter()
        .filter(|id| id.starts_with("gl-n4002"))
        .collect();
    assert_eq!(same, ["gl-n4002"], "one thread per discussion");
    assert_eq!(
        thread(&list, "gl-n4002")["comments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn own_published_general_thread_stays_open() {
    let setup = reviewed(GH);
    let env = &setup.env;
    env.run(&[
        "comment",
        "--body",
        "General note from me.",
        "--author",
        "me",
    ]);
    let mut pr = gh_before(&setup);
    pr.comments.push(gh_comment(
        602,
        "me",
        &marked("General note from me.", "h1/c1"),
    ));
    gh_sync(env, None, &pr.render(&setup));
    env.run(&["sync", "--pr", "42"]);
    let open = open_ids(&env.run(&["status"]));
    assert!(open.contains(&"h1".to_owned()), "{open:?}");
    assert!(!open.contains(&"gh-c900".to_owned()), "{open:?}");
    let listed = ids(&env.run(&["list", "--open"]));
    assert_eq!(listed, open, "list --open and status agree");
}

#[test]
fn own_review_body_is_not_imported() {
    let setup = reviewed(GH);
    let env = &setup.env;
    let mut pr = GhPr::new();
    pr.review(&setup.head);
    gh_sync(env, None, &pr.render(&setup));
    env.run(&["sync", "--pr", "42"]);
    assert!(!ids(&env.run(&["list"])).contains(&"gh-r700".to_owned()));
}

#[test]
fn finding_kill_refuses_a_posting_finding() {
    let setup = reviewed(GH);
    let env = &setup.env;
    link(&setup);
    env.fail_call(&strs(&post(GH_REVIEWS)));
    env.fail(&["publish", "--pr", "42"], None);
    let error = env.fail(
        &["finding", "kill", &setup.finding(1), "--reason", "late"],
        None,
    );
    assert!(error.contains("resolve"), "{error}");
}

#[test]
fn sync_never_reuses_a_killed_finding_id() {
    let setup = reviewed(GH);
    let env = &setup.env;
    let f2 = setup.finding(2);
    env.run(&["finding", "kill", &f2, "--reason", "wrong"]);
    let mut pr = GhPr::new();
    let body = format!(
        "**nit:** x\n<!-- review-finding v1 id={f2} sha={} -->",
        setup.head
    );
    pr.comments.push(gh_comment(601, "me", &body));
    gh_sync(env, None, &pr.render(&setup));
    env.run(&["sync", "--pr", "42"]);
    let list = env.run(&["list"]);
    assert!(!ids(&list).contains(&f2), "{list}");
    assert!(ids(&list).contains(&"gh-c601".to_owned()));
}

#[test]
fn comments_before_first_sync_are_mine() {
    let setup = reviewed(GH);
    let env = &setup.env;
    env.run(&["comment", "--body", "Written before sync."]);
    link(&setup);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert_eq!(dry["skipped"], json!([]), "{dry}");
    assert_eq!(writes_for(&dry, "h1/c1"), ["thread"]);
    env.run(&["sync", "--pr", "42"]);
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, "h1")["comments"][0]["author"]["login"], "me");
}

#[test]
fn target_without_local_branch_uses_pr_head() {
    let setup = reviewed(GH);
    let env = &setup.env;
    gh_sync(env, None, &GhPr::new().render(&setup));
    env.run(&["sync", "--pr", "42"]);
    env.git(&["switch", "-q", "--detach", "main"]);
    env.git(&["branch", "-q", "-D", "feat"]);
    let status = env.run(&["--target", "gh-42", "status"]);
    assert_eq!(status["head"], setup.head);
}

#[test]
fn publish_target_on_detached_head() {
    let setup = reviewed(GH);
    let env = &setup.env;
    link(&setup);
    env.run(&["sync", "--pr", "42"]);
    env.git(&["switch", "-q", "--detach"]);
    let dry = env.run(&["--target", "gh-42", "publish", "--dry-run"]);
    assert_eq!(kinds(&dry, "writes"), ["review", "thread", "record"]);
}

#[test]
fn github_reply_on_general_finding_posts_re_answer() {
    let setup = reviewed(GH);
    let env = &setup.env;
    let (f1, f2) = (setup.finding(1), setup.finding(2));
    link(&setup);
    // Both findings land with lost responses, so sync knows them by their markers.
    env.fail_call(&strs(&post(GH_REVIEWS)));
    env.fail(&["publish", "--pr", "42"], None);
    let mut pr = GhPr::new();
    pr.threads.push(gh_thread(
        "PRRT_f1",
        false,
        3,
        vec![gh_comment(501, "me", &sent(env, &f1))],
        &setup.head,
    ));
    pr.review(&setup.head);
    gh_sync(env, None, &pr.render(&setup));
    env.fail_call(&strs(&post(GH_COMMENTS)));
    env.fail(&["publish", "--pr", "42"], None);
    env.pass_call(&strs(&post(GH_COMMENTS)));
    pr.comments.push(gh_comment(601, "me", &sent(env, &f2)));
    gh_sync(env, None, &pr.render(&setup));
    env.run(&["sync", "--pr", "42"]);
    env.run(&[
        "reply",
        &f2,
        "--body",
        "Not fixed: needs a design change.",
        "--author",
        "me",
    ]);

    env.reset_calls();
    gh_sync(env, Some(1), &pr.render(&setup));
    gh_sync(env, Some(2), &pr.render(&setup));
    let answer = format!(
        "{}\nRe: {f2}",
        marked(
            "Not fixed: needs a design change.\n\nme",
            &format!("{f2}/c2")
        )
    );
    pr.comments.push(gh_comment(603, "me", &answer));
    gh_sync(env, None, &pr.render(&setup));
    env.respond_nth(&strs(&post(GH_COMMENTS)), 1, r#"{"id":603}"#);
    env.respond_nth(&strs(&post(GH_COMMENTS)), 2, r#"{"id":604}"#);
    let published = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&published, "posted"), ["reply", "record"]);
    let inputs = env.forge_inputs();
    let (_, posted) = inputs
        .iter()
        .find(|(_, input)| input.contains("Not fixed"))
        .unwrap();
    let posted: Value = serde_json::from_str(posted).unwrap();
    let posted = posted["body"].as_str().unwrap();
    assert!(
        posted.starts_with(&format!(
            "Not fixed: needs a design change.\n\nme\n<!-- agent-review id={f2}/c2 attempt=at-"
        )),
        "{posted}"
    );
    assert!(posted.ends_with(&format!(" -->\nRe: {f2}")), "{posted}");
    let list = env.run(&["list"]);
    let general = thread(&list, &f2);
    assert_eq!(general["comments"].as_array().unwrap().len(), 2);
    assert_eq!(general["comments"][1]["remote"]["id"], "603");
    assert!(!ids(&list).contains(&"gh-c603".to_owned()));
}

#[test]
fn locked_pass_findings_wait_for_reveal() {
    let setup = reviewed_with(GH, true);
    let env = &setup.env;
    gh_sync(env, None, &GhPr::new().render(&setup));
    let held = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert_eq!(
        writes_for(&held, &format!("{}/c1", setup.finding(1))),
        Vec::<String>::new()
    );
    assert!(
        held["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("reveal"),
        "{held}"
    );
    let reveal = [
        "pass",
        "start",
        "--run",
        &setup.run,
        "--kind",
        "reveal",
        "--reviewers",
        "-",
    ];
    env.run_stdin(&reveal, REVIEWER);
    env.run(&["pass", "end", "--run", &setup.run, "--pass", "c1p1"]);
    let shown = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert_eq!(
        writes_for(&shown, &format!("{}/c1", setup.finding(1))),
        ["review"]
    );
}

#[test]
fn rejected_finding_returns_to_pending_and_can_be_killed() {
    let setup = reviewed(GH);
    let env = &setup.env;
    let f1 = setup.finding(1);
    link(&setup);
    env.fail_call(&strs(&post(GH_REVIEWS)));
    env.fail(&["publish", "--pr", "42"], None);
    env.fail(&["publish", "--pr", "42"], None);
    env.run(&["sync", "--pr", "42"]);
    let list = env.run(&["list"]);
    let first = &thread(&list, &f1)["comments"][0];
    assert_eq!(first["post"], "pending", "no marker found: back to pending");
    assert!(first["attempt"].is_string(), "the attempt id stays");
    env.run(&["finding", "kill", &f1, "--reason", "the host rejects it"]);
}

#[test]
fn stale_unposted_findings_are_not_posted() {
    let setup = reviewed(GH);
    let env = &setup.env;
    let f2 = setup.finding(2);
    env.run(&["resolve", &f2, "--triage", "stale"]);
    link(&setup);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert_eq!(writes_for(&dry, &format!("{f2}/c1")), Vec::<String>::new());
    assert_eq!(kinds(&dry, "writes"), ["review", "record"]);
}

#[test]
fn run_end_releases_locked_findings() {
    let setup = reviewed_with(GH, true);
    let env = &setup.env;
    link(&setup);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert_eq!(kinds(&dry, "writes"), ["review", "thread", "record"]);
}

#[test]
fn my_unmarked_comments_are_not_open() {
    let setup = reviewed(GH);
    let env = &setup.env;
    let mut pr = GhPr::new();
    pr.comments.push(gh_comment(950, "me", "PTAL"));
    pr.reviews.push(json!({
        "databaseId": 751,
        "url": "https://github.com/me/repo/pull/42#pullrequestreview-751",
        "body": "Looked at the retry path.",
        "state": "COMMENTED",
        "submittedAt": TIME,
        "author": { "login": "me" },
        "editor": null,
    }));
    gh_sync(env, None, &pr.render(&setup));
    env.run(&["sync", "--pr", "42"]);
    env.run(&["comment", "--body", "Local note.", "--author", "me"]);
    let open = open_ids(&env.run(&["status"]));
    assert!(!open.contains(&"gh-c950".to_owned()), "{open:?}");
    assert!(!open.contains(&"gh-r751".to_owned()), "{open:?}");
    assert!(open.contains(&"h1".to_owned()), "{open:?}");
}

#[test]
fn target_on_detached_rebased_pr_head_uses_head() {
    let setup = reviewed(GH);
    let env = &setup.env;
    gh_sync(env, None, &GhPr::new().render(&setup));
    env.run(&["sync", "--pr", "42"]);
    env.git(&["switch", "-q", "--detach", "main"]);
    env.write("a.txt", &numbered(10, &[(3, "THREE")]));
    let pushed = env.commit_all("force-pushed PR head");
    let mut pr = GhPr::new();
    pr.head = Some(pushed);
    gh_sync(env, None, &pr.render(&setup));
    env.run(&["--target", "gh-42", "sync"]);
    env.write("a.txt", &numbered(10, &[(3, "THREE"), (5, "five")]));
    let fix = env.commit_all("fix on the pushed head");
    assert_eq!(env.run(&["--target", "gh-42", "status"])["head"], fix);
}

#[test]
fn sync_from_detached_head_adopts_the_branch_target() {
    let setup = reviewed(GH);
    let env = &setup.env;
    let branch_key = env.run(&["status"])["target"].as_str().unwrap().to_owned();
    env.git(&["switch", "-q", "--detach"]);
    gh_sync(env, None, &GhPr::new().render(&setup));
    let synced = env.run(&["sync", "--pr", "42"]);
    assert_eq!(synced["target"], "gh-42");
    assert_eq!(synced["renamed_from"], branch_key);
    let list = env.run(&["--target", "gh-42", "list"]);
    assert!(ids(&list).contains(&setup.finding(1)), "{list}");
}

#[test]
fn foreign_marker_never_claims_a_pending_comment() {
    let setup = reviewed(GH);
    let env = &setup.env;
    env.run(&["comment", "--body", "Local pending note.", "--author", "me"]);
    let mut pr = GhPr::new();
    pr.comments
        .push(gh_comment(950, "me", &marked("Other store text.", "h1/c1")));
    gh_sync(env, None, &pr.render(&setup));
    env.run(&["sync", "--pr", "42"]);
    let list = env.run(&["list"]);
    let local = &thread(&list, "h1")["comments"][0];
    assert_eq!(local["body"], "Local pending note.");
    assert_eq!(local["post"], "pending");
    assert_eq!(local["remote"], Value::Null);
    assert!(ids(&list).contains(&"gh-c950".to_owned()), "{list}");
}

#[test]
fn closed_unrelated_pr_is_not_attached_or_pruned() {
    let env = Env::with_branch();
    env.git(&["remote", "add", "origin", GH]);
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let base = env.git(&["rev-parse", "main"]);
    let branch_key = env.run(&["run", "start"])["target"]
        .as_str()
        .unwrap()
        .to_owned();
    env.respond(&["api", "--method", "GET", "user"], r#"{"login":"me"}"#);
    env.respond(
        &[
            "api",
            "--method",
            "GET",
            "repos/me/repo/pulls?head=me%3Afeat&state=all&per_page=20",
        ],
        r#"[{"number":5,"state":"closed"}]"#,
    );
    let vars = [
        ("HEAD", "1111111111111111111111111111111111111111"),
        ("BASE", base.as_str()),
        ("REPO", "me/repo"),
        ("NUMBER", "5"),
        ("STATE", "CLOSED"),
        ("CLOSED", "\"2020-01-01T00:00:00Z\""),
    ];
    for query in ["PullRequest", "PullReviews", "PullComments"] {
        let args = [
            "api",
            "graphql",
            "-f",
            "owner=me",
            "-f",
            "name=repo",
            "-F",
            "number=5",
            "-f",
            &format!("query={query}"),
        ];
        env.respond(&args, &fixture("gh-empty-pr.json", &vars));
    }
    let synced = env.run(&["sync"]);
    assert_eq!(synced["target"], "gh-5");
    assert_eq!(synced["pruned"], json!([]), "synced just now");
    let status = env.run(&["status"]);
    assert_eq!(status["target"], branch_key, "the branch target stays");
    assert_eq!(status["runs"].as_array().unwrap().len(), 1);
    assert_eq!(status["review"], Value::Null);
}

#[test]
fn record_counts_reviews_like_status() {
    let env = Env::with_branch();
    env.git(&["remote", "add", "origin", GH]);
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("C0");
    let base = env.git(&["rev-parse", "main"]);
    env.write("a.txt", &numbered(10, &[(3, "three"), (6, "six")]));
    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let pass = [
        "pass",
        "start",
        "--run",
        &run,
        "--kind",
        "full",
        "--reviewers",
        "-",
    ];
    env.run_stdin(&pass, REVIEWER);
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    let head = env.commit_all("commit exactly the reviewed tree");
    let setup = Setup {
        env,
        run,
        head,
        base,
    };
    link(&setup);
    assert_eq!(setup.env.run(&["status"])["reviews_at_head"], 1);
    let dry = setup.env.run(&["publish", "--pr", "42", "--dry-run"]);
    let record = dry["writes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|write| write["kind"] == "record")
        .unwrap()["input"]["body"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        record.starts_with(&format!(
            "**Review {}:** reviewed at {}.",
            setup.run,
            &setup.head[..7]
        )),
        "{record}"
    );
    assert!(record.contains("reviews-at-head=1 "), "{record}");
}

#[test]
fn gitlab_records_need_a_body_this_store_sent() {
    let setup = reviewed(GL);
    let env = &setup.env;
    let record = format!(
        "<!-- review-record v2 id=rv-zzzz head={} reviews-at-head=2 open=0 -->\n<!-- agent-review id=rv-zzzz/record -->",
        setup.head
    );
    let mut note = gl_note(6001, "me", &record);
    note["resolvable"] = json!(false);
    gl_sync(
        env,
        &setup,
        &[json!({ "id": "d-r", "individual_note": true, "notes": [note] })],
        &[],
    );
    let synced = env.run(&["sync", "--pr", "3"]);
    assert_eq!(
        synced["records"], 0,
        "GitLab shows no edits, so only sent text is trusted"
    );
    assert_eq!(env.run(&["status"])["imported_records"], json!([]));
}

#[test]
fn rename_merge_keeps_colliding_human_threads() {
    let env = Env::with_branch();
    env.git(&["remote", "add", "origin", GH]);
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let head = env.commit_all("change");
    let base = env.git(&["rev-parse", "main"]);
    let setup = Setup {
        env,
        run: String::new(),
        head,
        base,
    };
    let env = &setup.env;
    gh_sync(env, None, &GhPr::new().render(&setup));
    env.git(&["switch", "-q", "--detach"]);
    env.run(&["sync", "--pr", "42"]);
    env.run(&[
        "--target",
        "gh-42",
        "comment",
        "--body",
        "Detached note.",
        "--author",
        "me",
    ]);
    env.git(&["switch", "-q", "feat"]);
    env.run(&["comment", "--body", "Branch note.", "--author", "me"]);
    env.run(&["sync", "--pr", "42"]);
    let list = env.run(&["--target", "gh-42", "list"]);
    let mut texts: Vec<&str> = list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["comments"][0]["body"].as_str().unwrap())
        .collect();
    texts.sort_unstable();
    assert_eq!(texts, ["Branch note.", "Detached note."], "{list}");
}

#[test]
fn runs_ended_before_the_pr_are_never_published() {
    let setup = reviewed(GH);
    let env = &setup.env;
    let (f1, f2) = (setup.finding(1), setup.finding(2));
    env.run(&[
        "resolve",
        &f1,
        "--resolution",
        &format!("fixed {}", setup.head),
    ]);
    env.run(&["run", "end", "--run", &setup.run]);
    let later = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    gh_sync(env, None, &GhPr::new().render(&setup));
    env.run_with(
        &["sync", "--pr", "42"],
        None,
        &[("AGENT_REVIEW_NOW", &later.to_string())],
    );
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert_eq!(dry["writes"], json!([]), "{dry}");
    let skipped: Vec<&str> = dry["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|skip| skip["item"].as_str().unwrap())
        .collect();
    assert!(
        skipped.contains(&f1.as_str()) && skipped.contains(&f2.as_str()),
        "{dry}"
    );
}

#[test]
fn local_reply_is_never_published() {
    let setup = reviewed(GH);
    let env = &setup.env;
    let f1 = setup.finding(1);
    link(&setup);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert!(writes_for(&dry, &format!("{f1}/c1")).len() == 1, "{dry}");
    env.run(&[
        "reply",
        &f1,
        "--local",
        "--body",
        "Answer to the held question.",
        "--author",
        "me",
    ]);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert_eq!(
        writes_for(&dry, &format!("{f1}/c2")),
        Vec::<String>::new(),
        "{dry}"
    );
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f1)["comments"][1]["local"], true);
    let prompt = env.text(&["list", "--format", "prompt"]);
    assert!(prompt.contains("Answer to the held question."), "{prompt}");
}

#[test]
fn not_own_findings_posts_findings_and_my_threads_but_no_record() {
    let setup = reviewed(GH);
    let env = &setup.env;
    let mut pr = GhPr::new();
    pr.author = "alice";
    gh_sync(env, None, &pr.render(&setup));
    env.run(&["sync", "--pr", "42"]);
    env.run(&["comment", "--body", "My own note.", "--author", "me"]);
    env.run(&["run", "end", "--run", &setup.run]);
    let only = format!("{},h1", setup.finding(1));
    let dry = env.run(&[
        "publish",
        "--pr",
        "42",
        "--not-own",
        "--findings",
        "--only",
        &only,
        "--dry-run",
    ]);
    assert_eq!(kinds(&dry, "writes"), ["review", "thread"], "{dry}");
    assert_eq!(writes_for(&dry, "h1/c1"), ["thread"]);
    let skipped = dry["skipped"].to_string();
    assert!(skipped.contains(&format!("{}/record", setup.run)), "{dry}");
}

#[test]
fn only_limits_publish_to_named_threads() {
    let setup = reviewed(GH);
    let env = &setup.env;
    let mut pr = GhPr::new();
    pr.author = "alice";
    gh_sync(env, None, &pr.render(&setup));
    env.run(&["sync", "--pr", "42"]);
    env.run(&["run", "end", "--run", &setup.run]);
    let second = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let pass = [
        "pass",
        "start",
        "--run",
        &second,
        "--kind",
        "full",
        "--reviewers",
        "-",
    ];
    env.run_stdin(&pass, REVIEWER);
    let add = [
        "finding",
        "add",
        "--run",
        &second,
        "--pass",
        "c1p0",
        "--reviewer",
        "r1",
    ];
    env.run_stdin(&add, FINDINGS);
    env.run(&["pass", "end", "--run", &second, "--pass", "c1p0"]);
    let wanted = format!("{second}.c1p0.r1.f1");

    let error = env.fail(
        &[
            "publish",
            "--pr",
            "42",
            "--not-own",
            "--findings",
            "--dry-run",
        ],
        None,
    );
    assert!(error.contains("--only"), "{error}");
    let dry = env.run(&[
        "publish",
        "--pr",
        "42",
        "--not-own",
        "--findings",
        "--only",
        &wanted,
        "--dry-run",
    ]);
    let items: Vec<String> = dry["writes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|write| write["items"].as_array().unwrap().clone())
        .map(|item| item.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(items, [format!("{wanted}/c1")], "{dry}");
    let skipped = dry["skipped"].to_string();
    assert!(skipped.contains(&setup.finding(1)), "{dry}");
}
