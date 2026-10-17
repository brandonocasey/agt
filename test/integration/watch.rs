//! `agt watch`, ported from agent-watch's Node tests: a fake gh/glab replays numbered steps
//! (`test/fixtures/fake-steps.sh`) and the events on stdout and the exit status are checked.

use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use assert_fs::TempDir;
use serde_json::{Value, json};

const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixtures/fake-steps.sh");
const GITLAB_TARGET: &str = "group/sub/project!7@abc123";
const GITLAB_PROJECT: &str = "projects/group%2Fsub%2Fproject";

struct Step {
    includes: String,
    stdout: Option<String>,
    stderr: Option<String>,
    code: i32,
    delay: Option<f64>,
}

fn step(includes: &str, stdout: &str) -> Step {
    Step {
        includes: includes.to_owned(),
        stdout: Some(stdout.to_owned()),
        stderr: None,
        code: 0,
        delay: None,
    }
}

fn failing(includes: &str, stderr: &str, code: i32) -> Step {
    Step {
        includes: includes.to_owned(),
        stdout: None,
        stderr: Some(stderr.to_owned()),
        code,
        delay: None,
    }
}

impl Step {
    fn code(mut self, code: i32) -> Self {
        self.code = code;
        self
    }

    fn delay(mut self, seconds: f64) -> Self {
        self.delay = Some(seconds);
        self
    }
}

struct Scenario {
    _temp: TempDir,
    dir: std::path::PathBuf,
}

impl Scenario {
    fn new(steps: &[Step]) -> Self {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().canonicalize().unwrap();
        for (index, step) in steps.iter().enumerate() {
            let put = |ext: &str, text: &str| {
                fs::write(dir.join(format!("{index}.{ext}")), text).unwrap()
            };
            put("includes", &step.includes);
            if let Some(text) = &step.stdout {
                put("stdout", text);
            }
            if let Some(text) = &step.stderr {
                put("stderr", text);
            }
            put("code", &step.code.to_string());
            if let Some(seconds) = step.delay {
                put("delay", &seconds.to_string());
            }
        }
        Self { _temp: temp, dir }
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agt"));
        command
            .arg("watch")
            .args(args)
            .env("HOME", &self.dir)
            .env_remove("AGT_GH_BIN")
            .env_remove("AGT_GLAB_BIN")
            .env("AGT_GH_BIN", FAKE)
            .env("AGT_GLAB_BIN", FAKE)
            .env("FAKE_STEPS_DIR", &self.dir)
            .env("GITLAB_HOST", "");
        for (key, value) in env {
            command.env(key, value);
        }
        command
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

struct Outcome {
    status: i32,
    stderr: String,
    events: Vec<Value>,
}

fn events(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

const FAST: [&str; 6] = [
    "--interval-ms",
    "1",
    "--max-interval-ms",
    "2",
    "--deadline-seconds",
    "2",
];

fn run_with(steps: &[Step], target: &str, extra: &[&str], env: &[(&str, &str)]) -> Outcome {
    let scenario = Scenario::new(steps);
    let mut args = vec!["--target", target];
    args.extend(FAST);
    args.extend(extra);
    let output = scenario.command(&args, env).output().unwrap();
    Outcome {
        status: output.status.code().unwrap(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        events: events(&output.stdout),
    }
}

fn run(steps: &[Step]) -> Outcome {
    run_with(steps, "owner/repo#42@abc123", &[], &[])
}

fn view(fields: Value) -> String {
    let mut base = json!({
        "number": 42,
        "url": "https://github.com/owner/repo/pull/42",
        "state": "OPEN",
        "mergedAt": null,
        "closed": false,
        "headRefOid": "abc123",
    });
    for (key, value) in fields.as_object().unwrap() {
        base[key] = value.clone();
    }
    base.to_string()
}

fn check(state: &str, bucket: &str) -> Value {
    json!({
        "name": "test", "state": state, "bucket": bucket, "description": "",
        "link": "https://github.com/owner/repo/actions/runs/1", "workflow": "CI",
        "event": "pull_request",
    })
}

fn checks(state: &str, bucket: &str) -> String {
    json!([check(state, bucket)]).to_string()
}

fn reasons(outcome: &Outcome) -> Vec<&str> {
    outcome
        .events
        .iter()
        .map(|event| event["reason"].as_str().unwrap())
        .collect()
}

fn names(outcome: &Outcome) -> Vec<&str> {
    outcome
        .events
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect()
}

#[test]
fn emits_initial_changed_and_final_events_once() {
    let open = view(json!({}));
    let outcome = run(&[
        step("pr view 42", &open),
        step("pr checks 42", &checks("IN_PROGRESS", "pending")).code(8),
        step("pr view 42", &open),
        step("pr checks 42", &checks("SUCCESS", "pass")),
        step("pr view 42", &open),
    ]);
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(names(&outcome), ["initial", "change", "final"]);
    assert_eq!(outcome.events[1]["reason"], "required_checks_passed");
    assert_eq!(outcome.events[2]["reason"], "checks_complete");
    assert_eq!(
        outcome.events[2]["results"][0]["reason"],
        "required_checks_passed"
    );
}

#[test]
fn reports_a_head_change_without_querying_checks() {
    let outcome = run(&[step("pr view 42", &view(json!({"headRefOid": "def456"})))]);
    assert_eq!(outcome.status, 2, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "head_changed");
    assert_eq!(outcome.events.last().unwrap()["reason"], "action_required");
}

#[test]
fn accepts_failed_check_json_from_gh_exit_status_1() {
    let mut failed = check("FAILURE", "fail");
    failed["description"] = json!("failed");
    let outcome = run(&[
        step("pr view 42", &view(json!({}))),
        step("pr checks 42", &json!([failed]).to_string()).code(1),
    ]);
    assert_eq!(outcome.status, 2, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "check_failed");
    assert_eq!(outcome.events[0]["checks"][0]["description"], "failed");
}

#[test]
fn rechecks_the_head_before_reporting_completed_checks() {
    let outcome = run(&[
        step("pr view 42", &view(json!({}))),
        step("pr checks 42", &checks("SUCCESS", "pass")),
        step("pr view 42", &view(json!({"headRefOid": "def456"}))),
    ]);
    assert_eq!(outcome.status, 2, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "head_changed");
    assert_eq!(outcome.events[0]["expectedHead"], "abc123");
    assert_eq!(outcome.events[0]["observedHead"], "def456");
}

#[test]
fn reports_merged_and_closed_pull_requests_without_querying_checks() {
    let cases = [
        (
            json!({"state": "MERGED", "mergedAt": "2026-10-02T12:00:00Z", "closed": true}),
            0,
            "merged",
        ),
        (
            json!({"state": "CLOSED", "mergedAt": null, "closed": true}),
            2,
            "closed",
        ),
    ];
    for (fields, status, reason) in cases {
        let outcome = run(&[step("pr view 42", &view(fields))]);
        assert_eq!(outcome.status, status, "{}", outcome.stderr);
        assert_eq!(outcome.events[0]["reason"], reason);
    }
}

#[test]
fn reports_no_checks_unknown_checks_failures_and_cancellation() {
    let cases = [
        (json!([]), "no_required_checks"),
        (
            json!([{"name": "test", "state": "UNKNOWN", "bucket": "unknown"}]),
            "unknown_check_state",
        ),
        (
            json!([{"name": "test", "state": "FAILURE", "bucket": "fail"}]),
            "check_failed",
        ),
        (
            json!([{"name": "test", "state": "CANCELLED", "bucket": "cancel"}]),
            "check_cancelled",
        ),
    ];
    for (list, reason) in cases {
        let outcome = run(&[
            step("pr view 42", &view(json!({}))),
            step("pr checks 42", &list.to_string()),
        ]);
        assert_eq!(outcome.status, 2, "{}", outcome.stderr);
        assert_eq!(outcome.events[0]["reason"], reason);
    }
}

#[test]
fn preserves_authentication_permission_and_invalid_response_output() {
    let cases = [
        (
            "authentication required: run gh auth login",
            "authentication_error",
            None,
        ),
        (
            "HTTP 403: Resource not accessible",
            "permission_error",
            None,
        ),
        ("warning from proxy", "invalid_response", Some("{broken")),
    ];
    for (detail, reason, stdout) in cases {
        let mut first = failing("pr view 42", detail, i32::from(stdout.is_none()));
        first.stdout = stdout.map(str::to_owned);
        let outcome = run(&[first]);
        assert_eq!(outcome.status, 2, "{}", outcome.stderr);
        assert_eq!(outcome.events[0]["reason"], reason);
        let text = outcome.events[0]["detail"].as_str().unwrap();
        assert!(text.contains(detail), "{text}");
    }
    let detail = "HTTP 403: Resource not accessible";
    let outcome = run(&[
        step("pr view 42", &view(json!({}))),
        failing("pr checks 42", detail, 1),
    ]);
    assert_eq!(outcome.status, 2, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "permission_error");
    assert_eq!(outcome.events[0]["detail"], detail);
}

#[test]
fn failures_name_the_command_exit_status_and_signal() {
    let outcome = run(&[failing("pr view 42", "boom", 4)]);
    let event = &outcome.events[0];
    assert_eq!(event["reason"], "query_error");
    assert_eq!(event["exitCode"], 4);
    assert_eq!(event["signal"], Value::Null);
    let command: Vec<&str> = event["command"]
        .as_array()
        .unwrap()
        .iter()
        .map(|part| part.as_str().unwrap())
        .collect();
    assert_eq!(
        &command[1..],
        [
            "pr",
            "view",
            "42",
            "--repo",
            "owner/repo",
            "--json",
            "number,url,state,mergedAt,closed,headRefOid"
        ]
    );
    let silent = run(&[failing("pr view 42", "", 5)]);
    assert!(
        silent.events[0]["detail"]
            .as_str()
            .unwrap()
            .ends_with("exited with status 5")
    );
}

#[test]
fn a_missing_tool_is_a_query_error() {
    let outcome = run_with(
        &[],
        "owner/repo#42@abc123",
        &[],
        &[("AGT_GH_BIN", "/nonexistent/gh")],
    );
    assert_eq!(outcome.status, 2, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "query_error");
    assert_eq!(outcome.events[0]["exitCode"], "ENOENT");
}

#[test]
fn reports_missing_pull_request_and_check_data() {
    let outcome = run(&[step(
        "pr view 42",
        &json!({"state": "OPEN", "url": "https://github.com/owner/repo/pull/42"}).to_string(),
    )]);
    assert_eq!(outcome.status, 2, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "missing_pull_request_data");

    let outcome = run(&[
        step("pr view 42", &view(json!({}))),
        step(
            "pr checks 42",
            &json!([{"name": "test", "state": "SUCCESS"}]).to_string(),
        ),
    ]);
    assert_eq!(outcome.status, 2, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "missing_check_data");
}

#[test]
fn sorts_checks_like_javascript_locale_compare() {
    // Node's `localeCompare` on the serialized checks: the closing quote after a short name
    // sorts after a space, an underscore and a hyphen.
    let list = json!([
        {"name": "Zed", "state": "SUCCESS", "bucket": "pass"},
        {"name": "alpha-2", "state": "SUCCESS", "bucket": "pass"},
        {"name": "alpha_1", "state": "SUCCESS", "bucket": "pass"},
        {"name": "Alpha", "state": "SUCCESS", "bucket": "pass"},
        {"name": "alpha", "state": "SUCCESS", "bucket": "pass"},
        {"name": "alpha 3", "state": "SUCCESS", "bucket": "pass"},
        {"name": "10", "state": "SUCCESS", "bucket": "pass"},
    ]);
    let open = view(json!({}));
    let outcome = run(&[
        step("pr view 42", &open),
        step("pr checks 42", &list.to_string()),
        step("pr view 42", &open),
    ]);
    let order: Vec<&str> = outcome.events[0]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        [
            "10", "alpha 3", "alpha_1", "alpha-2", "alpha", "Alpha", "Zed"
        ]
    );
}

#[test]
fn events_keep_agent_watch_field_order() {
    let scenario = Scenario::new(&[
        step("pr view 42", &view(json!({}))),
        step("pr checks 42", &checks("SUCCESS", "pass")),
        step("pr view 42", &view(json!({}))),
    ]);
    let mut args = vec!["--target", "owner/repo#42@abc123"];
    args.extend(FAST);
    let output = scenario.command(&args, &[]).output().unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    let at = &lines[0][..lines[0].find(",\"event\"").unwrap()];
    assert!(
        at.starts_with("{\"at\":\"20") && at.ends_with("Z\""),
        "{at}"
    );
    assert!(lines[0].ends_with(
        ",\"event\":\"initial\",\"target\":\"owner/repo#42\",\"status\":\"complete\",\"reason\":\"required_checks_passed\",\"checks\":[{\"name\":\"test\",\"state\":\"SUCCESS\",\"bucket\":\"pass\",\"description\":\"\",\"link\":\"https://github.com/owner/repo/actions/runs/1\",\"workflow\":\"CI\",\"event\":\"pull_request\"}]}"
    ), "{}", lines[0]);
    assert!(lines[1].contains(",\"event\":\"final\",\"reason\":\"checks_complete\",\"results\":[{\"target\":\"owner/repo#42\",\"status\":\"complete\""), "{}", lines[1]);
}

#[test]
fn stops_at_a_finite_deadline() {
    let outcome = run_with(
        &[
            step("pr view 42", &view(json!({}))),
            step("pr checks 42", &checks("IN_PROGRESS", "pending")).code(8),
        ],
        "owner/repo#42@abc123",
        &[
            "--interval-ms",
            "2000",
            "--max-interval-ms",
            "2000",
            "--deadline-seconds",
            "1",
        ],
        &[],
    );
    assert_eq!(outcome.status, 124, "{}", outcome.stderr);
    assert_eq!(names(&outcome), ["initial", "final"]);
    assert_eq!(
        outcome.events.last().unwrap()["reason"],
        "deadline_exceeded"
    );
}

#[test]
fn recomputes_the_deadline_before_each_invocation() {
    let started = Instant::now();
    let outcome = run_with(
        &[
            step("pr view 42", &view(json!({}))).delay(0.7),
            step("pr checks 42", &checks("IN_PROGRESS", "pending"))
                .code(8)
                .delay(0.7),
        ],
        "owner/repo#42@abc123",
        &[
            "--interval-ms",
            "2000",
            "--max-interval-ms",
            "2000",
            "--deadline-seconds",
            "1",
        ],
        &[],
    );
    let elapsed = started.elapsed();
    assert_eq!(outcome.status, 124, "{}", outcome.stderr);
    assert_eq!(
        outcome.events.last().unwrap()["reason"],
        "deadline_exceeded"
    );
    assert!(
        elapsed < Duration::from_millis(1300),
        "exceeded the deadline by {:?}",
        elapsed - Duration::from_secs(1)
    );
}

#[test]
fn emits_one_final_event_when_terminated() {
    let scenario = Scenario::new(&[
        step("pr view 42", &view(json!({}))),
        step("pr checks 42", &checks("IN_PROGRESS", "pending")).code(8),
    ]);
    let mut child = scenario
        .command(
            &[
                "--target",
                "owner/repo#42@abc123",
                "--interval-ms",
                "5000",
                "--max-interval-ms",
                "5000",
                "--deadline-seconds",
                "30",
            ],
            &[],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().unwrap().unwrap();
    assert!(first.contains("\"event\":\"initial\""), "{first}");
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let rest: Vec<String> = lines.map(Result::unwrap).collect();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(143));
    let finals: Vec<&String> = rest
        .iter()
        .filter(|l| l.contains("\"event\":\"final\""))
        .collect();
    assert_eq!(finals.len(), 1, "{rest:?}");
    assert!(
        finals[0].contains("\"reason\":\"cancelled\""),
        "{}",
        finals[0]
    );
}

#[test]
fn watches_two_targets_and_reports_each_result() {
    let scenario = Scenario::new(&[
        step(
            "pr view 1",
            &view(json!({"number": 1, "state": "MERGED", "mergedAt": "x"})),
        ),
        step(
            "pr view 2",
            &view(json!({"number": 2, "state": "CLOSED", "closed": true})),
        ),
    ]);
    let mut args = vec!["--target", "o/r#1@abc123", "--target", "o/r#2@abc123"];
    args.extend(FAST);
    let output = scenario.command(&args, &[]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let list = events(&output.stdout);
    let results = list.last().unwrap()["results"].as_array().unwrap();
    assert_eq!(results[0]["target"], "o/r#1");
    assert_eq!(results[0]["reason"], "merged");
    assert_eq!(results[1]["target"], "o/r#2");
    assert_eq!(results[1]["reason"], "closed");
    assert_eq!(list.last().unwrap()["reason"], "action_required");
}

#[test]
fn bad_arguments_exit_64_with_the_usage() {
    let scenario = Scenario::new(&[]);
    let output = scenario
        .command(&["--target", "nonsense"], &[])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(64));
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.starts_with("invalid target: nonsense\nUsage: agt watch --target TARGET"),
        "{error}"
    );
    assert!(output.stdout.is_empty());
    let none = scenario.command(&[], &[]).output().unwrap();
    assert_eq!(none.status.code(), Some(64));
}

#[test]
fn help_prints_the_usage() {
    let scenario = Scenario::new(&[]);
    let output = scenario.command(&["--help"], &[]).output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.starts_with("Usage: agt watch --target TARGET [--target ...]\n"),
        "{text}"
    );
    assert!(text.ends_with("are not supported.\n"), "{text}");
    assert!(scenario.calls().is_empty());
}

fn mr_path(suffix: &str) -> String {
    format!("{GITLAB_PROJECT}/{suffix}")
}

fn merge_request(fields: Value) -> String {
    let mut base = json!({
        "iid": 7,
        "web_url": "https://gitlab.example.test/group/sub/project/-/merge_requests/7",
        "state": "opened",
        "sha": "abc123",
        "detailed_merge_status": "ci_still_running",
        "head_pipeline": {"id": 99, "status": "running", "web_url": "https://gitlab.example.test/p/99"},
    });
    for (key, value) in fields.as_object().unwrap() {
        if value.is_null() && key == "sha" {
            base.as_object_mut().unwrap().remove(key);
        } else {
            base[key] = value.clone();
        }
    }
    base.to_string()
}

fn jobs(status: &str) -> String {
    json!([{
        "name": "test", "status": status, "stage": "test",
        "web_url": "https://gitlab.example.test/j/1", "allow_failure": false,
    }])
    .to_string()
}

fn gitlab(steps: &[Step]) -> Outcome {
    run_with(
        steps,
        GITLAB_TARGET,
        &[],
        &[("GITLAB_HOST", "gitlab.example.test")],
    )
}

#[test]
fn watches_a_gitlab_pipeline_with_the_default_glab_host() {
    let host = "--hostname gitlab.com --method GET";
    let mr = mr_path("merge_requests/7");
    let jobs_path = mr_path("pipelines/99/jobs?per_page=100");
    let passed = merge_request(json!({
        "detailed_merge_status": "mergeable",
        "head_pipeline": {"id": 99, "status": "success", "web_url": "https://gitlab.example.test/p/99"},
    }));
    let outcome = run_with(
        &[
            step("config get host", "gitlab.com\n"),
            step(&format!("api {host} {mr}"), &merge_request(json!({}))),
            step(
                &format!("api {host} --paginate {jobs_path}"),
                &jobs("running"),
            ),
            step(&format!("api {host} {mr}"), &passed),
            step(
                &format!("api {host} --paginate {jobs_path}"),
                &jobs("success"),
            ),
            step(&format!("api {host} {mr}"), &passed),
        ],
        GITLAB_TARGET,
        &[],
        &[],
    );
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(names(&outcome), ["initial", "change", "final"]);
    assert_eq!(outcome.events[0]["target"], "group/sub/project!7");
    assert_eq!(outcome.events[0]["reason"], "checks_pending");
    assert_eq!(outcome.events[1]["reason"], "required_checks_passed");
    assert_eq!(outcome.events[1]["mergeStatus"], "mergeable");
    assert_eq!(
        outcome.events[1]["checks"][0]["link"],
        "https://gitlab.example.test/j/1"
    );
    assert_eq!(outcome.events[2]["reason"], "checks_complete");
}

#[test]
fn uses_gitlab_host_without_asking_glab_for_its_host() {
    let outcome = gitlab(&[step(
        &format!(
            "api --hostname gitlab.example.test --method GET {}",
            mr_path("merge_requests/7")
        ),
        &merge_request(json!({"sha": "def456"})),
    )]);
    assert_eq!(outcome.status, 2, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "head_changed");
    assert_eq!(outcome.events[0]["observedHead"], "def456");
    assert_eq!(outcome.events.last().unwrap()["reason"], "action_required");
}

#[test]
fn a_missing_gitlab_host_blocks() {
    let outcome = run_with(&[step("config get host", "\n")], GITLAB_TARGET, &[], &[]);
    assert_eq!(outcome.status, 2, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "missing_gitlab_host");
}

#[test]
fn reports_gitlab_merge_request_states_without_querying_jobs() {
    let cases = [
        (json!({"state": "merged"}), 0, "merged"),
        (json!({"state": "closed"}), 2, "closed"),
        (json!({"state": "other"}), 2, "unknown_pull_request_state"),
        (json!({"head_pipeline": null}), 2, "no_required_checks"),
        (json!({"sha": null}), 2, "missing_pull_request_data"),
    ];
    for (fields, status, reason) in cases {
        let outcome = gitlab(&[step(&mr_path("merge_requests/7"), &merge_request(fields))]);
        assert_eq!(outcome.status, status, "{reason}: {}", outcome.stderr);
        assert_eq!(outcome.events[0]["reason"], reason);
    }
}

#[test]
fn reports_a_locked_gitlab_merge_request_as_pending() {
    let mr = mr_path("merge_requests/7");
    let outcome = gitlab(&[
        step(&mr, &merge_request(json!({"state": "locked"}))),
        step(&mr, &merge_request(json!({"state": "merged"}))),
    ]);
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(
        reasons(&outcome),
        ["merge_in_progress", "merged", "checks_complete"]
    );
}

#[test]
fn reports_gitlab_pipeline_blockers() {
    let cases = [
        ("failed", "check_failed"),
        ("canceled", "check_cancelled"),
        ("manual", "manual_action_required"),
        ("skipped", "unknown_check_state"),
    ];
    for (status, reason) in cases {
        let outcome = gitlab(&[
            step(
                &mr_path("merge_requests/7"),
                &merge_request(json!({"head_pipeline": {"id": 99, "status": status}})),
            ),
            step(&mr_path("pipelines/99/jobs?per_page=100"), &jobs(status)),
        ]);
        assert_eq!(outcome.status, 2, "{status}: {}", outcome.stderr);
        assert_eq!(outcome.events[0]["reason"], reason);
        assert_eq!(outcome.events[0]["pipeline"]["status"], status);
    }
    let outcome = gitlab(&[
        step(&mr_path("merge_requests/7"), &merge_request(json!({}))),
        step(
            &mr_path("pipelines/99/jobs?per_page=100"),
            &json!([{"name": "test"}]).to_string(),
        ),
    ]);
    assert_eq!(outcome.status, 2, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "missing_check_data");
}

#[test]
fn reads_fork_merge_request_jobs_from_the_pipeline_project() {
    let mr = mr_path("merge_requests/7");
    let passed = merge_request(json!({
        "head_pipeline": {"id": 99, "status": "success", "project_id": 555},
    }));
    let outcome = gitlab(&[
        step(&mr, &passed),
        step(
            "projects/555/pipelines/99/jobs?per_page=100",
            &jobs("success"),
        ),
        step(&mr, &passed),
    ]);
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "required_checks_passed");
}

#[test]
fn rechecks_the_gitlab_head_pipeline_before_reporting_success() {
    let mr = mr_path("merge_requests/7");
    let outcome = gitlab(&[
        step(
            &mr,
            &merge_request(json!({"head_pipeline": {"id": 99, "status": "success"}})),
        ),
        step(&mr_path("pipelines/99/jobs?per_page=100"), &jobs("success")),
        step(
            &mr,
            &merge_request(json!({"head_pipeline": {"id": 100, "status": "running"}})),
        ),
        step(&mr, &merge_request(json!({"sha": "def456"}))),
    ]);
    assert_eq!(outcome.status, 2, "{}", outcome.stderr);
    assert_eq!(
        reasons(&outcome),
        ["pipeline_changed", "head_changed", "action_required"]
    );
    assert_eq!(outcome.events[0]["observedPipeline"], 100);
    assert_eq!(outcome.events[0]["expectedPipeline"], 99);
}

#[test]
fn preserves_gitlab_permission_and_authentication_errors() {
    for (detail, reason) in [
        ("glab: 403 Forbidden", "permission_error"),
        ("glab: 401 Unauthorized", "authentication_error"),
    ] {
        let outcome = gitlab(&[failing(&mr_path("merge_requests/7"), detail, 1)]);
        assert_eq!(outcome.status, 2, "{}", outcome.stderr);
        assert_eq!(outcome.events[0]["reason"], reason);
        assert_eq!(outcome.events[0]["detail"], detail);
    }
}
