use serde_json::{Value, json};

use crate::support::forge::assert_read_only;
use crate::support::forge::github::{self, Github};
use crate::support::forge::gitlab::{self, Gitlab};
use crate::support::scenario::{Setup, ids, thread};
use crate::support::{Env, fixture};

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
    let setup = Setup::synced_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    let branch_key = env.run(&["status", "--full"])["target"].clone();
    github::sync_fixtures(&setup);

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
    assert_read_only(&Github::new(), env);

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
    let setup = Setup::synced_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    let f1 = format!("{}.c1p0.r1.f1", setup.run);
    env.run(&["resolve", &f1, "--triage", "valid"]);
    github::sync_fixtures(&setup);
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
        "github/threads-1.json",
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
    let setup = Setup::synced_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    github::sync_fixtures(&setup);
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
    let setup = Setup::synced_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    github::sync_fixtures(&setup);
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
    let setup = Setup::synced_branch("https://github.com/me/repo.git");
    let env = &setup.env;
    github::login(env);
    github::pulls(env, "me/repo", "[]");
    env.respond(
        &["api", "--method", "GET", "repos/me/repo"],
        r#"{"fork":true,"parent":{"full_name":"up/repo"}}"#,
    );
    github::pulls(
        env,
        "up/repo",
        r#"[{"number":7,"state":"closed"},{"number":8,"state":"open"}]"#,
    );
    github::empty_pr(
        env,
        "up/repo",
        "8",
        "OPEN",
        "null",
        &setup.head,
        &setup.base,
    );
    let synced = env.run(&["sync"]);
    assert_eq!(synced["target"], "gh-8");
    assert_eq!(synced["review"]["repo"], "up/repo");
    assert_eq!(env.run(&["status"])["target"], "gh-8");
    assert!(env.state().join("github.com/me/repo/gh-8.json").exists());
    assert_read_only(&Github::new(), env);
}

#[test]
fn prune_ninety_days_after_close() {
    let setup = Setup::synced_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    github::login(env);
    github::empty_pr(
        env,
        "me/repo",
        "5",
        "MERGED",
        "\"2026-01-01T00:00:00Z\"",
        &setup.head,
        &setup.base,
    );
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

fn assert_unsent_finding(local: &Value) {
    assert_eq!(local["comments"][0]["body"], "Local text of f1.");
    assert_eq!(local["comments"][0]["post"], "pending");
    assert_eq!(local["comments"][0]["remote"], Value::Null);
}

#[test]
fn gitlab_sync_does_not_claim_an_unsent_legacy_marker() {
    let setup = Setup::synced_branch("git@gitlab.example.com:group/sub/proj.git");
    let env = &setup.env;
    let f1 = format!("{}.c1p0.r1.f1", setup.run);
    gitlab::sync_fixtures(&setup, &setup.head);
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
    assert_read_only(&Gitlab::new(), env);
}

#[test]
fn hosts_file_names_the_service_of_an_unknown_host() {
    let setup = Setup::synced_branch("git@gitlab.example.com:group/sub/proj.git");
    let env = &setup.env;
    let old = env.git(&["rev-parse", "HEAD~1"]);
    gitlab::sync_fixtures(&setup, &old);
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
    let setup = Setup::synced_branch("git@gitlab.example.com:group/sub/proj.git");
    let env = &setup.env;
    let old = env.git(&["rev-parse", "HEAD~1"]);
    gitlab::sync_fixtures(&setup, &old);
    env.run_with(&["sync"], None, &[("GITLAB_HOST", "gitlab.example.com")]);
    let again = env.run(&["sync"]);
    assert_eq!(again["target"], "gl-3");
}

#[test]
fn gitlab_self_hosted_sync() {
    let setup = Setup::synced_branch("git@gitlab.example.com:group/sub/proj.git");
    let env = &setup.env;
    let old = env.git(&["rev-parse", "HEAD~1"]);
    let f1 = format!("{}.c1p0.r1.f1", setup.run);
    env.run(&["resolve", &f1, "--resolution", "answered"]);
    gitlab::sync_fixtures(&setup, &old);

    let synced = env.run_with(&["sync"], None, &[("GITLAB_HOST", "gitlab.example.com")]);
    assert_eq!(synced["target"], "gl-3");
    assert_eq!(synced["drafts"], 1);
    assert_eq!(synced["threads"]["imported"], 4);
    assert_eq!(synced["threads"]["updated"], 0);
    assert_read_only(&Gitlab::new(), env);
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
    github::login(&env);
    github::pulls(&env, "me/repo", "[]");
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
