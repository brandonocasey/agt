//! `agt ci log` and `agt ci retry`. A fake gh/glab/tea answers the exact calls the commands make.

use std::process::Output;

use serde_json::Value;

use crate::support::forge::argv::{gh_get, gh_raw, glab_get};
use crate::support::forge::forgejo;
use crate::support::{Env, args, strs};

const GITLAB_JOB: &str = "https://gitlab.example.test/group/sub/project/-/jobs/9";
const GITHUB_JOB: &str = "https://github.com/owner/repo/actions/runs/55/job/9";
const GITHUB_RUN: &str = "https://github.com/owner/repo/actions/runs/55";
const FORGEJO_JOB: &str = "https://codeberg.org/o/r/actions/runs/4/jobs/1";
const GITLAB_TRACE: &str = "projects/group%2Fsub%2Fproject/jobs/9/trace";
const GITHUB_LOG: &str = "repos/owner/repo/actions/jobs/9/logs";

fn in_repo(origin: &str) -> Env {
    let env = Env::new();
    env.git(&["remote", "add", "origin", origin]);
    env
}

fn ci(env: &Env, command: &[&str]) -> Output {
    env.bin(&env.repo).arg("ci").args(command).output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A log of the lines `line 1` to `line <count>`.
fn lines(count: usize) -> String {
    (1..=count).map(|n| format!("line {n}\n")).collect()
}

fn respond(env: &Env, request: &[String], body: &str) {
    env.respond(&strs(request), body);
}

fn gitlab_post(path: &str) -> Vec<String> {
    args(&["api", "-X", "POST", path])
}

fn github_post(path: &str) -> Vec<String> {
    args(&["api", "--method", "POST", path])
}

fn posts(env: &Env) -> Vec<String> {
    env.forge_calls()
        .into_iter()
        .filter(|call| call.contains("POST"))
        .collect()
}

#[test]
fn log_prints_the_last_200_lines_of_a_gitlab_job_url_by_default() {
    let env = Env::new();
    respond(&env, &glab_get(GITLAB_TRACE), &lines(250));
    let output = ci(&env, &["log", GITLAB_JOB]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        lines(250)
            .split_inclusive('\n')
            .skip(50)
            .collect::<String>()
    );
    let calls = env.forge_calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].starts_with("gitlab.example.test\t"), "{calls:?}");
}

#[test]
fn log_tail_limits_the_lines_and_a_short_log_prints_whole() {
    let env = Env::new();
    respond(&env, &glab_get(GITLAB_TRACE), &lines(10));
    let output = ci(&env, &["log", GITLAB_JOB, "--tail", "3"]);
    assert_eq!(stdout(&output), "line 8\nline 9\nline 10\n");
    let output = ci(&env, &["log", GITLAB_JOB, "--tail", "50"]);
    assert_eq!(stdout(&output), lines(10));
    respond(
        &env,
        &glab_get("projects/group%2Fsub%2Fproject/jobs/8/trace"),
        "",
    );
    let output = ci(
        &env,
        &[
            "log",
            "https://gitlab.example.test/group/sub/project/-/jobs/8",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    let output = ci(&env, &["log", GITLAB_JOB, "--tail", "0"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("at least 1"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn log_reads_a_gitlab_job_id_in_the_current_repositorys_tracker() {
    let env = in_repo("https://gitlab.com/g/p.git");
    respond(&env, &glab_get("projects/g%2Fp/jobs/9/trace"), &lines(4));
    let output = ci(&env, &["log", "9", "--tail", "2"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "line 3\nline 4\n");
}

#[test]
fn log_reads_a_github_job_by_url_and_by_id() {
    let env = Env::new();
    respond(&env, &gh_raw(GITHUB_LOG), &lines(6));
    let output = ci(&env, &["log", GITHUB_JOB, "--tail", "2"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "line 5\nline 6\n");
    let env = in_repo("https://github.com/owner/repo.git");
    respond(&env, &gh_raw(GITHUB_LOG), &lines(6));
    let output = ci(&env, &["log", "9", "--tail", "1"]);
    assert_eq!(stdout(&output), "line 6\n");
}

/// A log with escapes and lone carriage returns, as a runner prints.
const COLORED_LOG: &str = "\u{1b}[31;1merror\u{1b}[0m: build failed\r\n\u{1b}]0;title\u{7}next\u{1b}[2K\ttabbed\n\u{1b}]8;;https://x.test\u{1b}\\link\u{1b}]8;;\u{1b}\\\nbell\u{7}end\nsection_end:1:a\r\u{1b}[0Ksection_start:2:b\r\u{1b}[0KHeader\n";
const CLEAN_LOG: &str = "error: build failed\nnext\ttabbed\nlink\nbellend\nsection_end:1:a\nsection_start:2:b\nHeader\n";

#[test]
fn log_prints_a_colored_log_without_escape_sequences() {
    let env = Env::new();
    respond(&env, &glab_get(GITLAB_TRACE), COLORED_LOG);
    let output = ci(&env, &["log", GITLAB_JOB]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), CLEAN_LOG);
    let output = ci(&env, &["log", GITLAB_JOB, "--tail", "3"]);
    assert_eq!(
        stdout(&output),
        "section_end:1:a\nsection_start:2:b\nHeader\n"
    );
    let env = Env::new();
    respond(&env, &gh_raw(GITHUB_LOG), COLORED_LOG);
    let output = ci(&env, &["log", GITHUB_JOB]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), CLEAN_LOG);
    let calls = env.forge_calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].contains("--allow-escape-sequences"), "{calls:?}");
}

#[test]
fn log_reads_a_github_job_with_a_gh_that_rejects_the_escape_flag() {
    let env = Env::new();
    env.fail_call_with(
        &strs(&gh_raw(GITHUB_LOG)),
        "unknown flag: --allow-escape-sequences",
    );
    respond(&env, &gh_get(GITHUB_LOG), COLORED_LOG);
    let output = ci(&env, &["log", GITHUB_JOB]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), CLEAN_LOG);
    let calls = env.forge_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[0].contains("--allow-escape-sequences"), "{calls:?}");
    assert!(!calls[1].contains("--allow-escape-sequences"), "{calls:?}");
}

#[test]
fn log_of_an_unknown_job_exits_2_and_says_so() {
    let env = Env::new();
    env.fail_call_with(
        &strs(&glab_get(GITLAB_TRACE)),
        "glab: 404 Not Found (HTTP 404)",
    );
    let output = ci(&env, &["log", GITLAB_JOB]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("job 9 was not found in gitlab.example.test/group/sub/project"),
        "{}",
        stderr(&output)
    );
    env.fail_call_with(&strs(&gh_raw(GITHUB_LOG)), "gh: Not Found (HTTP 404)");
    let output = ci(&env, &["log", GITHUB_JOB]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("job 9 was not found in github.com/owner/repo (or has no log)"),
        "{}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
}

#[test]
fn log_refuses_a_forgejo_job_without_calling_tea() {
    let env = Env::new();
    for job in [FORGEJO_JOB, "https://codeberg.org/o/r/actions/runs/4"] {
        let output = ci(&env, &["log", job]);
        assert_eq!(output.status.code(), Some(2), "{job}");
        assert!(
            stderr(&output).contains("agt does not support reading job logs on Forgejo"),
            "{}",
            stderr(&output)
        );
    }
    let env = in_repo("https://codeberg.org/o/r.git");
    let output = ci(&env, &["log", "9"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("agt does not support reading job logs on Forgejo"),
        "{}",
        stderr(&output)
    );
    assert!(env.forge_calls().is_empty());
}

#[test]
fn log_takes_a_job_not_a_run_or_anything_else() {
    let env = Env::new();
    for bad in [GITHUB_RUN, "build", "-1"] {
        let output = ci(&env, &["log", bad]);
        assert_eq!(output.status.code(), Some(2), "{bad}");
    }
    let output = ci(&env, &["log", GITHUB_RUN]);
    assert!(
        stderr(&output).contains("takes a job, not a run"),
        "{}",
        stderr(&output)
    );
    assert!(env.forge_calls().is_empty());
}

/// Actions URLs that are no target. Nothing may be sent for them.
const BAD_ACTIONS_URLS: [&str; 9] = [
    "https://github.com/owner/repo/actions/runs/55/jobs/9",
    "https://github.com/owner/repo/actions/runs/55/workflow",
    "https://github.com/owner/repo/actions/runs/55/job/9/extra",
    "https://github.com/owner/repo/actions/runs/55/attempts/x",
    "https://github.com/owner/repo/actions/runs/55/attempts/2/extra",
    "https://github.com/owner/repo/actions/runs/55/job/99999999999999999999",
    "https://github.com/owner/repo/actions/runs/0",
    "https://github.com/owner/repo/actions/runs/0/job/9",
    "https://github.com/owner/repo/actions/runs/55/job/0",
];

#[test]
fn retry_and_log_reject_an_actions_url_with_a_trailing_path_and_send_nothing() {
    let env = in_repo("https://github.com/owner/repo.git");
    for command in ["retry", "log"] {
        for url in BAD_ACTIONS_URLS {
            let output = ci(&env, &[command, url]);
            assert_eq!(output.status.code(), Some(2), "{command} {url}");
            assert!(
                stderr(&output).contains("expected a GitLab job URL"),
                "{command} {url}: {}",
                stderr(&output)
            );
            assert!(stdout(&output).is_empty(), "{command} {url}");
        }
    }
    assert!(env.forge_calls().is_empty());
}

#[test]
fn retry_takes_an_actions_run_url_with_a_slash_attempt_query_or_fragment() {
    let env = Env::new();
    respond(
        &env,
        &github_post("repos/owner/repo/actions/runs/55/rerun-failed-jobs"),
        "{}",
    );
    respond(
        &env,
        &github_post("repos/owner/repo/actions/jobs/9/rerun"),
        "{}",
    );
    for tail in [
        "/",
        "/attempts/2",
        "?pr=1",
        "#summary",
        "/attempts/2?pr=1#x",
    ] {
        let url = format!("{GITHUB_RUN}{tail}");
        let output = ci(&env, &["retry", &url]);
        assert!(output.status.success(), "{url}: {}", stderr(&output));
        let done: Value = serde_json::from_str(&stdout(&output)).unwrap();
        assert_eq!(done["retried"], "failed_jobs", "{url}");
    }
    let url = format!("{GITHUB_JOB}/attempts/2");
    let output = ci(&env, &["retry", &url]);
    assert!(output.status.success(), "{url}: {}", stderr(&output));
    assert_eq!(posts(&env).len(), 6);
}

#[test]
fn retry_starts_a_gitlab_job_again_and_names_the_new_one() {
    let env = Env::new();
    respond(
        &env,
        &gitlab_post("projects/group%2Fsub%2Fproject/jobs/9/retry"),
        r#"{"id": 10, "status": "pending", "web_url": "https://gitlab.example.test/group/sub/project/-/jobs/10"}"#,
    );
    let output = ci(&env, &["retry", GITLAB_JOB]);
    assert!(output.status.success(), "{}", stderr(&output));
    let done: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(done["forge"], "gitlab");
    assert_eq!(done["host"], "gitlab.example.test");
    assert_eq!(done["repo"], "group/sub/project");
    assert_eq!(
        (done["retried"].as_str(), done["id"].as_u64()),
        (Some("job"), Some(9))
    );
    assert_eq!(done["new_job"]["id"], 10);
    assert_eq!(done["new_job"]["status"], "pending");
    assert_eq!(posts(&env).len(), 1);
}

#[test]
fn retry_reruns_a_github_job_and_the_failed_jobs_of_a_run() {
    let env = Env::new();
    respond(
        &env,
        &github_post("repos/owner/repo/actions/jobs/9/rerun"),
        "{}",
    );
    respond(
        &env,
        &github_post("repos/owner/repo/actions/runs/55/rerun-failed-jobs"),
        "{}",
    );
    let job: Value = serde_json::from_str(&stdout(&ci(&env, &["retry", GITHUB_JOB]))).unwrap();
    assert_eq!(job["forge"], "github");
    assert_eq!(job["repo"], "owner/repo");
    assert_eq!(
        (job["retried"].as_str(), job["id"].as_u64()),
        (Some("job"), Some(9))
    );
    let run: Value = serde_json::from_str(&stdout(&ci(&env, &["retry", GITHUB_RUN]))).unwrap();
    assert_eq!(
        (run["retried"].as_str(), run["run"].as_u64()),
        (Some("failed_jobs"), Some(55))
    );
    assert_eq!(posts(&env).len(), 2);
}

#[test]
fn retry_reads_a_bare_job_id_in_the_current_repositorys_tracker() {
    let env = in_repo("https://github.com/owner/repo.git");
    respond(
        &env,
        &github_post("repos/owner/repo/actions/jobs/9/rerun"),
        "{}",
    );
    let output = ci(&env, &["retry", "9"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let env = in_repo("https://gitlab.com/g/p.git");
    respond(
        &env,
        &gitlab_post("projects/g%2Fp/jobs/9/retry"),
        r#"{"id": 10, "status": "pending", "web_url": "u"}"#,
    );
    let output = ci(&env, &["retry", "9"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(posts(&env).len(), 1);
}

#[test]
fn retry_is_refused_offline_without_a_call() {
    let env = in_repo("https://github.com/owner/repo.git");
    for target in [GITHUB_JOB, GITHUB_RUN, GITLAB_JOB, FORGEJO_JOB] {
        let flag = env
            .bin(&env.repo)
            .arg("--offline")
            .args(["ci", "retry", target])
            .output()
            .unwrap();
        let var = env
            .bin(&env.repo)
            .env("AGT_OFFLINE", "1")
            .args(["ci", "retry", target])
            .output()
            .unwrap();
        for output in [flag, var] {
            assert_eq!(output.status.code(), Some(2), "{target}");
            assert!(
                stderr(&output).contains("offline mode forbids agt ci"),
                "{target}"
            );
        }
    }
    assert!(env.forge_calls().is_empty());
}

#[test]
fn retry_is_refused_when_push_is_explicitly_off_and_nothing_is_sent() {
    for (origin, target) in [
        ("https://github.com/owner/repo.git", GITHUB_JOB),
        ("https://github.com/owner/repo.git", GITHUB_RUN),
        (
            "https://gitlab.com/g/p.git",
            "https://gitlab.com/g/p/-/jobs/9",
        ),
    ] {
        let env = in_repo(origin);
        env.write_config("[pull_requests]\npush = \"off\"\n");
        let output = ci(&env, &["retry", target]);
        assert_eq!(output.status.code(), Some(2), "{target}");
        assert!(
            stderr(&output).contains("refused: push is off for"),
            "{}",
            stderr(&output)
        );
        assert!(stdout(&output).is_empty());
        assert!(env.forge_calls().is_empty(), "{target}");
    }
}

#[test]
fn retry_outside_a_repository_follows_the_global_push_setting() {
    let env = Env::new();
    let elsewhere = env.path("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    respond(
        &env,
        &github_post("repos/owner/repo/actions/jobs/9/rerun"),
        "{}",
    );
    let retry = |env: &Env| {
        env.bin(&elsewhere)
            .args(["ci", "retry", GITHUB_JOB])
            .output()
            .unwrap()
    };
    assert!(retry(&env).status.success());
    env.write_config("[pull_requests]\npush = \"off\"\n");
    let output = retry(&env);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("refused: push is off for github.com/owner/repo"));
    assert_eq!(posts(&env).len(), 1);
}

#[test]
fn retry_of_an_unknown_job_exits_2() {
    let env = Env::new();
    env.fail_call_with(
        &strs(&github_post("repos/owner/repo/actions/jobs/9/rerun")),
        "gh: Not Found (HTTP 404)",
    );
    let output = ci(&env, &["retry", GITHUB_JOB]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("job 9 was not found in github.com/owner/repo"),
        "{}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("no log"),
        "a retry reads no log: {}",
        stderr(&output)
    );
    assert!(stdout(&output).is_empty());
}

#[test]
fn retry_refuses_a_forgejo_target_without_calling_tea() {
    let env = in_repo("https://codeberg.org/o/r.git");
    forgejo::logins(&env);
    for target in [FORGEJO_JOB, "9"] {
        let output = ci(&env, &["retry", target]);
        assert_eq!(output.status.code(), Some(2), "{target}");
        assert!(
            stderr(&output).contains("agt does not support rerunning jobs on Forgejo"),
            "{}",
            stderr(&output)
        );
    }
    assert!(posts(&env).is_empty());
}
