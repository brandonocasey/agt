//! `agt ci failures` for a GitHub or Forgejo PR. A fake gh/tea answers the exact calls.

use std::process::{Output, Stdio};

use serde_json::{Value, json};

use crate::support::forge::argv::{gh_raw, tea_get};
use crate::support::forge::forgejo;
use crate::support::{Env, strs};

fn pull_query(owner: &str, name: &str) -> Vec<String> {
    [
        "api",
        "graphql",
        "-f",
        &format!("owner={owner}"),
        "-f",
        &format!("name={name}"),
        "-F",
        "number=42",
        "-f",
        "query=PullChecks",
    ]
    .map(String::from)
    .to_vec()
}

fn failures(env: &Env, target: &str) -> Output {
    env.bin(&env.repo)
        .args(["ci", "failures", target])
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn check_run(id: u64, name: &str, conclusion: &str) -> Value {
    json!({
        "__typename": "CheckRun", "databaseId": id, "name": name, "status": "COMPLETED",
        "conclusion": conclusion, "title": "",
        "detailsUrl": format!("https://github.com/owner/repo/actions/runs/7/job/{id}"),
        "isRequired": true,
        "checkSuite": {"workflowRun": {"event": "pull_request", "workflow": {"name": "CI"}}},
    })
}

fn status_context(context: &str, state: &str) -> Value {
    json!({
        "__typename": "StatusContext", "context": context, "state": state, "description": "",
        "targetUrl": "https://ci.example.com/build/1", "isRequired": false,
    })
}

fn pull(contexts: &[Value]) -> String {
    json!({"data": {"repository": {"pullRequest": {
        "number": 42, "url": "https://github.com/owner/repo/pull/42", "baseRefName": "main",
        "state": "OPEN", "mergedAt": null, "closed": false,
        "headRefOid": "0123456789abcdef0123456789abcdef01234567",
        "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
            "pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": contexts,
        }}}}]},
    }}}})
    .to_string()
}

fn github_repo(owner: &str, name: &str, contexts: &[Value]) -> Env {
    let env = Env::new();
    env.git(&[
        "remote",
        "add",
        "origin",
        &format!("https://github.com/{owner}/{name}.git"),
    ]);
    env.respond(&strs(&pull_query(owner, name)), &pull(contexts));
    env
}

fn github(contexts: &[Value]) -> Env {
    github_repo("owner", "repo", contexts)
}

/// A Jest failure under the timestamps GitHub puts in front of every log line.
const JEST_LOG: &str = "2026-01-01T00:00:00.0000000Z ##[group]Run npm test\n\
2026-01-01T00:00:01.0000000Z   ● Login › shows an error for a bad password\n\
2026-01-01T00:00:02.0000000Z Tests: 1 failed, 4 passed\n";

#[test]
fn a_github_pr_lists_failed_checks_and_the_failing_specs_of_their_logs() {
    let env = github(&[
        check_run(501, "build", "SUCCESS"),
        check_run(502, "unit", "FAILURE"),
        status_context("ci/external", "FAILURE"),
    ]);
    env.respond(
        &strs(&gh_raw("repos/owner/repo/actions/jobs/502/logs")),
        JEST_LOG,
    );
    for target in [
        "owner/repo#42",
        "#42",
        "https://github.com/owner/repo/pull/42",
    ] {
        let output = failures(&env, target);
        assert!(output.status.success(), "{target}: {}", stderr(&output));
        let text = stdout(&output);
        assert!(
            text.contains("owner/repo#42 https://github.com/owner/repo/pull/42"),
            "{text}"
        );
        assert!(
            text.contains("head 01234567: 2 failed of 3 checks, 0 still running"),
            "{text}"
        );
        let row = text.lines().find(|line| line.contains("unit")).unwrap();
        assert!(row.contains("502") && row.contains("FAILURE"), "{row}");
        assert!(
            row.contains("https://github.com/owner/repo/actions/runs/7/job/502"),
            "{row}"
        );
        let status = text
            .lines()
            .find(|line| line.contains("ci/external"))
            .unwrap();
        assert!(
            status.trim_start().starts_with('-'),
            "a status has no id: {status}"
        );
        assert!(
            !text.contains("build  "),
            "a passing check is not listed: {text}"
        );
        assert!(
            text.contains("Login › shows an error for a bad password"),
            "{text}"
        );
        assert!(!text.contains("verdict"), "{text}");
    }
    // Only the failed Actions job has a log to read; the external status has none.
    let logs = env
        .forge_calls()
        .into_iter()
        .filter(|call| call.contains("/logs"))
        .count();
    assert_eq!(logs, 3);
}

#[test]
fn a_colored_github_log_still_yields_its_failing_specs() {
    let env = github(&[check_run(502, "unit", "FAILURE")]);
    env.respond(
        &strs(&gh_raw("repos/owner/repo/actions/jobs/502/logs")),
        "\u{1b}]0;job\u{7}2026-01-01T00:00:00.0000000Z \u{1b}[36;1mRun npm test\u{1b}[0m\n\
2026-01-01T00:00:01.0000000Z \u{1b}[31m  ● Login › colored failure\u{1b}[0m\u{1b}[K\n\
2026-01-01T00:00:02.0000000Z \u{1b}]0;title\u{7}Tests: 1 failed, 4 passed\n",
    );
    let output = failures(&env, "owner/repo#42");
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("Login › colored failure"), "{text}");
    assert!(!text.contains('\u{1b}'), "{text:?}");
}

/// A WebdriverIO failure as a GitHub runner logs it. `checkout` is the path after
/// `/home/runner/work/`; the runner checks out to `work/<repo>/<repo>`.
fn wdio_log(checkout: &str) -> String {
    format!(
        "2026-01-01T00:00:00.0000000Z ##[group]Run npm run e2e\n\
2026-01-01T00:00:01.0000000Z [0-0] FAILED in chrome - file:///home/runner/work/{checkout}/e2e/a.e2e.ts\n\
2026-01-01T00:00:02.0000000Z Spec Files: 0 passed, 1 failed, 1 total\n"
    )
}

#[test]
fn a_webdriverio_spec_is_shown_relative_to_the_runners_checkout() {
    // A repository named like the end of `work` must not match inside it.
    for (owner, name) in [("owner", "repo"), ("o", "ork")] {
        let env = github_repo(owner, name, &[check_run(502, "e2e", "FAILURE")]);
        env.respond(
            &strs(&gh_raw(&format!(
                "repos/{owner}/{name}/actions/jobs/502/logs"
            ))),
            &wdio_log(&format!("{name}/{name}")),
        );
        let output = failures(&env, &format!("{owner}/{name}#42"));
        assert!(output.status.success(), "{name}: {}", stderr(&output));
        let text = stdout(&output);
        let row = text.lines().find(|line| line.contains("a.e2e.ts")).unwrap();
        assert_eq!(
            row.split_whitespace().collect::<Vec<_>>(),
            ["e2e/a.e2e.ts", "e2e"],
            "{name}"
        );
        assert!(!text.contains("home/runner"), "{name}: {text}");
    }
}

#[test]
fn cancelled_checks_are_listed_with_the_failed_ones_and_their_logs_are_not_read() {
    let env = github(&[
        check_run(501, "build", "SUCCESS"),
        check_run(502, "unit", "FAILURE"),
        check_run(504, "deploy", "CANCELLED"),
    ]);
    env.respond(
        &strs(&gh_raw("repos/owner/repo/actions/jobs/502/logs")),
        JEST_LOG,
    );
    let output = failures(&env, "owner/repo#42");
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("head 01234567: 1 failed, 1 cancelled of 3 checks, 0 still running"),
        "{text}"
    );
    let row = text.lines().find(|line| line.contains("deploy")).unwrap();
    assert!(row.contains("504") && row.contains("CANCELLED"), "{row}");
    assert!(!text.contains("No failed"), "{text}");
    let logs: Vec<String> = env
        .forge_calls()
        .into_iter()
        .filter(|call| call.contains("/logs"))
        .collect();
    assert_eq!(logs.len(), 1, "{logs:?}");
    assert!(logs[0].contains("jobs/502/logs"), "{logs:?}");
}

#[test]
fn a_pr_with_only_a_cancelled_check_lists_it() {
    let env = github(&[check_run(504, "deploy", "CANCELLED")]);
    let output = failures(&env, "#42");
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("head 01234567: 0 failed, 1 cancelled of 1 checks"),
        "{text}"
    );
    assert!(text.contains("CANCELLED"), "{text}");
    assert!(!text.contains("No failed"), "{text}");
}

#[test]
fn a_github_pr_without_failures_says_so_and_reads_no_log() {
    let env = github(&[
        check_run(501, "build", "SUCCESS"),
        check_run(503, "slow", "NEUTRAL"),
    ]);
    let output = failures(&env, "#42");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("No failed or cancelled checks."),
        "{}",
        stdout(&output)
    );
    assert!(env.forge_calls().iter().all(|call| !call.contains("/logs")));
}

#[test]
fn an_unreadable_log_warns_and_the_failed_check_is_still_listed() {
    let env = github(&[check_run(502, "unit", "FAILURE")]);
    env.fail_call_with(
        &strs(&gh_raw("repos/owner/repo/actions/jobs/502/logs")),
        "gh: Not Found (HTTP 404)",
    );
    let output = failures(&env, "owner/repo#42");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("502"), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("could not read the log of job 502"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_github_pr_that_does_not_exist_exits_2() {
    let env = Env::new();
    env.respond(
        &strs(&pull_query("owner", "repo")),
        r#"{"data": {"repository": {"pullRequest": null}}}"#,
    );
    let output = failures(&env, "owner/repo#42");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("owner/repo#42 was not found"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn failures_takes_a_pr_not_a_run_or_a_commit() {
    let env = Env::new();
    for (target, message) in [
        (
            "https://github.com/owner/repo/actions/runs/55",
            "failures takes a PR, not an Actions run",
        ),
        ("owner/repo@abc123", "failures takes no @SHA"),
    ] {
        let output = failures(&env, target);
        assert_eq!(output.status.code(), Some(2), "{target}");
        assert!(
            stderr(&output).contains(message),
            "{target}: {}",
            stderr(&output)
        );
    }
    assert!(env.forge_calls().is_empty());
}

#[test]
fn a_forgejo_pr_lists_failed_statuses_and_no_specs() {
    let env = Env::new();
    forgejo::logins(&env);
    let sha = "0123456789abcdef0123456789abcdef01234567";
    env.respond(
        &strs(&tea_get("repos/o/r/pulls/5")),
        &json!({
            "number": 5, "html_url": "https://codeberg.org/o/r/pulls/5", "state": "open",
            "merged": false, "merged_at": null, "head": {"ref": "feat", "sha": sha},
        })
        .to_string(),
    );
    env.respond(
        &strs(&tea_get(&format!("repos/o/r/commits/{sha}/status"))),
        &json!({"statuses": [
            {"id": 31, "context": "ci/build", "status": "failure", "description": "",
             "target_url": "https://codeberg.org/o/r/actions/runs/1"},
            {"id": 32, "context": "ci/lint", "status": "success", "description": "",
             "target_url": "https://codeberg.org/o/r/actions/runs/2"},
        ]})
        .to_string(),
    );
    let output = failures(&env, "https://codeberg.org/o/r/pulls/5");
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("head 01234567: 1 failed of 2 checks"),
        "{text}"
    );
    let row = text.lines().find(|line| line.contains("ci/build")).unwrap();
    assert!(
        row.contains("31") && row.contains("https://codeberg.org/o/r/actions/runs/1"),
        "{row}"
    );
    assert!(!text.contains("ci/lint"), "{text}");
    assert!(
        text.contains("agt does not read job logs on Forgejo"),
        "{text}"
    );
    assert!(env.forge_calls().iter().all(|call| !call.contains("/logs")));
}

#[test]
fn a_bare_hash_failures_target_uses_the_origin_tracker() {
    let env = Env::new();
    env.git(&["remote", "add", "origin", "https://codeberg.org/o/r.git"]);
    forgejo::logins(&env);
    env.respond(
        &strs(&tea_get("repos/o/r/pulls/5")),
        &json!({"number": 5, "html_url": "u", "state": "open", "merged": false,
            "merged_at": null, "head": {"ref": "f", "sha": "abc123"}})
        .to_string(),
    );
    env.respond(
        &strs(&tea_get("repos/o/r/commits/abc123/status")),
        &json!({"statuses": []}).to_string(),
    );
    let output = failures(&env, "#5");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("o/r#5"), "{}", stdout(&output));
}

#[test]
fn a_closed_stdout_stops_a_github_failures_run_before_it_reads_the_job_logs() {
    let env = github(&[check_run(502, "unit", "FAILURE")]);
    env.respond(
        &strs(&gh_raw("repos/owner/repo/actions/jobs/502/logs")),
        JEST_LOG,
    );
    let mut child = env
        .bin(&env.repo)
        .args(["ci", "failures", "owner/repo#42"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stderr(&output), "");
    let logs: Vec<String> = env
        .forge_calls()
        .into_iter()
        .filter(|call| call.contains("/logs"))
        .collect();
    assert_eq!(logs, Vec::<String>::new());
}
