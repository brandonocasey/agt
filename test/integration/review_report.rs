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
    env.run(&["sync"]);
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
    env.run(&["sync"]);
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
