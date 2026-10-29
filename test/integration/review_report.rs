//! `agt status`, `agt review status` and `agt qa status` by name, their unreviewed and untested
//! commits, what was fixed, and the `--all` summary. Piped output is JSON; the text renderers
//! are unit-tested on fixed JSON.

use std::process::Output;

use serde_json::{Value, json};

use crate::support::forge::github;
use crate::support::scenario::Setup;
use crate::support::{Env, numbered};

const REVIEWER: &str = r#"[{"model":"m","harness":"codex"}]"#;
const FINDING: &str =
    r#"[{"type":"bug","severity":"high","path":"a.txt","line":3,"scenario":"broken"}]"#;

fn agt(env: &Env, command: &[&str], args: &[&str], vars: &[(&str, &str)]) -> Output {
    env.bin(&env.repo)
        .args(command)
        .args(args)
        .envs(vars.iter().copied())
        .output()
        .unwrap()
}

fn success(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The error text of a command that must fail.
fn failure(output: &Output) -> String {
    assert_eq!(output.status.code(), Some(1), "should fail");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn top(env: &Env, args: &[&str]) -> Value {
    success(&agt(env, &["status"], args, &[]))
}

fn qa(env: &Env, args: &[&str]) -> Value {
    success(&agt(env, &["qa"], args, &[]))
}

/// A branch `feat` with one commit and a full review pass that found one bug, in its own
/// store. Returns the environment, the run and the finding.
fn reviewed() -> (Env, String, String) {
    reviewed_on(Env::with_branch().offline())
}

fn reviewed_on(env: Env) -> (Env, String, String) {
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let started = env.run_stdin(
        &["pass", "start", "--kind", "full", "--reviewers", "-"],
        REVIEWER,
    );
    let run = started["run"].as_str().unwrap().to_owned();
    let ended = env.run_stdin(
        &[
            "pass",
            "end",
            "--run",
            &run,
            "--pass",
            "c1p0",
            "--findings",
            "-",
            "--reviewer",
            "r1",
        ],
        FINDING,
    );
    let finding = ended["added"][0].as_str().unwrap().to_owned();
    env.run(&["run", "end", "--run", &run]);
    (env, run, finding)
}

fn shas(commits: &Value) -> Vec<&str> {
    commits
        .as_array()
        .unwrap()
        .iter()
        .map(|commit| commit["sha"].as_str().unwrap())
        .collect()
}

#[test]
fn new_commits_are_counted_unreviewed_and_untested() {
    let (env, _, _) = reviewed();
    let status = env.run(&["status"]);
    assert_eq!(status["unreviewed_commits"], json!([]));
    assert_eq!(status["untested_commits"][0]["subject"], "change");
    assert_eq!(status["tested_head"], Value::Null);
    assert_eq!(status["opened_at"], Value::Null);

    env.write("b.txt", "b\n");
    let second = env.commit_all("second");
    let status = env.run(&["status"]);
    assert_eq!(status["unreviewed_commits"], json!([second]));
    assert_eq!(status["untested_commits"].as_array().unwrap().len(), 2);

    let started = qa(&env, &["run", "start"]);
    let run = started["run"].as_str().unwrap().to_owned();
    qa(
        &env,
        &[
            "check",
            "--run",
            &run,
            "--name",
            "login",
            "--outcome",
            "pass",
        ],
    );
    let status = env.run(&["status"]);
    assert_eq!(status["untested_commits"], json!([]));
    assert_eq!(status["tested_head"], second.as_str());

    env.write("c.txt", "c\n");
    let third = env.commit_all("third");
    let status = qa(&env, &["status"]);
    assert_eq!(shas(&status["untested_commits"]), [third.as_str()]);
    assert_eq!(status["tested_head"], second.as_str());
    assert!(status.get("commits").is_none(), "{status}");
    // The same fields, and the review's own, through the top-level command.
    let all = top(&env, &[]);
    assert_eq!(all["untested_commits"], status["untested_commits"]);
    assert_eq!(all["unreviewed_commits"].as_array().unwrap().len(), 2);
}

#[test]
fn a_rebase_that_changes_no_content_keeps_the_commits_tested() {
    let (env, _, _) = reviewed();
    let run = qa(&env, &["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    qa(
        &env,
        &[
            "check",
            "--run",
            &run,
            "--name",
            "login",
            "--outcome",
            "pass",
        ],
    );
    env.git(&["commit", "-q", "--amend", "-m", "change, reworded"]);
    let status = qa(&env, &["status"]);
    assert_eq!(status["untested_commits"], json!([]), "{status}");
}

#[test]
fn fixed_threads_and_checks_name_their_fix_commit() {
    let (env, _, finding) = reviewed();
    let run = qa(&env, &["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let failed = qa(
        &env,
        &[
            "check",
            "--run",
            &run,
            "--name",
            "checkout",
            "--outcome",
            "fail",
        ],
    )["check"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let found = qa(
        &env,
        &[
            "finding",
            "--check",
            &failed,
            "--type",
            "bug",
            "--severity",
            "high",
            "--body",
            "The total ignores the coupon.",
        ],
    )["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(env.run(&["status"])["fixed"]["threads"], json!([]));

    env.write("a.txt", &numbered(10, &[(3, "fixed")]));
    let fix = env.commit_all("fix the total");
    env.run(&["resolve", &finding, "--fixed", &fix]);
    env.run(&["resolve", &found, "--fixed", &fix]);
    qa(
        &env,
        &[
            "check",
            "--run",
            &run,
            "--name",
            "checkout",
            "--outcome",
            "pass",
        ],
    );

    let fixed = env.run(&["status"])["fixed"].clone();
    let threads = fixed["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 2, "{fixed}");
    for thread in threads {
        assert_eq!(thread["sha"], fix.as_str());
        assert_eq!(thread["subject"], "fix the total");
    }
    let review = threads
        .iter()
        .find(|thread| thread["id"] == finding.as_str());
    assert_eq!(review.unwrap()["check"], Value::Null);
    let qa_thread = threads.iter().find(|thread| thread["id"] == found.as_str());
    assert_eq!(qa_thread.unwrap()["check"], failed.as_str());
    assert_eq!(fixed["checks"][0]["name"], "checkout");
    assert_eq!(qa(&env, &["status"])["fixed"], fixed);
}

#[test]
fn a_branch_and_a_worktree_name_the_same_target() {
    let (env, _, _) = reviewed();
    env.git(&["switch", "-q", "main"]);
    let tree = env.path("trees").join("review-tree");
    env.git(&["worktree", "add", "-q", tree.to_str().unwrap(), "feat"]);

    let by_branch = env.run(&["status", "feat"]);
    assert_eq!(by_branch["branch"], "feat");
    assert_eq!(by_branch["reviewed"], true);
    let by_worktree = env.run(&["status", "review-tree"]);
    assert_eq!(by_worktree["target"], by_branch["target"]);
    let by_path = top(&env, &[tree.to_str().unwrap()]);
    assert_eq!(by_path["target"], by_branch["target"]);
    let qa = qa(&env, &["status", "review-tree"]);
    assert_eq!(qa["target"], by_branch["target"]);

    let error = env.fail(&["status", "nowhere"], None);
    assert!(error.contains("nowhere is no known"), "{error}");
    let error = env.fail(&["status", "feat", "--target", "x"], None);
    assert!(error.contains("cannot be combined"), "{error}");
}

#[test]
fn a_name_for_two_different_branches_lists_both() {
    let (env, _, _) = reviewed();
    env.git(&["switch", "-q", "main"]);
    let tree = env.path("trees").join("other");
    env.git(&["worktree", "add", "-q", tree.to_str().unwrap(), "feat"]);
    env.git(&["branch", "other", "main"]);
    let error = env.fail(&["status", "other"], None);
    assert!(
        error.contains("more than one branch (feat, other)"),
        "{error}"
    );
}

#[test]
fn an_mr_number_or_url_finds_the_synced_target() {
    let setup = Setup::synced_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    github::sync_fixtures(&setup);
    // The report tests read what sync pulled; nothing is published.
    env.write_config("[sync]\npush = \"off\"\n");
    env.run(&["sync", "--pr", "42"]);
    env.git(&["switch", "-q", "main"]);
    for name in ["42", "#42", "!42", "https://github.com/me/repo/pull/42"] {
        let status = success(&agt(env, &["status"], &[name], &[("AGT_OFFLINE", "1")]));
        assert_eq!(status["target"], "gh-42", "{name}");
        assert_eq!(status["branch"], "feat", "{name}");
        assert_eq!(status["opened_at"], "2026-10-20T08:00:00Z", "{name}");
    }
    let error = failure(&agt(env, &["status"], &["43"], &[("AGT_OFFLINE", "1")]));
    assert!(error.contains("43 is no known"), "{error}");
}

/// A branch with a comment, so a target exists, and no PR/MR.
fn local_only(env: &Env, branch: &str) {
    env.git(&["switch", "-q", "-c", branch, "main"]);
    env.write(&format!("{branch}.txt"), "x\n");
    env.commit_all(branch);
    env.run(&[
        "comment", "--body", "Why?", "--type", "question", "--author", "alice",
    ]);
}

fn gh_pull(number: u64, branch: &str, created: &str) -> Value {
    json!({
        "number": number,
        "url": format!("https://github.com/acme/app/pull/{number}"),
        "title": "t", "state": "OPEN", "mergedAt": null, "closedAt": null,
        "baseRefName": "main", "headRefName": branch, "headRefOid": "abc",
        "isCrossRepository": false, "createdAt": created,
        "author": {"login": "ann"}, "isDraft": number == 8,
    })
}

#[test]
fn the_summary_asks_the_chosen_tracker_not_origin() {
    let (env, _, _) = reviewed_on(Env::with_branch());
    env.git(&["remote", "add", "origin", "git@github.com:me/app.git"]);
    env.git(&["remote", "add", "upstream", "git@github.com:acme/app.git"]);
    // Origin is turned off for pull requests, so upstream is the only tracker.
    env.write_config(
        "[[tracker]]\nhost = \"github.com\"\nrepo = \"me/app\"\npull_requests = false\n",
    );
    respond_open_pulls_of(
        &env,
        "acme",
        &[gh_pull(7, "elsewhere", "2026-10-20T08:00:00Z")],
    );
    let summary = success(&agt(&env, &["status"], &["--all"], &[]));
    assert_eq!(summary["note"], Value::Null, "{summary}");
    let rows = summary["targets"].as_array().unwrap();
    assert!(rows.iter().any(|row| row["mr"] == 7), "{summary}");
    assert!(
        !env.forge_calls()
            .iter()
            .any(|call| call.contains("owner=me ")),
        "{:?}",
        env.forge_calls()
    );
}

#[test]
fn the_summary_lists_open_mrs_oldest_first_then_local_targets() {
    let (env, _, _) = reviewed_on(Env::with_branch());
    env.git(&["remote", "add", "origin", "git@github.com:acme/app.git"]);
    local_only(&env, "scratch");
    env.git(&["switch", "-q", "feat"]);
    let nodes = [
        gh_pull(7, "feat", "2026-10-20T08:00:00Z"),
        gh_pull(8, "other", "2026-10-10T08:00:00Z"),
    ];
    env.respond(
        &[
            "api",
            "graphql",
            "-f",
            "owner=acme",
            "-f",
            "name=app",
            "-f",
            "query=OpenPulls",
        ],
        &json!({"data": {"repository": {"pullRequests": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": nodes,
        }}}})
        .to_string(),
    );
    let summary = success(&agt(&env, &["status"], &["--all"], &[]));
    assert_eq!(summary["offline"], false, "{summary}");
    let rows = summary["targets"].as_array().unwrap();
    let order: Vec<(&Value, &Value)> = rows
        .iter()
        .map(|row| (&row["mr"], &row["branch"]))
        .collect();
    assert_eq!(
        order,
        [
            (&json!(8), &json!("other")),
            (&json!(7), &json!("feat")),
            (&Value::Null, &json!("scratch")),
        ]
    );
    assert_eq!(rows[0]["draft"], true);
    assert_eq!(rows[0]["author"], "ann");
    assert_eq!(rows[0]["target"], Value::Null);
    assert_eq!(rows[0].get("unreviewed"), None);
    assert_eq!(rows[1]["opened_at"], "2026-10-20T08:00:00Z");
    assert_eq!(rows[1]["unreviewed"], 0);
    assert_eq!(rows[1]["untested"], 1);
    assert_eq!(rows[1]["open"], 1);
    assert_eq!(rows[2]["open"], 1);
    assert_eq!(rows[2]["unreviewed"], 1);

    // The review-only and QA-only commands count their own threads.
    let review = success(&agt(&env, &["review", "status"], &["--all"], &[]));
    assert_eq!(review["targets"][1]["open"], 1);
    let qa_only = success(&agt(&env, &["qa", "status"], &["--all"], &[]));
    assert_eq!(qa_only["targets"][1]["open"], 0);
    assert_eq!(qa_only["targets"][1]["qa"], Value::Null);
}

#[test]
fn the_offline_summary_uses_stored_reviews() {
    let setup = Setup::synced_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    github::sync_fixtures(&setup);
    // The report tests read what sync pulled; nothing is published.
    env.write_config("[sync]\npush = \"off\"\n");
    env.run(&["sync", "--pr", "42"]);
    local_only(env, "scratch");
    let summary = success(&agt(env, &["status"], &["--all"], &[("AGT_OFFLINE", "1")]));
    assert_eq!(summary["offline"], true);
    assert!(summary["note"].as_str().unwrap().contains("offline"));
    let rows = summary["targets"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{summary}");
    assert_eq!(rows[0]["mr"], 42);
    assert_eq!(rows[0]["opened_at"], "2026-10-20T08:00:00Z");
    assert_eq!(rows[1]["branch"], "scratch");
    assert!(
        setup
            .env
            .forge_calls()
            .iter()
            .all(|call| !call.contains("OpenPulls"))
    );
}

#[test]
fn all_takes_no_name_and_no_target() {
    let (env, _, _) = reviewed();
    let error = failure(&agt(&env, &["status"], &["--all", "feat"], &[]));
    assert!(error.contains("--all"), "{error}");
}

/// The one-PR lookup of a name, for `owner/app`; `None` answers that the number does not exist.
fn respond_one_pull(env: &Env, owner: &str, number: u64, node: Option<Value>) {
    env.respond(
        &[
            "api",
            "graphql",
            "-f",
            &format!("owner={owner}"),
            "-f",
            "name=app",
            "-F",
            &format!("number={number}"),
            "-f",
            "query=OnePull",
        ],
        &json!({"data": {"repository": {"pullRequest": node}}}).to_string(),
    );
}

fn respond_open_pulls_of(env: &Env, owner: &str, nodes: &[Value]) {
    env.respond(
        &[
            "api",
            "graphql",
            "-f",
            &format!("owner={owner}"),
            "-f",
            "name=app",
            "-f",
            "query=OpenPulls",
        ],
        &json!({"data": {"repository": {"pullRequests": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": nodes,
        }}}})
        .to_string(),
    );
}

#[test]
fn a_relative_worktree_path_names_the_branch() {
    let (env, _, _) = reviewed();
    env.git(&["switch", "-q", "main"]);
    let tree = env.path("trees").join("rel-tree");
    env.git(&["worktree", "add", "-q", tree.to_str().unwrap(), "feat"]);
    let relative = tree
        .strip_prefix(env.repo.parent().unwrap())
        .map(|rest| format!("../{}", rest.display()))
        .unwrap();
    let status = top(&env, &[&relative]);
    assert_eq!(status["branch"], "feat", "{relative}");
}

#[test]
fn a_fork_pull_does_not_resolve_to_a_same_named_local_branch() {
    let (env, _, _) = reviewed_on(Env::with_branch());
    env.git(&["remote", "add", "origin", "git@github.com:acme/app.git"]);
    env.git(&["switch", "-q", "main"]);
    let mut fork = gh_pull(9, "feat", "2026-10-20T08:00:00Z");
    fork["isCrossRepository"] = json!(true);
    respond_one_pull(&env, "acme", 9, Some(fork));
    let error = failure(&agt(&env, &["status"], &["9"], &[]));
    assert!(error.contains("9 is no known PR/MR"), "{error}");
}

#[test]
fn a_url_for_another_repository_is_not_origins_number() {
    let (env, _, _) = reviewed_on(Env::with_branch());
    env.git(&["remote", "add", "origin", "git@github.com:acme/app.git"]);
    env.git(&["switch", "-q", "main"]);
    respond_one_pull(
        &env,
        "acme",
        9,
        Some(gh_pull(9, "feat", "2026-10-20T08:00:00Z")),
    );
    let error = failure(&agt(
        &env,
        &["status"],
        &["https://github.com/someone/else/pull/9"],
        &[],
    ));
    assert!(error.contains("is no known PR/MR"), "{error}");
}

#[test]
fn a_failing_forge_call_falls_back_to_a_local_name() {
    let (env, _, _) = reviewed_on(Env::with_branch());
    env.git(&["remote", "add", "origin", "git@github.com:acme/app.git"]);
    env.git(&["switch", "-q", "main"]);
    // No response is registered for the open PR/MR query, so the forge call fails.
    let tree = env.path("trees").join("123");
    env.git(&["worktree", "add", "-q", tree.to_str().unwrap(), "feat"]);
    let status = top(&env, &["123"]);
    assert_eq!(status["branch"], "feat");
}

#[test]
fn the_summary_drops_local_targets_whose_branch_is_gone() {
    let (env, _, _) = reviewed();
    local_only(&env, "scratch");
    env.git(&["switch", "-q", "main"]);
    env.git(&["branch", "-D", "scratch"]);
    let summary = success(&agt(&env, &["status"], &["--all"], &[]));
    let branches: Vec<&Value> = summary["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| &row["branch"])
        .collect();
    assert_eq!(branches, [&json!("feat")], "{summary}");
}

#[test]
fn a_pull_does_not_take_a_target_that_holds_another_review() {
    let setup = Setup::synced_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    github::sync_fixtures(&setup);
    // The report tests read what sync pulled; nothing is published.
    env.write_config("[sync]\npush = \"off\"\n");
    env.run(&["sync", "--pr", "42"]);
    let mut pull = gh_pull(77, "feat", "2026-10-20T08:00:00Z");
    pull["url"] = json!("https://github.com/me/repo/pull/77");
    env.respond(
        &[
            "api",
            "graphql",
            "-f",
            "owner=me",
            "-f",
            "name=repo",
            "-f",
            "query=OpenPulls",
        ],
        &json!({"data": {"repository": {"pullRequests": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [pull],
        }}}})
        .to_string(),
    );
    let summary = success(&agt(env, &["status"], &["--all"], &[]));
    let row = summary["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["mr"] == 77)
        .unwrap();
    assert_eq!(row["target"], Value::Null, "{summary}");
}

#[test]
fn sync_refuses_all_and_status_target_conflicts_with_range() {
    let (env, _, _) = reviewed_on(Env::with_branch());
    let error = failure(&agt(&env, &["status"], &["--all", "--sync"], &[]));
    assert!(error.contains("--all"), "{error}");
    let output = agt(
        &env,
        &["status"],
        &["--target", "x", "--range", "a..b"],
        &[],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
}

#[test]
fn a_url_matches_origin_in_any_case() {
    let (env, _, _) = reviewed_on(Env::with_branch());
    env.git(&["remote", "add", "origin", "git@github.com:acme/app.git"]);
    env.git(&["switch", "-q", "main"]);
    respond_one_pull(
        &env,
        "acme",
        9,
        Some(gh_pull(9, "feat", "2026-10-20T08:00:00Z")),
    );
    let url = "https://github.com/Acme/App/pull/9";
    let status = success(&agt(&env, &["status"], &[url], &[]));
    assert_eq!(status["branch"], "feat");
}

#[test]
fn sync_with_an_mr_name_imports_that_mr() {
    let setup = Setup::synced_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    github::sync_fixtures(&setup);
    env.git(&["switch", "-q", "main"]);
    let status = success(&agt(env, &["status"], &["42", "--sync"], &[]));
    assert_eq!(status["target"], "gh-42");
    assert_eq!(status["branch"], "feat");
}

#[test]
fn sync_refuses_another_branch_target_without_a_pr() {
    let (env, _, _) = reviewed_on(Env::with_branch());
    env.git(&["remote", "add", "origin", "git@github.com:acme/app.git"]);
    local_only(&env, "scratch");
    let key = top(&env, &["--json"])["target"]
        .as_str()
        .unwrap()
        .to_owned();
    env.git(&["switch", "-q", "feat"]);
    for command in [
        &["status", "--target", &key, "--sync"][..],
        &["review", "--target", &key, "sync"],
        // Naming the tracker never lifts the guard.
        &["review", "--target", &key, "sync", "--remote", "origin"],
    ] {
        let error = failure(&agt(&env, command, &[], &[]));
        assert!(
            error.contains("has no PR/MR and is not the current branch's"),
            "{error}"
        );
    }
}

#[test]
fn a_stored_review_takes_a_fork_remote_but_not_another_project() {
    let setup = Setup::synced_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    github::sync_fixtures(&setup);
    env.write_config("[sync]\npush = \"off\"\n");
    env.run(&["sync", "--pr", "42"]);
    env.git(&["remote", "add", "fork", "git@github.com:you/repo.git"]);
    env.git(&["remote", "add", "other", "git@github.com:me/unrelated.git"]);
    let synced = agt(
        env,
        &["review", "--target", "gh-42", "sync", "--remote", "fork"],
        &[],
        &[],
    );
    assert!(
        synced.status.success(),
        "{}",
        String::from_utf8_lossy(&synced.stderr)
    );
    let error = failure(&agt(
        env,
        &["review", "--target", "gh-42", "sync", "--remote", "other"],
        &[],
        &[],
    ));
    assert!(
        error.contains("is synced with github.com/me/repo"),
        "{error}"
    );
}

#[test]
fn a_named_status_survives_the_auto_pull_renaming_its_target() {
    let setup = Setup::synced_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    github::sync_fixtures(&setup);
    env.write_config("[sync]\nauto_pull = true\n");
    env.git(&["switch", "-q", "main"]);
    for command in [&["status"][..], &["qa", "status"]] {
        let status = success(&agt(env, command, &["feat"], &[]));
        assert_eq!(status["target"], "gh-42", "{status}");
    }
}

/// A synced review of `me/repo` #42 on `feat`, with origin then pointed at the fork `me/app`.
fn parent_review_in_a_fork() -> Setup {
    let setup = Setup::synced_branch("git@github.com:me/repo.git");
    github::sync_fixtures(&setup);
    // The report tests read what sync pulled; nothing is published.
    setup.env.write_config("[sync]\npush = \"off\"\n");
    setup.env.run(&["sync", "--pr", "42"]);
    setup
        .env
        .git(&["remote", "set-url", "origin", "git@github.com:me/app.git"]);
    setup
}

#[test]
fn a_fork_summary_keeps_the_open_reviews_of_the_parent_repository() {
    let setup = parent_review_in_a_fork();
    respond_open_pulls_of(&setup.env, "me", &[]);
    let summary = success(&agt(&setup.env, &["status"], &["--all"], &[]));
    let rows = summary["targets"].as_array().unwrap();
    assert!(rows.iter().any(|row| row["mr"] == 42), "{summary}");
}

#[test]
fn a_pull_number_never_takes_the_target_of_another_repository() {
    let setup = parent_review_in_a_fork();
    let mut pull = gh_pull(42, "elsewhere", "2026-10-20T08:00:00Z");
    pull["url"] = json!("https://github.com/me/app/pull/42");
    respond_open_pulls_of(&setup.env, "me", &[pull]);
    let summary = success(&agt(&setup.env, &["status"], &["--all"], &[]));
    let rows = summary["targets"].as_array().unwrap();
    let own = rows
        .iter()
        .find(|row| row["branch"] == "elsewhere")
        .unwrap();
    assert_eq!(own["target"], Value::Null, "{summary}");
}

#[test]
fn a_pull_resolved_to_its_branch_does_not_report_another_pulls_review() {
    let setup = parent_review_in_a_fork();
    let env = &setup.env;
    env.git(&["switch", "-q", "main"]);
    let mut pull = gh_pull(43, "feat", "2026-10-20T08:00:00Z");
    pull["url"] = json!("https://github.com/me/app/pull/43");
    respond_one_pull(env, "me", 43, Some(pull));
    let error = failure(&agt(env, &["status"], &["43"], &[]));
    assert!(
        error.contains("43 is PR/MR 43 of branch feat, which has no review or QA records here"),
        "{error}"
    );
}

#[test]
fn a_name_and_target_conflict_is_refused_before_the_sync() {
    let setup = Setup::synced_branch("git@github.com:me/repo.git");
    let env = &setup.env;
    github::sync_fixtures(&setup);
    let key = top(env, &["--json"])["target"].as_str().unwrap().to_owned();
    let error = failure(&agt(
        env,
        &["status"],
        &["42", "--target", &key, "--sync"],
        &[],
    ));
    assert!(
        error.contains("NAME cannot be combined with --target or --range"),
        "{error}"
    );
    let after = success(&agt(env, &["status"], &["--target", &key], &[]));
    assert_eq!(after["target"], key.as_str());
    let missing = failure(&agt(env, &["status"], &["--target", "gh-42"], &[]));
    assert!(missing.contains("does not exist"), "{missing}");
}

#[test]
fn a_closed_pull_resolves_to_its_branch_by_name() {
    let (env, _, _) = reviewed_on(Env::with_branch());
    env.git(&["remote", "add", "origin", "git@github.com:acme/app.git"]);
    env.git(&["switch", "-q", "main"]);
    let mut pull = gh_pull(9, "feat", "2026-10-20T08:00:00Z");
    pull["state"] = json!("CLOSED");
    pull["closedAt"] = json!("2026-10-21T08:00:00Z");
    respond_one_pull(&env, "acme", 9, Some(pull));
    let status = top(&env, &["9"]);
    assert_eq!(status["branch"], "feat");
}

#[test]
fn a_missing_pull_number_is_unknown() {
    let (env, _, _) = reviewed_on(Env::with_branch());
    env.git(&["remote", "add", "origin", "git@github.com:acme/app.git"]);
    env.git(&["switch", "-q", "main"]);
    respond_one_pull(&env, "acme", 9, None);
    let error = failure(&agt(&env, &["status"], &["9"], &[]));
    assert!(error.contains("9 is no known PR/MR"), "{error}");
}

#[test]
fn a_fork_pull_with_the_parents_number_and_branch_takes_no_parent_target() {
    let setup = parent_review_in_a_fork();
    let mut pull = gh_pull(42, "feat", "2026-10-20T08:00:00Z");
    pull["url"] = json!("https://github.com/me/app/pull/42");
    respond_open_pulls_of(&setup.env, "me", &[pull]);
    let summary = success(&agt(&setup.env, &["status"], &["--all"], &[]));
    let rows = summary["targets"].as_array().unwrap();
    let own = rows
        .iter()
        .find(|row| row["url"] == "https://github.com/me/app/pull/42")
        .unwrap();
    assert_eq!(own["target"], Value::Null, "{summary}");
    assert!(
        rows.iter()
            .any(|row| row["mr"] == 42 && row["target"] != Value::Null),
        "{summary}"
    );
}

#[test]
fn a_closed_pull_of_the_current_branch_does_not_report_another_pulls_review() {
    let setup = parent_review_in_a_fork();
    let env = &setup.env;
    let mut pull = gh_pull(5, "feat", "2026-10-01T08:00:00Z");
    pull["url"] = json!("https://github.com/me/app/pull/5");
    pull["state"] = json!("CLOSED");
    respond_one_pull(env, "me", 5, Some(pull));
    let error = failure(&agt(env, &["status"], &["5"], &[]));
    assert!(
        error.contains("5 is PR/MR 5 of branch feat, which has no review or QA records here"),
        "{error}"
    );
}
