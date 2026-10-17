use serde_json::{Value, json};

use crate::support::{Env, fixture, numbered};

const REVIEWER: &str = r#"[{"model":"m1","effort":"xhigh","harness":"codex"}]"#;
const FINDINGS: &str = r#"[
  {"type":"bug","severity":"high","path":"a.txt","line":3,"scenario":"Local text of f1."},
  {"type":"bug","severity":"low","path":"a.txt","line":3,"scenario":"Local text of f2."}
]"#;

struct Setup {
    env: Env,
    run: String,
    head: String,
    base: String,
}

/// A `feat` branch with one reviewed commit, two local findings and an ended run.
fn reviewed_branch(origin: &str) -> Setup {
    let env = Env::with_branch();
    env.git(&["remote", "add", "origin", origin]);
    env.write("b.txt", "b\n");
    env.commit_all("old");
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let head = env.commit_all("change");
    let base = env.git(&["rev-parse", "main"]);
    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    env.run_stdin(
        &[
            "pass",
            "start",
            "--run",
            &run,
            "--kind",
            "full",
            "--reviewers",
            "-",
        ],
        REVIEWER,
    );
    env.run_stdin(
        &[
            "finding",
            "add",
            "--run",
            &run,
            "--pass",
            "c1p0",
            "--reviewer",
            "r1",
        ],
        FINDINGS,
    );
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    env.run(&["run", "end", "--run", &run]);
    Setup {
        env,
        run,
        head,
        base,
    }
}

fn graphql(owner: &str, name: &str, number: &str, query: &str) -> Vec<String> {
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

fn respond(env: &Env, args: &[String], body: &str) {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    env.respond(&args, body);
}

fn with_cursor(mut args: Vec<String>, cursor: &str) -> Vec<String> {
    args.extend(["-f".to_owned(), format!("cursor={cursor}")]);
    args
}

fn github_responses(setup: &Setup) {
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
    respond(env, &pull, &fixture("gh-threads-1.json", &vars));
    respond(
        env,
        &with_cursor(pull, "t-2"),
        &fixture("gh-threads-2.json", &vars),
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
        &fixture("gh-thread-comments.json", &vars),
    );
    respond(
        env,
        &graphql("me", "repo", "42", "PullReviews"),
        &fixture("gh-reviews.json", &vars),
    );
    let comments = graphql("me", "repo", "42", "PullComments");
    respond(env, &comments, &fixture("gh-comments-1.json", &vars));
    respond(
        env,
        &with_cursor(comments, "ic-2"),
        &fixture("gh-comments-2.json", &vars),
    );
}

fn thread<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["id"] == id)
        .unwrap_or_else(|| panic!("no thread {id} in {list}"))
}

fn ids(list: &Value) -> Vec<String> {
    list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Every forge call was a GET or a GraphQL query; the fake refuses mutations and unknown calls.
fn assert_read_only(env: &Env) {
    let calls = env.forge_calls();
    assert!(!calls.is_empty());
    for call in calls {
        let args = call.split('\t').nth(1).unwrap();
        let read = args.starts_with("api --method GET ")
            || args.starts_with("api -X GET ")
            || (args.starts_with("api graphql ") && args.contains("query="));
        assert!(read, "unexpected forge call: {call}");
        assert!(!args.contains("mutation"), "{call}");
    }
}

fn assert_imported_list(setup: &Setup) {
    let env = &setup.env;
    let list = env.run(&["list"]);
    let general = thread(&list, &format!("{}.c1p0.r1.f5", setup.run));
    assert_eq!(general["resolved"], true);
    assert_eq!(general["resolution"], "fixed abc1234");
    assert_eq!(general["severity"], "low");
    assert_eq!(general["comments"].as_array().unwrap().len(), 2);
    assert_eq!(
        thread(&list, "gh-r5401")["remote"]["review_state"],
        "APPROVED"
    );
    assert_eq!(thread(&list, "gh-t1301")["current"]["outdated"], true);
    let all = ids(&list);
    assert!(!all.contains(&"gh-r5001".to_owned()) && !all.contains(&"gh-r5402".to_owned()));
    assert!(
        all.contains(&"gh-c2002".to_owned()),
        "untrusted record stays a comment"
    );
}

#[test]
fn github_sync_imports_threads_reviews_and_comments() {
    let setup = reviewed_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    let branch_key = env.run(&["status", "--full"])["target"].clone();
    github_responses(&setup);

    let synced = env.run(&["sync"]);
    assert_eq!(synced["target"], "gh-42");
    assert_eq!(synced["renamed_from"], branch_key);
    assert_eq!(synced["login"], "me");
    assert_eq!(
        synced["review"]["url"],
        "https://github.com/me/repo/pull/42"
    );
    assert_eq!(synced["records"], 1);
    assert_eq!(synced["answered"], 1);
    assert_read_only(env);

    let status = env.run(&["status", "--full"]);
    assert_eq!(status["target"], "gh-42");
    assert_eq!(status["runs"][0]["id"], setup.run.as_str());
    assert_eq!(
        status["reviews_at_head"], 3,
        "1 local full pass + trusted record of 2"
    );
    assert_eq!(status["imported_records"].as_array().unwrap().len(), 1);
    assert_eq!(status["outdated_threads"], json!(["gh-t1301"]));
    let open: Vec<&str> = status["open_threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["id"].as_str().unwrap())
        .collect();
    assert!(
        !open.contains(&"gh-c2005"),
        "general comments are not open items: {open:?}"
    );
    assert!(open.contains(&"gh-t1101"), "{open:?}");
    assert!(!env.state().join("local").exists());

    assert_imported_list(&setup);

    let again = env.run(&["sync"]);
    assert_eq!(again["threads"]["imported"], 0);
    assert_eq!(again["threads"]["total"], synced["threads"]["total"]);
    assert!(again["renamed_from"].is_null());
}

#[test]
fn github_sync_remote_state_wins_and_local_fields_stay() {
    let setup = reviewed_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    let f1 = format!("{}.c1p0.r1.f1", setup.run);
    env.run(&["resolve", &f1, "--triage", "valid"]);
    github_responses(&setup);
    env.run(&["sync"]);

    let shown = &env.run(&["show", &f1])["thread"];
    assert_eq!(shown["resolved"], true, "remote resolved state wins");
    assert_eq!(shown["severity"], "high");
    assert_eq!(shown["triage"], "valid");
    assert_eq!(shown["anchor"]["line"], 3);
    let comments = shown["comments"].as_array().unwrap();
    assert_eq!(
        comments.len(),
        3,
        "first page, second page of inner comments"
    );
    assert_eq!(comments[0]["author"]["kind"], "agent", "local author kept");
    assert_eq!(
        comments[0]["body"].as_str().unwrap(),
        fixture_first_body(&setup)
    );
    assert_eq!(comments[0]["post"], "posted");
    assert_eq!(comments[0]["remote"]["id"], "1001");
    assert_eq!(comments[2]["remote"]["is_me"], false);
    let f2 = &env.run(&["show", &format!("{}.c1p0.r1.f2", setup.run)])["thread"];
    assert_eq!(f2["comments"][0]["post"], "pending");
}

fn fixture_first_body(setup: &Setup) -> String {
    let page = fixture(
        "gh-threads-1.json",
        &[
            ("RUN", &setup.run),
            ("HEAD", &setup.head),
            ("BASE", &setup.base),
            ("REPO", "me/repo"),
            ("NUMBER", "42"),
        ],
    );
    let page: Value = serde_json::from_str(&page).unwrap();
    page["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]["nodes"][0]
        ["body"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn github_sync_ignores_untrusted_markers() {
    let setup = reviewed_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    github_responses(&setup);
    env.run(&["sync"]);
    let list = env.run(&["list"]);
    let all = ids(&list);
    for number in ["f9", "f8", "f7"] {
        let id = format!("{}.c1p0.r1.{number}", setup.run);
        assert!(!all.contains(&id), "{id} must not be trusted: {all:?}");
    }
    for imported in ["gh-t1101", "gh-t1201", "gh-t1301"] {
        let found = thread(&list, imported);
        assert!(found["finding"].is_null(), "{found}");
        assert_eq!(found["type"], "comment");
    }
}

#[test]
fn prompt_hides_other_authors_and_author_defaults_to_login() {
    let setup = reviewed_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    github_responses(&setup);
    env.run(&["sync"]);

    let prompt = env.text(&["list", "--format", "prompt"]);
    for hidden in [
        "Bob agrees",
        "Alice asks",
        "Carol asks",
        "Bob approves",
        "Copied marker",
    ] {
        assert!(!prompt.contains(hidden), "{hidden} leaked:\n{prompt}");
    }
    assert!(prompt.contains("Posted text of the finding."), "{prompt}");
    assert!(
        prompt.contains("(comment by another author, text hidden)"),
        "{prompt}"
    );
    assert!(
        prompt.contains(
            "- gh-t1301 [comment] a.txt:5 (outdated) (open)\n  URL: https://github.com/me/repo/pull/42#discussion_r1301\n"
        ),
        "{prompt}"
    );

    let comment = env.run(&["comment", "--body", "x"]);
    assert_eq!(comment["thread"]["comments"][0]["author"]["login"], "me");
    assert_eq!(comment["thread"]["comments"][0]["post"], "pending");
}

#[test]
fn github_fork_pr_found_in_parent() {
    let setup = reviewed_branch("https://github.com/me/repo.git");
    let env = &setup.env;
    env.respond(&["api", "--method", "GET", "user"], r#"{"login":"me"}"#);
    let pulls = |repo: &str| format!("repos/{repo}/pulls?head=me%3Afeat&state=all&per_page=20");
    env.respond(&["api", "--method", "GET", &pulls("me/repo")], "[]");
    env.respond(
        &["api", "--method", "GET", "repos/me/repo"],
        r#"{"fork":true,"parent":{"full_name":"up/repo"}}"#,
    );
    env.respond(
        &["api", "--method", "GET", &pulls("up/repo")],
        r#"[{"number":7,"state":"closed"},{"number":8,"state":"open"}]"#,
    );
    let vars = [
        ("HEAD", setup.head.as_str()),
        ("BASE", setup.base.as_str()),
        ("REPO", "up/repo"),
        ("NUMBER", "8"),
        ("STATE", "OPEN"),
        ("CLOSED", "null"),
    ];
    for query in ["PullRequest", "PullReviews", "PullComments"] {
        respond(
            env,
            &graphql("up", "repo", "8", query),
            &fixture("gh-empty-pr.json", &vars),
        );
    }
    let synced = env.run(&["sync"]);
    assert_eq!(synced["target"], "gh-8");
    assert_eq!(synced["review"]["repo"], "up/repo");
    assert_eq!(env.run(&["status"])["target"], "gh-8");
    assert!(env.state().join("github.com/me/repo/gh-8.json").exists());
    assert_read_only(env);
}

#[test]
fn prune_ninety_days_after_close() {
    let setup = reviewed_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    env.respond(&["api", "--method", "GET", "user"], r#"{"login":"me"}"#);
    let vars = [
        ("HEAD", setup.head.as_str()),
        ("BASE", setup.base.as_str()),
        ("REPO", "me/repo"),
        ("NUMBER", "5"),
        ("STATE", "MERGED"),
        ("CLOSED", "\"2026-01-01T00:00:00Z\""),
    ];
    for query in ["PullRequest", "PullReviews", "PullComments"] {
        respond(
            env,
            &graphql("me", "repo", "5", query),
            &fixture("gh-empty-pr.json", &vars),
        );
    }
    let closed: u64 = 1_767_225_600;
    let day = 86_400;
    let at = |days: u64| (closed + days * day).to_string();
    let synced = env.run_with(
        &["sync", "--pr", "5"],
        None,
        &[("AGENT_REVIEW_NOW", &at(59))],
    );
    assert_eq!(synced["review"]["state"], "merged");
    assert_eq!(synced["review"]["closed_at"], "2026-01-01T00:00:00Z");

    // The sync on day 59 is the later change, so the 90 days count from it.
    let kept = env.run_with(&["prune"], None, &[("AGENT_REVIEW_NOW", &at(59 + 89))]);
    assert_eq!(kept["pruned"], json!([]));
    let pruned = env.run_with(&["prune"], None, &[("AGENT_REVIEW_NOW", &at(59 + 91))]);
    assert_eq!(
        pruned["pruned"][0]["target"], "gh-5",
        "91 days after the last change"
    );
}

fn gitlab_responses(setup: &Setup, old: &str) {
    let env = &setup.env;
    let project = "projects/group%2Fsub%2Fproj";
    let vars = [
        ("RUN", setup.run.as_str()),
        ("HEAD", setup.head.as_str()),
        ("BASE", setup.base.as_str()),
        ("OLD", old),
        ("HOST", "gitlab.example.com"),
        ("REPO", "group/sub/proj"),
    ];
    env.respond(&["api", "-X", "GET", "user"], r#"{"username":"me"}"#);
    env.respond(
        &[
            "api",
            "-X",
            "GET",
            &format!("{project}/merge_requests?source_branch=feat&state=all&per_page=20"),
        ],
        r#"[{"iid":9,"state":"opened","source_project_id":555},{"iid":3,"state":"opened","source_project_id":87447936}]"#,
    );
    env.respond(&["api", "-X", "GET", project], r#"{"id":87447936}"#);
    let mr = format!("{project}/merge_requests/3");
    env.respond(&["api", "-X", "GET", &mr], &fixture("gl-mr.json", &vars));
    env.respond(
        &[
            "api",
            "-X",
            "GET",
            "--paginate",
            &format!("{mr}/discussions?per_page=100"),
        ],
        &fixture("gl-discussions.json", &vars),
    );
    env.respond(
        &[
            "api",
            "-X",
            "GET",
            "--paginate",
            &format!("{mr}/draft_notes?per_page=100"),
        ],
        &fixture("gl-drafts.json", &vars),
    );
}

fn assert_unsent_finding(local: &Value) {
    assert_eq!(local["comments"][0]["body"], "Local text of f1.");
    assert_eq!(local["comments"][0]["post"], "pending");
    assert_eq!(local["comments"][0]["remote"], Value::Null);
}

#[test]
fn gitlab_sync_does_not_claim_an_unsent_legacy_marker() {
    let setup = reviewed_branch("git@gitlab.example.com:group/sub/proj.git");
    let env = &setup.env;
    let f1 = format!("{}.c1p0.r1.f1", setup.run);
    gitlab_responses(&setup, &setup.head);
    let replacement = format!("Edited text\n<!-- review-finding v1 id={f1} -->");
    env.respond(
        &[
            "api",
            "-X",
            "GET",
            "--paginate",
            "projects/group%2Fsub%2Fproj/merge_requests/3/discussions?per_page=100",
        ],
        &json!([{
            "id": "edited-discussion",
            "individual_note": false,
            "notes": [{
                "id": 8000, "body": replacement, "author": {"username": "me"},
                "created_at": "2026-10-16T10:00:00Z", "system": false,
                "resolvable": true, "resolved": true,
            }],
        }])
        .to_string(),
    );

    env.run_with(&["sync"], None, &[("GITLAB_HOST", "gitlab.example.com")]);
    let list = env.run(&["list"]);
    let local = thread(&list, &f1);
    assert_unsent_finding(local);
    assert_eq!(local["resolved"], false);
    assert_eq!(thread(&list, "gl-n8000")["type"], "comment");
    assert_read_only(env);
}

#[test]
fn hosts_file_names_the_service_of_an_unknown_host() {
    let setup = reviewed_branch("git@gitlab.example.com:group/sub/proj.git");
    let env = &setup.env;
    let old = env.git(&["rev-parse", "HEAD~1"]);
    gitlab_responses(&setup, &old);
    let refused = env.fail(&["sync"], None);
    assert!(
        refused.contains("unsupported forge gitlab.example.com"),
        "{refused}"
    );
    let dir = env.path("config/agent-review");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("hosts.json"),
        r#"{"Gitlab.Example.com": {"service": "gitlab"}}"#,
    )
    .unwrap();
    let probed = env.forge_calls().len();
    let synced = env.run(&["sync"]);
    assert_eq!(synced["target"], "gl-3");
    for call in &env.forge_calls()[probed..] {
        assert!(call.starts_with("gitlab.example.com\t"), "{call}");
        assert!(!call.contains("auth status"), "{call}");
    }
}

#[test]
fn a_host_synced_before_is_remembered_without_asking_the_cli() {
    let setup = reviewed_branch("git@gitlab.example.com:group/sub/proj.git");
    let env = &setup.env;
    let old = env.git(&["rev-parse", "HEAD~1"]);
    gitlab_responses(&setup, &old);
    env.run_with(&["sync"], None, &[("GITLAB_HOST", "gitlab.example.com")]);
    let again = env.run(&["sync"]);
    assert_eq!(again["target"], "gl-3");
}

#[test]
fn gitlab_self_hosted_sync() {
    let setup = reviewed_branch("git@gitlab.example.com:group/sub/proj.git");
    let env = &setup.env;
    let old = env.git(&["rev-parse", "HEAD~1"]);
    let f1 = format!("{}.c1p0.r1.f1", setup.run);
    env.run(&["resolve", &f1, "--resolution", "answered"]);
    gitlab_responses(&setup, &old);

    let synced = env.run_with(&["sync"], None, &[("GITLAB_HOST", "gitlab.example.com")]);
    assert_eq!(synced["target"], "gl-3");
    assert_eq!(synced["drafts"], 1);
    assert_eq!(synced["threads"]["imported"], 4);
    assert_eq!(synced["threads"]["updated"], 0);
    assert_read_only(env);
    for call in env.forge_calls() {
        assert!(call.starts_with("gitlab.example.com\t"), "{call}");
    }

    let list = env.run(&["list"]);
    let bot = thread(&list, "gl-n4005224826");
    assert_eq!(bot["resolved"], true);
    assert_eq!(
        bot["comments"].as_array().unwrap().len(),
        1,
        "system note dropped"
    );
    assert_eq!(
        bot["current"]["outdated"], true,
        "line 3 changed since the position head"
    );
    assert_eq!(
        thread(&list, "gl-n4007000002")["current"]["outdated"],
        false
    );
    let general = thread(&list, "gl-n4007000001");
    assert_eq!(general["remote"]["kind"], "general");
    assert_eq!(general["comments"][0]["created_at"], "2026-10-14T07:00:00Z");
    assert!(!ids(&list).iter().any(|id| id == "gl-n4000889251"));

    let mine = thread(&list, &f1);
    assert_eq!(mine["resolved"], true, "an unpublished local resolve stays");
    assert_eq!(mine["resolve_pending"], true);
    assert_eq!(mine["resolution"], "answered");
    assert_eq!(mine["severity"], "high");
    assert_unsent_finding(mine);
    assert_eq!(thread(&list, "gl-n4005300001")["type"], "comment");
    let draft = &thread(&list, &format!("{}.c1p0.r1.f2", setup.run))["comments"][0];
    assert_eq!(draft["post"], "posting");
    assert_eq!(draft["draft_id"], "77");

    let status = env.run(&["status"]);
    assert_eq!(status["review"]["host"], "gitlab.example.com");
    let open: Vec<&str> = status["open_threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["id"].as_str().unwrap())
        .collect();
    assert!(!open.contains(&f1.as_str()) && open.contains(&"gl-n4007000002"));
    assert!(!open.contains(&"gl-n4007000001"));
}

#[test]
fn sync_errors_without_forge_or_pr() {
    let env = Env::with_branch();
    let error = env.fail(&["sync"], None);
    assert!(error.contains("origin remote"), "{error}");
    env.git(&["remote", "add", "origin", "git@github.com:me/repo.git"]);
    env.respond(&["api", "--method", "GET", "user"], r#"{"login":"me"}"#);
    env.respond(
        &[
            "api",
            "--method",
            "GET",
            "repos/me/repo/pulls?head=me%3Afeat&state=all&per_page=20",
        ],
        "[]",
    );
    env.respond(
        &["api", "--method", "GET", "repos/me/repo"],
        r#"{"fork":false}"#,
    );
    let missing = env.fail(&["sync"], None);
    assert!(
        missing.contains("no PR/MR found for branch feat"),
        "{missing}"
    );
    let unexpected = env.fail(&["sync", "--pr", "99"], None);
    assert!(unexpected.contains("unexpected call"), "{unexpected}");
}

#[test]
fn forge_comes_from_the_host_never_a_guess() {
    let env = Env::with_branch();
    env.git(&[
        "remote",
        "add",
        "origin",
        "git@code.example.org:me/repo.git",
    ]);
    let error = env.fail(&["sync", "--pr", "5"], None);
    assert!(
        error.contains("unsupported forge code.example.org"),
        "{error}"
    );
    assert!(
        !env.forge_calls().iter().any(|call| call.contains("user")),
        "nothing is asked of an unknown host"
    );
    let forgejo = env.fail(
        &["sync", "--pr", "https://codeberg.org/me/repo/pulls/5"],
        None,
    );
    assert!(forgejo.contains("--pr must be a number or a"), "{forgejo}");

    // A host `gh auth status` knows is GitHub (Enterprise).
    env.respond(&["auth", "status", "--hostname", "code.example.org"], "");
    env.fail(&["sync", "--pr", "5"], None);
    assert!(
        env.forge_calls()
            .iter()
            .any(|call| call.ends_with("\tapi --method GET user")),
        "{:?}",
        env.forge_calls()
    );
}

#[test]
fn gitlab_host_comes_from_glab_or_gitlab_host() {
    let env = Env::with_branch();
    env.git(&[
        "remote",
        "add",
        "origin",
        "git@code.example.org:me/repo.git",
    ]);
    // glab's global default host is not the self-hosted one it is also logged in to.
    env.respond(&["config", "get", "host"], "gitlab.com\n");
    env.respond_glab(&["auth", "status", "--hostname", "code.example.org"], "");
    env.fail(&["sync", "--pr", "5"], None);
    assert!(
        env.forge_calls()
            .iter()
            .any(|call| call == "code.example.org\tapi -X GET user"),
        "{:?}",
        env.forge_calls()
    );

    let other = Env::with_branch();
    other.git(&["remote", "add", "origin", "git@git.example.net:me/repo.git"]);
    other.exec(
        &["sync", "--pr", "5"],
        None,
        &[("GITLAB_HOST", "https://git.example.net")],
    );
    assert!(
        other
            .forge_calls()
            .iter()
            .any(|call| call == "git.example.net\tapi -X GET user"),
        "{:?}",
        other.forge_calls()
    );
}
