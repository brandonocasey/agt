//! `agt pr`: a fake gh/glab answers the exact calls the command makes, and the JSON on stdout,
//! the request bodies it sent and the paused auto-merge record are checked.

use std::process::Output;

use serde_json::{Value, json};

use crate::support::Env;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn setup(origin: &str) -> Env {
    let env = Env::new();
    env.git(&["remote", "add", "origin", origin]);
    env.write("a.txt", "a\n");
    env.commit_all("first");
    env.git(&["switch", "-q", "-c", "feat"]);
    env
}

fn github() -> Env {
    setup("https://github.com/o/r.git")
}

fn gitlab() -> Env {
    setup("https://gitlab.com/g/p.git")
}

fn pr(env: &Env, args: &[&str]) -> Output {
    env.command(env!("CARGO_BIN_EXE_agt"), &env.repo)
        .arg("pr")
        .args(args)
        .output()
        .unwrap()
}

fn ok(env: &Env, args: &[&str]) -> Value {
    let output = pr(env, args);
    assert!(
        output.status.success(),
        "agt pr {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn err(env: &Env, args: &[&str]) -> String {
    let output = pr(env, args);
    assert_eq!(output.status.code(), Some(1), "agt pr {args:?} should fail");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn node(number: u64, extra: &Value) -> String {
    let mut node = json!({
        "id": "PR_node", "number": number, "url": format!("https://github.com/o/r/pull/{number}"),
        "title": "Add a", "state": "OPEN", "isDraft": false, "headRefName": "feat",
        "headRefOid": SHA, "baseRefName": "main", "author": {"login": "me"},
        "labels": {"nodes": [{"name": "bug"}]}, "autoMergeRequest": null,
        "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN", "mergeQueueEntry": null,
    });
    for (key, value) in extra.as_object().unwrap() {
        node[key] = value.clone();
    }
    node.to_string()
}

fn gh_view(env: &Env, number: u64, extra: &Value) {
    let reply = format!(
        r#"{{"data":{{"repository":{{"pullRequest":{}}}}}}}"#,
        node(number, extra)
    );
    env.respond(
        &[
            "api",
            "graphql",
            "-f",
            "query=PrView",
            "-f",
            "owner=o",
            "-f",
            "name=r",
            "-F",
            &format!("number={number}"),
        ],
        &reply,
    );
}

fn gh_list(env: &Env, branch: &str, nodes: &[String]) {
    let reply = format!(
        r#"{{"data":{{"viewer":{{"login":"me"}},"repository":{{"pullRequests":{{"nodes":[{}]}}}}}}}}"#,
        nodes.join(",")
    );
    env.respond(
        &[
            "api",
            "graphql",
            "-f",
            "query=PrList",
            "-f",
            "owner=o",
            "-f",
            "name=r",
            "-f",
            &format!("branch={branch}"),
        ],
        &reply,
    );
}

fn inputs(env: &Env) -> Vec<(String, Value)> {
    env.forge_inputs()
        .into_iter()
        .map(|(args, input)| (args, serde_json::from_str(&input).unwrap_or(Value::Null)))
        .collect()
}

#[test]
fn view_reads_the_pr_of_the_current_branch() {
    let env = github();
    gh_list(&env, "feat", &[node(7, &json!({}))]);
    gh_view(
        &env,
        7,
        &json!({"autoMergeRequest": {"mergeMethod": "SQUASH"}, "mergeable": "CONFLICTING"}),
    );
    let view = ok(&env, &["view"]);
    assert_eq!(view["number"], 7);
    assert_eq!(view["state"], "open");
    assert_eq!(view["head_sha"], SHA);
    assert_eq!(view["author"], "me");
    assert_eq!(view["auto_merge"], "on");
    assert_eq!(view["auto_merge_method"], "squash");
    assert_eq!(view["queue"], Value::Null);
    assert_eq!(view["conflicts"], true);
    assert_eq!(view["labels"], json!(["bug"]));
}

#[test]
fn view_without_a_pr_names_the_branch() {
    let env = github();
    gh_list(&env, "feat", &[]);
    assert!(err(&env, &["view"]).contains("no PR/MR for branch feat in o/r"));
}

#[test]
fn an_unknown_forge_is_reported() {
    let env = setup("https://git.example.com/o/r.git");
    assert!(err(&env, &["view", "1"]).contains("not a GitHub or GitLab host"));
}

#[test]
fn list_mine_searches_by_author_on_the_server() {
    let env = github();
    // The remote names an old slug; search only finds the renamed repository.
    env.respond(
        &[
            "api",
            "graphql",
            "-f",
            "query=PrRepo",
            "-f",
            "owner=o",
            "-f",
            "name=r",
        ],
        r#"{"data":{"repository":{"nameWithOwner":"o/renamed"}}}"#,
    );
    env.respond(
        &[
            "api",
            "graphql",
            "-f",
            "query=PrSearch",
            "-f",
            "q=is:pr repo:o/renamed author:@me sort:created-desc is:open head:feat",
        ],
        &format!(
            r#"{{"data":{{"search":{{"nodes":[{}, {}, {{}}]}}}}}}"#,
            node(2, &json!({})),
            // `head:feat` also matches branches that start with `feat`.
            node(3, &json!({"headRefName": "feat-2"}))
        ),
    );
    let rows = ok(&env, &["list", "--mine", "--branch", "feat"]);
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["number"], 2);
}

#[test]
fn a_fork_pr_with_the_same_head_name_is_not_the_branchs_pr() {
    let env = github();
    gh_list(
        &env,
        "feat",
        &[
            node(1, &json!({"isCrossRepository": true})),
            node(2, &json!({"isCrossRepository": false})),
        ],
    );
    gh_view(&env, 2, &json!({}));
    assert_eq!(ok(&env, &["view"])["number"], 2);

    let env = github();
    gh_list(
        &env,
        "feat",
        &[node(1, &json!({"isCrossRepository": true}))],
    );
    env.respond(
        &["api", "--method", "POST", "repos/o/r/pulls", "--input", "-"],
        r#"{"number": 9}"#,
    );
    gh_view(&env, 9, &json!({}));
    assert_eq!(ok(&env, &["create", "--title", "T"])["number"], 9);

    let env = gitlab();
    env.respond(
        &["api", "-X", "GET", "projects/g%2Fp/merge_requests?state=all&order_by=created_at&sort=desc&per_page=100&source_branch=feat"],
        &format!("[{}]", gl_mr(&json!({"source_project_id": 8, "target_project_id": 5, "merge_when_pipeline_succeeds": false}))),
    );
    let message = err(&env, &["view"]);
    assert!(message.contains("no PR/MR for branch feat"), "{message}");
}

#[test]
fn create_without_a_default_branch_asks_for_base() {
    let env = github();
    env.git(&["branch", "-m", "main", "trunk"]);
    env.git(&["switch", "-q", "-c", "work"]);
    env.git(&["branch", "-D", "feat"]);
    gh_list(&env, "work", &[]);
    let message = err(&env, &["create", "--title", "T"]);
    assert!(
        message.contains("--base") && !message.contains("--default-branch"),
        "{message}"
    );
}

#[test]
fn create_refuses_when_the_branch_has_a_pr() {
    let env = github();
    env.respond(
        &["api", "graphql", "-f", "query=PrList", "-f", "owner=o", "-f", "name=r", "-f", "branch=feat"],
        &format!(
            r#"{{"data":{{"viewer":{{"login":"me"}},"repository":{{"pullRequests":{{"nodes":[{}]}}}}}}}}"#,
            node(7, &json!({}))
        ),
    );
    let output = pr(&env, &["create", "--title", "t"]);
    assert_eq!(output.status.code(), Some(1));
    let shown: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(shown["number"], 7);
    assert!(String::from_utf8_lossy(&output.stderr).contains("already has 7"));
}

#[test]
fn create_posts_the_branch_against_the_default_base() {
    let env = github();
    gh_list(&env, "feat", &[]);
    env.respond(
        &["api", "--method", "POST", "repos/o/r/pulls", "--input", "-"],
        r#"{"number": 9}"#,
    );
    gh_view(&env, 9, &json!({"isDraft": true}));
    let created = ok(&env, &["create", "--title", "T", "--body", "B", "--draft"]);
    assert_eq!(created["number"], 9);
    let sent = &inputs(&env)[0].1;
    assert_eq!(
        *sent,
        json!({"title": "T", "body": "B", "head": "feat", "base": "main", "draft": true})
    );
}

#[test]
fn update_replaces_the_label_set() {
    let env = github();
    gh_view(&env, 7, &json!({}));
    env.respond(
        &[
            "api",
            "--method",
            "PATCH",
            "repos/o/r/issues/7",
            "--input",
            "-",
        ],
        "{}",
    );
    ok(
        &env,
        &[
            "update",
            "7",
            "--title",
            "New",
            "--add-label",
            "ship",
            "--remove-label",
            "bug",
        ],
    );
    assert_eq!(
        inputs(&env)[0].1,
        json!({"title": "New", "labels": ["ship"]})
    );
    assert!(err(&env, &["update", "7"]).contains("nothing to update"));
}

#[test]
fn merge_refuses_a_head_that_is_not_the_sha() {
    let env = github();
    gh_view(&env, 7, &json!({}));
    let stale = "fedcba9876543210";
    assert!(err(&env, &["merge", "7", "--sha", stale]).contains("not fedcba98"));
    assert!(env.forge_inputs().is_empty());
}

#[test]
fn merge_sends_the_head_guard() {
    let env = github();
    gh_view(&env, 7, &json!({}));
    env.respond(
        &[
            "api",
            "--method",
            "PUT",
            "repos/o/r/pulls/7/merge",
            "--input",
            "-",
        ],
        "{}",
    );
    ok(&env, &["merge", "7", "--sha", SHA, "--method", "squash"]);
    assert_eq!(
        inputs(&env)[0].1,
        json!({"sha": SHA, "merge_method": "squash"})
    );
}

#[test]
fn auto_merge_of_a_pr_that_can_merge_now_merges_it() {
    let env = github();
    gh_view(&env, 7, &json!({}));
    env.respond(
        &[
            "api",
            "--method",
            "PUT",
            "repos/o/r/pulls/7/merge",
            "--input",
            "-",
        ],
        "{}",
    );
    ok(
        &env,
        &["merge", "7", "--sha", SHA, "--method", "squash", "--auto"],
    );
    assert_eq!(
        inputs(&env)[0].1,
        json!({"sha": SHA, "merge_method": "squash"})
    );
}

#[test]
fn auto_merge_off_records_the_method_and_on_restores_it() {
    let env = github();
    gh_view(
        &env,
        7,
        &json!({"autoMergeRequest": {"mergeMethod": "REBASE"}}),
    );
    env.respond_nth(
        &[
            "api",
            "graphql",
            "-f",
            "query=PrView",
            "-f",
            "owner=o",
            "-f",
            "name=r",
            "-F",
            "number=7",
        ],
        2,
        &format!(
            r#"{{"data":{{"repository":{{"pullRequest":{}}}}}}}"#,
            node(7, &json!({}))
        ),
    );
    env.respond(
        &[
            "api",
            "graphql",
            "-f",
            "query=PrAutoOff",
            "-f",
            "id=PR_node",
        ],
        "{}",
    );
    let off = ok(&env, &["auto-merge", "off", "7"]);
    assert_eq!(off["changed"], true);
    assert_eq!(off["pr"]["auto_merge"], "off");
    let record = std::fs::read_to_string(env.path("state/agents/pr/auto-merge.json")).unwrap();
    assert!(record.contains("rebase"), "{record}");

    env.reset_calls();
    // Checks still run, so GitHub offers auto-merge rather than an immediate merge.
    gh_view(&env, 7, &json!({"mergeStateStatus": "BLOCKED"}));
    env.respond_nth(
        &[
            "api",
            "graphql",
            "-f",
            "query=PrView",
            "-f",
            "owner=o",
            "-f",
            "name=r",
            "-F",
            "number=7",
        ],
        2,
        &format!(
            r#"{{"data":{{"repository":{{"pullRequest":{}}}}}}}"#,
            node(7, &json!({"autoMergeRequest": {"mergeMethod": "REBASE"}}))
        ),
    );
    env.respond(
        &[
            "api",
            "graphql",
            "-f",
            "query=PrAuto",
            "-f",
            "id=PR_node",
            "-f",
            "method=REBASE",
            "-f",
            &format!("sha={SHA}"),
        ],
        "{}",
    );
    let on = ok(&env, &["auto-merge", "on", "7"]);
    assert_eq!(on["changed"], true);
    assert_eq!(on["pr"]["auto_merge"], "on");
    let record = std::fs::read_to_string(env.path("state/agents/pr/auto-merge.json")).unwrap();
    assert!(!record.contains("rebase"), "{record}");
}

#[test]
fn auto_merge_leaves_a_queued_pr_alone() {
    let env = github();
    gh_view(&env, 7, &json!({"mergeQueueEntry": {"state": "QUEUED"}}));
    assert!(err(&env, &["auto-merge", "off", "7"]).contains("merge queue"));
}

fn gl_mr(extra: &Value) -> String {
    let mut mr = json!({
        "iid": 3, "web_url": "https://gitlab.com/g/p/-/merge_requests/3", "title": "Draft: Add a",
        "state": "opened", "draft": true, "source_branch": "feat", "target_branch": "main",
        "sha": SHA, "author": {"username": "me"}, "labels": [], "has_conflicts": false,
        "detailed_merge_status": "draft_status", "merge_when_pipeline_succeeds": true,
        "squash_on_merge": true,
    });
    for (key, value) in extra.as_object().unwrap() {
        mr[key] = value.clone();
    }
    mr.to_string()
}

#[test]
fn gitlab_view_reports_the_merge_train() {
    let env = gitlab();
    env.respond(
        &["api", "-X", "GET", "projects/g%2Fp/merge_requests?state=all&order_by=created_at&sort=desc&per_page=100&source_branch=feat"],
        &format!("[{}]", gl_mr(&json!({}))),
    );
    env.respond(
        &["api", "-X", "GET", "projects/g%2Fp/merge_requests/3"],
        &gl_mr(&json!({})),
    );
    env.respond(
        &[
            "api",
            "-X",
            "GET",
            "projects/g%2Fp/merge_trains/merge_requests/3",
        ],
        r#"{"status": "fresh"}"#,
    );
    let view = ok(&env, &["view"]);
    assert_eq!(view["number"], 3);
    assert_eq!(view["draft"], true);
    assert_eq!(view["auto_merge"], "on");
    assert_eq!(view["auto_merge_method"], "squash");
    assert_eq!(view["queue"], "fresh");
    assert_eq!(view["mergeable"], "blocked");
}

#[test]
fn gitlab_merge_auto_squash_sends_the_sha() {
    let env = gitlab();
    let ready =
        gl_mr(&json!({"draft": false, "merge_when_pipeline_succeeds": false, "title": "Add a"}));
    env.respond(
        &["api", "-X", "GET", "projects/g%2Fp/merge_requests/3"],
        &ready,
    );
    env.respond(
        &[
            "api",
            "-X",
            "PUT",
            "projects/g%2Fp/merge_requests/3/merge",
            "-H",
            "Content-Type: application/json",
            "--input",
            "-",
        ],
        "{}",
    );
    ok(
        &env,
        &["merge", "3", "--sha", SHA, "--method", "squash", "--auto"],
    );
    assert_eq!(
        inputs(&env)[0].1,
        json!({"sha": SHA, "should_remove_source_branch": false, "squash": true, "merge_when_pipeline_succeeds": true})
    );
    assert!(
        err(&env, &["merge", "3", "--sha", SHA, "--method", "rebase"]).contains("cannot rebase")
    );
}

const GL_MRS: &str = "projects/g%2Fp/merge_requests";
const GL_LIST: &str = "?state=opened&order_by=created_at&sort=desc&per_page=100";
const GL_WRITE: [&str; 4] = ["-H", "Content-Type: application/json", "--input", "-"];

fn gl_get(env: &Env, path: &str, body: &str) {
    env.respond(&["api", "-X", "GET", path], body);
}

fn gl_put(env: &Env, path: &str) {
    let mut args = vec!["api", "-X", "PUT", path];
    args.extend(GL_WRITE);
    env.respond(&args, "{}");
}

#[test]
fn mergeable_has_one_value_set_on_both_forges() {
    let env = gitlab();
    let status = |detailed: &str, conflicts: bool| {
        gl_get(
            &env,
            &format!("{GL_MRS}/3"),
            &gl_mr(
                &json!({"detailed_merge_status": detailed, "has_conflicts": conflicts, "merge_when_pipeline_succeeds": false}),
            ),
        );
        ok(&env, &["view", "3"])["mergeable"].clone()
    };
    assert_eq!(status("mergeable", false), "mergeable");
    assert_eq!(status("checking", false), "unknown");
    assert_eq!(status("not_approved", false), "blocked");
    assert_eq!(status("need_rebase", true), "conflicting");
    assert_eq!(status("need_rebase", false), "blocked");
    let env = github();
    gh_view(&env, 7, &json!({"mergeStateStatus": "BLOCKED"}));
    assert_eq!(ok(&env, &["view", "7"])["mergeable"], "blocked");
    gh_view(
        &env,
        7,
        &json!({"mergeable": "UNKNOWN", "mergeStateStatus": "UNKNOWN"}),
    );
    assert_eq!(ok(&env, &["view", "7"])["mergeable"], "unknown");
}

#[test]
fn gitlab_create_posts_a_draft_mr() {
    let env = gitlab();
    gl_get(&env, &format!("{GL_MRS}{GL_LIST}&source_branch=feat"), "[]");
    let mut post = vec!["api", "-X", "POST", GL_MRS];
    post.extend(GL_WRITE);
    env.respond(&post, r#"{"iid": 3}"#);
    gl_get(
        &env,
        &format!("{GL_MRS}/3"),
        &gl_mr(&json!({"merge_when_pipeline_succeeds": false})),
    );
    let created = ok(&env, &["create", "--title", "T", "--body", "B", "--draft"]);
    assert_eq!(created["number"], 3);
    assert_eq!(
        inputs(&env)[0].1,
        json!({"source_branch": "feat", "target_branch": "main", "title": "Draft: T", "description": "B"})
    );
}

#[test]
fn gitlab_update_sets_fields_labels_and_draft_state() {
    let env = gitlab();
    let path = format!("{GL_MRS}/3");
    gl_get(
        &env,
        &path,
        &gl_mr(&json!({"merge_when_pipeline_succeeds": false})),
    );
    gl_put(&env, &path);
    ok(
        &env,
        &[
            "update",
            "3",
            "--body",
            "B",
            "--add-label",
            "a",
            "--add-label",
            "b",
            "--remove-label",
            "c",
        ],
    );
    assert_eq!(
        inputs(&env)[0].1,
        json!({"description": "B", "add_labels": "a,b", "remove_labels": "c"})
    );
    ok(&env, &["update", "3", "--ready"]);
    assert_eq!(inputs(&env)[1].1, json!({"title": "Add a"}));
    ok(&env, &["update", "3", "--title", "Draft: New", "--draft"]);
    assert_eq!(inputs(&env)[2].1, json!({"title": "Draft: New"}));
    gl_get(
        &env,
        &path,
        &gl_mr(&json!({"title": "Add a", "merge_when_pipeline_succeeds": false})),
    );
    ok(&env, &["update", "3", "--draft"]);
    assert_eq!(inputs(&env)[3].1, json!({"title": "Draft: Add a"}));
}

#[test]
fn gitlab_list_mine_asks_for_created_by_me() {
    let env = gitlab();
    let mr = gl_mr(&json!({"merge_when_pipeline_succeeds": false}));
    gl_get(
        &env,
        &format!("{GL_MRS}{GL_LIST}&scope=created_by_me"),
        &format!("[{mr}]"),
    );
    let rows = ok(&env, &["list", "--mine"]);
    assert_eq!(rows[0]["number"], 3);
    assert_eq!(rows[0]["state"], "open");
}

#[test]
fn gitlab_merge_refuses_a_head_that_is_not_the_sha() {
    let env = gitlab();
    gl_get(
        &env,
        &format!("{GL_MRS}/3"),
        &gl_mr(&json!({"draft": false, "merge_when_pipeline_succeeds": false})),
    );
    assert!(err(&env, &["merge", "3", "--sha", "fedcba9876543210"]).contains("not fedcba98"));
    assert!(env.forge_inputs().is_empty());
}

#[test]
fn github_ready_and_draft_run_their_mutations() {
    let env = github();
    gh_view(&env, 7, &json!({}));
    env.respond(
        &["api", "graphql", "-f", "query=PrDraft", "-f", "id=PR_node"],
        "{}",
    );
    ok(&env, &["update", "7", "--ready"]);
    ok(&env, &["update", "7", "--draft"]);
    let calls = env.forge_calls().join("\n");
    assert_eq!(calls.matches("query=PrDraft").count(), 2);
    assert!(env.forge_inputs().is_empty());
}

#[test]
fn gitlab_merge_method_merge_refuses_a_squash_on_mr() {
    let env = gitlab();
    let on = gl_mr(&json!({"draft": false, "merge_when_pipeline_succeeds": false}));
    gl_get(&env, &format!("{GL_MRS}/3"), &on);
    let message = err(&env, &["merge", "3", "--sha", SHA, "--method", "merge"]);
    assert!(message.contains("squash on"), "{message}");
    let off = gl_mr(
        &json!({"draft": false, "merge_when_pipeline_succeeds": false, "squash_on_merge": false}),
    );
    gl_get(&env, &format!("{GL_MRS}/3"), &off);
    gl_put(&env, &format!("{GL_MRS}/3/merge"));
    ok(&env, &["merge", "3", "--sha", SHA, "--method", "merge"]);
    assert_eq!(
        inputs(&env)[0].1,
        json!({"sha": SHA, "should_remove_source_branch": false})
    );
}

#[test]
fn gitlab_auto_merge_on_that_merges_at_once_succeeds_and_forgets_the_memo() {
    let env = gitlab();
    let mr = format!("{GL_MRS}/3");
    let open = |auto: bool| {
        gl_mr(
            &json!({"draft": false, "merge_when_pipeline_succeeds": auto, "detailed_merge_status": "mergeable"}),
        )
    };
    gl_get(&env, &mr, &open(true));
    env.respond_nth(&["api", "-X", "GET", &mr], 2, &open(false));
    env.fail_call_with(
        &[
            "api",
            "-X",
            "GET",
            "projects/g%2Fp/merge_trains/merge_requests/3",
        ],
        "glab: 404 Not Found (HTTP 404)",
    );
    let cancel = vec![
        "api",
        "-X",
        "POST",
        "projects/g%2Fp/merge_requests/3/cancel_merge_when_pipeline_succeeds",
    ];
    env.respond(&cancel, "{}");
    ok(&env, &["auto-merge", "off", "3"]);
    let record = env.path("state/agents/pr/auto-merge.json");
    assert!(std::fs::read_to_string(&record).unwrap().contains("squash"));

    env.reset_calls();
    gl_get(&env, &mr, &open(false));
    env.respond_nth(
        &["api", "-X", "GET", &mr],
        2,
        &gl_mr(&json!({"state": "merged", "merge_when_pipeline_succeeds": false})),
    );
    gl_put(&env, &format!("{mr}/merge"));
    let on = ok(&env, &["auto-merge", "on", "3"]);
    assert_eq!(on["merged"], true);
    assert_eq!(on["pr"]["state"], "merged");
    assert!(!std::fs::read_to_string(&record).unwrap().contains("squash"));
}
