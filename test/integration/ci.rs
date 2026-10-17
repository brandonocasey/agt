//! `agt ci`, checked against gl-ci's own output: every `test/fixtures/ci/*.json` holds the fake
//! GitLab responses of one run, its arguments and the stdout, stderr and exit status the
//! gl-ci script produced for them (clock times as `HH:MM:SS`, its `gl-ci:` prefix as `agt ci:`).

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::support::Env;

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixtures/ci");

fn normalize(text: &str) -> String {
    let mut out = String::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut at = 0;
    while at < bytes.len() {
        let clock = at + 8 <= bytes.len()
            && bytes[at..at + 8].iter().enumerate().all(|(i, c)| {
                if i == 2 || i == 5 {
                    *c == ':'
                } else {
                    c.is_ascii_digit()
                }
            });
        if clock {
            out.push_str("HH:MM:SS");
            at += 8;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    out
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap())
        .collect()
}

fn run_case(path: &Path) {
    let case: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let name = path.file_stem().unwrap().to_string_lossy();
    let env = Env::new();
    for response in case["responses"].as_array().unwrap() {
        let args = strings(&response["args"]);
        let body = response["body"].as_str().unwrap();
        if let Some(stderr) = response.get("fail") {
            env.fail_call_with(&args, stderr.as_str().unwrap());
        } else if let Some(nth) = response.get("nth") {
            env.respond_nth(&args, usize::try_from(nth.as_u64().unwrap()).unwrap(), body);
        } else {
            env.respond(&args, body);
        }
    }
    let mut command = env.command(env!("CARGO_BIN_EXE_agt"), &env.repo);
    command.arg("ci").args(strings(&case["args"]));
    for (key, value) in case["env"].as_object().unwrap() {
        command.env(key, value.as_str().unwrap());
    }
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(i32::try_from(case["code"].as_i64().unwrap()).unwrap()),
        "{name}: {stderr}"
    );
    assert_eq!(
        normalize(&String::from_utf8_lossy(&output.stdout)),
        case["stdout"].as_str().unwrap(),
        "{name}: stdout"
    );
    if let Some(expected) = case["stderr"].as_str() {
        assert_eq!(normalize(&stderr), expected, "{name}: stderr");
    }
}

fn case(name: &str) {
    run_case(&Path::new(FIXTURES).join(format!("{name}.json")));
}

#[test]
fn failures_compare_an_mr_with_the_default_branch() {
    case("failures-mix");
}

#[test]
fn failures_without_failed_jobs_say_so() {
    case("failures-clean");
}

#[test]
fn failures_without_a_pipeline_exit_2() {
    case("failures-no-pipeline");
}

#[test]
fn failures_warn_when_the_pipeline_is_older_than_the_newest_commit() {
    case("failures-older-pipeline");
    case("failures-merged-results");
}

#[test]
fn failures_without_default_branch_history_report_every_spec_as_new() {
    case("failures-no-history");
}

#[test]
fn failures_reject_pipelines_bad_targets_and_api_errors() {
    case("failures-pipeline-url");
    case("failures-bad-target");
    case("failures-n-zero");
    case("failures-api-error");
}

#[test]
fn failures_take_the_host_from_an_mr_url() {
    case("failures-mr-url");
    let env = Env::new();
    let fixture: Value = serde_json::from_slice(
        &fs::read(Path::new(FIXTURES).join("failures-mr-url.json")).unwrap(),
    )
    .unwrap();
    for response in fixture["responses"].as_array().unwrap() {
        env.respond(
            &strings(&response["args"]),
            response["body"].as_str().unwrap(),
        );
    }
    let output = env
        .command(env!("CARGO_BIN_EXE_agt"), &env.repo)
        .args(["ci", "failures"])
        .arg("https://gitlab.example.com/group/proj/-/merge_requests/5")
        .env("GITLAB_HOST", "ignored.example.com")
        .output()
        .unwrap();
    assert!(output.status.success());
    let calls = env.forge_calls();
    assert!(!calls.is_empty());
    assert!(
        calls
            .iter()
            .all(|call| call.starts_with("gitlab.example.com\t")),
        "{calls:?}"
    );
}

#[test]
fn wait_reports_success_failure_and_other_endings() {
    for name in [
        "wait-success",
        "wait-failed",
        "wait-canceled",
        "wait-manual",
        "wait-pipeline-url",
        "wait-mr-url",
    ] {
        case(name);
    }
}

#[test]
fn wait_prints_progress_only_when_it_changes() {
    case("wait-progress");
}

#[test]
fn wait_for_an_mr_waits_for_the_pipeline_of_the_sha() {
    case("wait-mr-sha");
    case("wait-mr-no-pipeline");
}

#[test]
fn wait_times_out_with_status_3() {
    case("wait-timeout");
}

#[test]
fn wait_gives_up_after_five_api_errors_in_a_row() {
    case("wait-api-errors");
    case("wait-bad-target");
}
