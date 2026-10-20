//! `agt ci wait`: a fake gh/glab replays numbered steps (`test/fixtures/fake-steps.sh`) and the
//! events on stdout and the exit status are checked.

use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::support::Env;

const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixtures/fake-steps.sh");
const GITHUB_TARGET: &str = "owner/repo#42@abc123";
const GITLAB_TARGET: &str = "group/sub/project!7@abc123";
const GITLAB_PROJECT: &str = "projects/group%2Fsub%2Fproject";
const GET: &str = "api -X GET";

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
    fn delay(mut self, seconds: f64) -> Self {
        self.delay = Some(seconds);
        self
    }
}

struct Scenario {
    env: Env,
    dir: std::path::PathBuf,
}

impl Scenario {
    fn new(steps: &[Step]) -> Self {
        let env = Env::new();
        let dir = env.path("steps");
        fs::create_dir_all(&dir).unwrap();
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
        Self { env, dir }
    }

    fn command(&self, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut command = self.env.bin(&self.env.repo);
        command
            .args(["ci", "wait"])
            .args(args)
            .env("AGT_GH_BIN", FAKE)
            .env("AGT_GLAB_BIN", FAKE)
            .env("FAKE_STEPS_DIR", &self.dir)
            .env("GITLAB_HOST", "");
        for (key, value) in env {
            command.env(key, value);
        }
        command
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
    "--interval",
    "0.001",
    "--max-interval",
    "0.002",
    "--timeout",
    "2",
];

fn run_args(steps: &[Step], targets: &[&str], extra: &[&str], env: &[(&str, &str)]) -> Outcome {
    let scenario = Scenario::new(steps);
    let mut args = targets.to_vec();
    if !extra.contains(&"--timeout") {
        args.extend(FAST);
    }
    args.extend(extra);
    let output = scenario.command(&args, env).output().unwrap();
    Outcome {
        status: output.status.code().unwrap(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        events: events(&output.stdout),
    }
}

fn run_with(steps: &[Step], target: &str, extra: &[&str], env: &[(&str, &str)]) -> Outcome {
    run_args(steps, &[target], extra, env)
}

fn run(steps: &[Step]) -> Outcome {
    run_with(steps, GITHUB_TARGET, &[], &[])
}

/// The reply to the one GraphQL read of a PR: its fields and its status check rollup.
fn pull(fields: Value, contexts: &[Value]) -> String {
    let mut pull = json!({
        "number": 42,
        "url": "https://github.com/owner/repo/pull/42",
        "state": "OPEN",
        "mergedAt": null,
        "closed": false,
        "headRefOid": "abc123",
        "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": contexts,
        }}}}]},
    });
    for (key, value) in fields.as_object().unwrap() {
        pull[key] = value.clone();
    }
    json!({"data": {"repository": {"pullRequest": pull}}}).to_string()
}

fn run_node(name: &str, status: &str, conclusion: Option<&str>) -> Value {
    json!({
        "__typename": "CheckRun", "name": name, "status": status, "conclusion": conclusion,
        "title": "", "detailsUrl": "https://github.com/owner/repo/actions/runs/1",
        "isRequired": true,
        "checkSuite": {"workflowRun": {"event": "pull_request", "workflow": {"name": "CI"}}},
    })
}

fn open(contexts: &[Value]) -> String {
    pull(json!({}), contexts)
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

fn last(outcome: &Outcome) -> &Value {
    outcome.events.last().unwrap()
}

#[test]
fn emits_initial_changed_and_final_events_once() {
    let outcome = run(&[
        step(
            "PullChecks",
            &open(&[run_node("test", "IN_PROGRESS", None)]),
        ),
        step(
            "PullChecks",
            &open(&[run_node("test", "COMPLETED", Some("SUCCESS"))]),
        ),
    ]);
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(names(&outcome), ["initial", "change", "final"]);
    assert_eq!(outcome.events[0]["target"], "owner/repo#42");
    assert_eq!(outcome.events[0]["reason"], "checks_pending");
    assert_eq!(outcome.events[1]["reason"], "required_checks_passed");
    assert_eq!(last(&outcome)["reason"], "checks_complete");
    assert_eq!(
        last(&outcome)["results"][0]["reason"],
        "required_checks_passed"
    );
    assert_eq!(last(&outcome)["results"][0]["failed"], json!([]));
}

#[test]
fn events_keep_their_field_order() {
    let scenario = Scenario::new(&[step(
        "PullChecks",
        &open(&[run_node("test", "COMPLETED", Some("SUCCESS"))]),
    )]);
    let output = scenario
        .command(&[GITHUB_TARGET, "--timeout", "2"], &[])
        .output()
        .unwrap();
    let lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(output.status.code(), Some(0));
    let keys = |line: &str| -> Vec<String> {
        serde_json::from_str::<serde_json::Map<String, Value>>(line)
            .unwrap()
            .keys()
            .cloned()
            .collect()
    };
    assert_eq!(
        keys(&lines[0]),
        ["at", "event", "target", "status", "reason", "checks"]
    );
    assert_eq!(keys(&lines[1]), ["at", "event", "reason", "results"]);
    let check: Vec<String> = serde_json::from_str::<Value>(&lines[0]).unwrap()["checks"][0]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        check,
        [
            "name",
            "state",
            "bucket",
            "description",
            "link",
            "workflow",
            "event"
        ]
    );
}

#[test]
fn keeps_only_required_checks_and_maps_their_buckets() {
    let mut optional = run_node("optional", "COMPLETED", Some("FAILURE"));
    optional["isRequired"] = json!(false);
    let status = json!({
        "__typename": "StatusContext", "context": "ci/legacy", "state": "SUCCESS",
        "description": "ok", "targetUrl": "https://ci.example/1", "isRequired": true,
    });
    let outcome = run(&[step(
        "PullChecks",
        &open(&[
            optional,
            status,
            run_node("skipped", "COMPLETED", Some("SKIPPED")),
            run_node("test", "COMPLETED", Some("SUCCESS")),
        ]),
    )]);
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    let checks = outcome.events[0]["checks"].as_array().unwrap();
    let found: Vec<(&str, &str)> = checks
        .iter()
        .map(|c| (c["name"].as_str().unwrap(), c["bucket"].as_str().unwrap()))
        .collect();
    assert_eq!(
        found,
        [
            ("ci/legacy", "pass"),
            ("skipped", "skipping"),
            ("test", "pass")
        ]
    );
}

#[test]
fn follows_the_check_rollup_pages() {
    let first = open(&[run_node("a", "COMPLETED", Some("SUCCESS"))]).replace(
        "\"hasNextPage\":false,\"endCursor\":null",
        "\"hasNextPage\":true,\"endCursor\":\"c1\"",
    );
    let outcome = run(&[
        step("PullChecks", &first),
        step(
            "cursor=c1",
            &open(&[run_node("b", "COMPLETED", Some("SUCCESS"))]),
        ),
    ]);
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["checks"].as_array().unwrap().len(), 2);
}

#[test]
fn reports_a_head_change() {
    let outcome = run(&[step(
        "PullChecks",
        &pull(json!({"headRefOid": "def456"}), &[]),
    )]);
    assert_eq!(outcome.status, 1, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "head_changed");
    assert_eq!(outcome.events[0]["expectedHead"], "abc123");
    assert_eq!(outcome.events[0]["observedHead"], "def456");
    assert_eq!(last(&outcome)["reason"], "action_required");
}

#[test]
fn pins_the_head_read_at_the_start_without_a_sha() {
    let outcome = run_with(
        &[
            step("PullChecks", &open(&[run_node("t", "IN_PROGRESS", None)])),
            step(
                "PullChecks",
                &pull(
                    json!({"headRefOid": "def456"}),
                    &[run_node("t", "IN_PROGRESS", None)],
                ),
            ),
        ],
        "owner/repo#42",
        &[],
        &[],
    );
    assert_eq!(outcome.status, 1, "{}", outcome.stderr);
    assert_eq!(
        reasons(&outcome),
        ["checks_pending", "head_changed", "action_required"]
    );
    assert_eq!(outcome.events[1]["expectedHead"], "abc123");
}

#[test]
fn sha_waits_for_the_head_to_reach_the_commit() {
    let outcome = run_with(
        &[
            step("PullChecks", &pull(json!({"headRefOid": "old000"}), &[])),
            step(
                "PullChecks",
                &open(&[run_node("t", "COMPLETED", Some("SUCCESS"))]),
            ),
        ],
        "owner/repo#42",
        &["--sha", "abc123"],
        &[],
    );
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(
        reasons(&outcome)[..2],
        ["waiting_for_pipeline", "required_checks_passed"]
    );
}

#[test]
fn sha_needs_one_pull_request_or_merge_request() {
    let both = run_args(&[], &["o/r#1", "o/r#2"], &["--sha", "abc123"], &[]);
    assert_eq!(both.status, 2);
    assert!(both.stderr.contains("--sha"), "{}", both.stderr);
    let pipeline = run_args(
        &[],
        &["https://gitlab.example.test/g/p/-/pipelines/9"],
        &["--sha", "abc123"],
        &[],
    );
    assert_eq!(pipeline.status, 2);
}

#[test]
fn reports_merged_and_closed_pull_requests() {
    let cases = [
        (
            json!({"state": "MERGED", "mergedAt": "2026-10-02T12:00:00Z", "closed": true}),
            0,
            "merged",
        ),
        (
            json!({"state": "CLOSED", "mergedAt": null, "closed": true}),
            1,
            "closed",
        ),
    ];
    for (fields, status, reason) in cases {
        let outcome = run(&[step("PullChecks", &pull(fields, &[]))]);
        assert_eq!(outcome.status, status, "{}", outcome.stderr);
        assert_eq!(outcome.events[0]["reason"], reason);
    }
}

#[test]
fn reports_no_checks_unknown_checks_failures_and_cancellation() {
    let mut optional = run_node("optional", "COMPLETED", Some("SUCCESS"));
    optional["isRequired"] = json!(false);
    let cases = [
        (vec![optional], "no_required_checks"),
        (
            vec![run_node("test", "COMPLETED", Some("MYSTERY"))],
            "unknown_check_state",
        ),
        (
            vec![run_node("test", "COMPLETED", Some("FAILURE"))],
            "check_failed",
        ),
        (
            vec![run_node("test", "COMPLETED", Some("CANCELLED"))],
            "check_cancelled",
        ),
    ];
    for (nodes, reason) in cases {
        let outcome = run(&[step("PullChecks", &open(&nodes))]);
        assert_eq!(outcome.status, 1, "{}", outcome.stderr);
        assert_eq!(outcome.events[0]["reason"], reason);
    }
}

#[test]
fn the_final_event_lists_the_failed_checks() {
    let outcome = run(&[step(
        "PullChecks",
        &open(&[
            run_node("lint", "COMPLETED", Some("SUCCESS")),
            run_node("test", "COMPLETED", Some("FAILURE")),
        ]),
    )]);
    let failed = &last(&outcome)["results"][0]["failed"];
    assert_eq!(failed.as_array().unwrap().len(), 1, "{failed}");
    assert_eq!(failed[0]["name"], "test");
    assert_eq!(
        failed[0]["link"],
        "https://github.com/owner/repo/actions/runs/1"
    );
}

#[test]
fn reports_missing_pull_request_data() {
    let outcome = run(&[step("PullChecks", &pull(json!({"headRefOid": null}), &[]))]);
    assert_eq!(outcome.status, 1, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "missing_pull_request_data");
}

#[test]
fn query_failures_name_their_cause_and_exit_2() {
    let cases = [
        (
            "authentication required: run gh auth login",
            "authentication_error",
        ),
        (
            "gh: Resource not accessible by integration (HTTP 403)",
            "permission_error",
        ),
        ("gh: Not Found (HTTP 404)", "query_error"),
    ];
    for (stderr, reason) in cases {
        let outcome = run(&[failing("PullChecks", stderr, 1)]);
        assert_eq!(outcome.status, 2, "{}", outcome.stderr);
        assert_eq!(outcome.events[0]["reason"], reason);
        assert!(
            outcome.events[0]["detail"]
                .as_str()
                .unwrap()
                .contains(stderr),
            "{}",
            outcome.events[0]
        );
        assert_eq!(last(&outcome)["reason"], "action_required");
    }
}

#[test]
fn a_missing_tool_is_a_query_error() {
    let outcome = run_with(
        &[],
        GITHUB_TARGET,
        &[],
        &[("AGT_GH_BIN", "/nonexistent/agt-test-gh")],
    );
    assert_eq!(outcome.status, 2, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "query_error");
}

#[test]
fn a_missing_pull_request_is_a_query_error() {
    let outcome = run(&[step(
        "PullChecks",
        r#"{"data":{"repository":{"pullRequest":null}}}"#,
    )]);
    assert_eq!(outcome.status, 2, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "query_error");
}

#[test]
fn stops_at_the_timeout_with_status_3() {
    let outcome = run_with(
        &[step(
            "PullChecks",
            &open(&[run_node("test", "IN_PROGRESS", None)]),
        )],
        GITHUB_TARGET,
        &["--interval", "2", "--max-interval", "2", "--timeout", "1"],
        &[],
    );
    assert_eq!(outcome.status, 3, "{}", outcome.stderr);
    assert_eq!(names(&outcome), ["initial", "final"]);
    assert_eq!(last(&outcome)["reason"], "deadline_exceeded");
}

#[test]
fn a_tool_still_running_at_the_timeout_ends_the_wait_on_time() {
    let started = Instant::now();
    let outcome = run_with(
        &[step("PullChecks", &open(&[])).delay(5.0)],
        GITHUB_TARGET,
        &["--timeout", "1"],
        &[],
    );
    assert_eq!(outcome.status, 3, "{}", outcome.stderr);
    assert_eq!(last(&outcome)["reason"], "deadline_exceeded");
    assert!(started.elapsed() < Duration::from_millis(3000));
}

#[test]
fn emits_one_final_event_when_terminated() {
    let scenario = Scenario::new(&[step(
        "PullChecks",
        &open(&[run_node("test", "IN_PROGRESS", None)]),
    )]);
    let mut child = scenario
        .command(
            &[
                GITHUB_TARGET,
                "--interval",
                "5",
                "--max-interval",
                "5",
                "--timeout",
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
fn waits_for_two_targets_and_reports_each_result() {
    let second = pull(
        json!({"number": 43, "url": "https://github.com/owner/repo/pull/43"}),
        &[run_node("test", "COMPLETED", Some("SUCCESS"))],
    );
    let outcome = run_args(
        &[
            step(
                "PullChecks",
                &open(&[run_node("test", "IN_PROGRESS", None)]),
            ),
            step("PullChecks", &second),
            step(
                "PullChecks",
                &open(&[run_node("test", "COMPLETED", Some("SUCCESS"))]),
            ),
        ],
        &[GITHUB_TARGET, "owner/repo#43@abc123"],
        &[],
        &[],
    );
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    let results = last(&outcome)["results"].as_array().unwrap();
    let targets: Vec<&str> = results
        .iter()
        .map(|r| r["target"].as_str().unwrap())
        .collect();
    assert_eq!(targets, ["owner/repo#42", "owner/repo#43"]);
    assert!(
        results
            .iter()
            .all(|r| r["reason"] == "required_checks_passed")
    );
}

#[test]
fn bad_arguments_exit_2() {
    let cases: [&[&str]; 6] = [
        &[],
        &["nope"],
        &["o/r#1", "o/r#1@abc123"],
        &["o/r#1", "--interval", "x"],
        &["o/r#1", "--interval", "0"],
        &["o/r#1", "--interval", "9", "--max-interval", "8"],
    ];
    for args in cases {
        let scenario = Scenario::new(&[]);
        let output = scenario.command(args, &[]).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn a_local_reference_for_the_other_forge_than_origin_is_a_usage_error() {
    for (origin, target, forge) in [
        ("https://github.com/o/r.git", "!5", "GitHub"),
        ("https://gitlab.com/g/p.git", "#5", "GitLab"),
    ] {
        let scenario = Scenario::new(&[]);
        scenario.env.git(&["remote", "add", "origin", origin]);
        let output = scenario.command(&[target], &[]).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{target}");
        assert!(output.stdout.is_empty(), "{target}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(&format!("is {forge}, but")), "{stderr}");
    }
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

fn jobs_path() -> String {
    mr_path("pipelines/99/jobs?per_page=100")
}

#[test]
fn waits_for_a_gitlab_pipeline_on_the_host_glab_names() {
    let mr = mr_path("merge_requests/7");
    let passed = merge_request(json!({
        "detailed_merge_status": "mergeable",
        "head_pipeline": {"id": 99, "status": "success", "web_url": "https://gitlab.example.test/p/99"},
    }));
    let outcome = run_with(
        &[
            step("config get host", "gitlab.com\n"),
            step(&format!("{GET} {mr}"), &merge_request(json!({}))),
            step(
                &format!("{GET} --paginate {}", jobs_path()),
                &jobs("running"),
            ),
            step(&format!("{GET} {mr}"), &passed),
            step(
                &format!("{GET} --paginate {}", jobs_path()),
                &jobs("success"),
            ),
            step(&format!("{GET} {mr}"), &passed),
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
    assert_eq!(last(&outcome)["reason"], "checks_complete");
}

#[test]
fn gitlab_host_is_used_without_asking_glab_for_its_host() {
    let outcome = gitlab(&[step(
        &format!("{GET} {}", mr_path("merge_requests/7")),
        &merge_request(json!({"state": "merged"})),
    )]);
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "merged");
}

#[test]
fn a_missing_gitlab_host_is_a_usage_error() {
    let outcome = run_with(
        &[failing("config get host", "no host", 1)],
        GITLAB_TARGET,
        &[],
        &[],
    );
    assert_eq!(outcome.status, 2);
    assert!(outcome.events.is_empty());
    assert!(outcome.stderr.contains("GitLab host"), "{}", outcome.stderr);
}

#[test]
fn reports_gitlab_merge_request_states_without_querying_jobs() {
    let cases = [
        (json!({"state": "merged"}), 0, "merged"),
        (json!({"state": "closed"}), 1, "closed"),
        (json!({"state": "mystery"}), 1, "unknown_pull_request_state"),
        (json!({"head_pipeline": null}), 1, "no_required_checks"),
        (json!({"sha": "def456"}), 1, "head_changed"),
        (json!({"sha": null}), 1, "missing_pull_request_data"),
    ];
    for (fields, status, reason) in cases {
        let outcome = gitlab(&[step(
            &format!("{GET} {}", mr_path("merge_requests/7")),
            &merge_request(fields),
        )]);
        assert_eq!(outcome.status, status, "{reason}: {}", outcome.stderr);
        assert_eq!(outcome.events[0]["reason"], reason);
    }
}

#[test]
fn a_locked_gitlab_merge_request_is_pending() {
    let mr = mr_path("merge_requests/7");
    let outcome = gitlab(&[
        step(
            &format!("{GET} {mr}"),
            &merge_request(json!({"state": "locked"})),
        ),
        step(
            &format!("{GET} {mr}"),
            &merge_request(json!({"state": "merged"})),
        ),
    ]);
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(
        reasons(&outcome),
        ["merge_in_progress", "merged", "checks_complete"]
    );
}

#[test]
fn reports_gitlab_pipeline_blockers_and_failed_jobs() {
    let cases = [
        ("failed", "check_failed"),
        ("canceled", "check_cancelled"),
        ("manual", "manual_action_required"),
        ("skipped", "unknown_check_state"),
    ];
    for (status, reason) in cases {
        let mr = merge_request(json!({
            "head_pipeline": {"id": 99, "status": status, "web_url": "https://gitlab.example.test/p/99"},
        }));
        let outcome = gitlab(&[
            step(&format!("{GET} {}", mr_path("merge_requests/7")), &mr),
            step(
                &format!("{GET} --paginate {}", jobs_path()),
                &jobs(if status == "failed" {
                    "failed"
                } else {
                    "success"
                }),
            ),
        ]);
        assert_eq!(outcome.status, 1, "{reason}: {}", outcome.stderr);
        assert_eq!(outcome.events[0]["reason"], reason);
        let failed = last(&outcome)["results"][0]["failed"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(failed, usize::from(status == "failed"), "{reason}");
    }
}

#[test]
fn failed_jobs_allowed_to_fail_are_not_listed() {
    let mr = merge_request(json!({
        "head_pipeline": {"id": 99, "status": "failed", "web_url": "https://gitlab.example.test/p/99"},
    }));
    let jobs = json!([
        {"name": "lint", "status": "failed", "stage": "test", "web_url": "u", "allow_failure": true},
        {"name": "test", "status": "failed", "stage": "test", "web_url": "u2", "allow_failure": false},
    ])
    .to_string();
    let outcome = gitlab(&[
        step(&format!("{GET} {}", mr_path("merge_requests/7")), &mr),
        step(&format!("{GET} --paginate {}", jobs_path()), &jobs),
    ]);
    let failed = &last(&outcome)["results"][0]["failed"];
    assert_eq!(failed.as_array().unwrap().len(), 1, "{failed}");
    assert_eq!(failed[0]["name"], "test");
    assert_eq!(failed[0]["stage"], "test");
}

#[test]
fn reads_fork_merge_request_jobs_from_the_pipeline_project() {
    let mr = merge_request(json!({
        "head_pipeline": {"id": 99, "project_id": 555, "status": "running", "web_url": "u"},
    }));
    let outcome = gitlab(&[
        step(&format!("{GET} {}", mr_path("merge_requests/7")), &mr),
        step(
            &format!("{GET} --paginate projects/555/pipelines/99/jobs?per_page=100"),
            &jobs("running"),
        ),
    ]);
    assert_eq!(outcome.events[0]["reason"], "checks_pending");
}

#[test]
fn rechecks_the_gitlab_head_pipeline_before_reporting_success() {
    let mr = mr_path("merge_requests/7");
    let passed = |id: u64| {
        merge_request(json!({
            "head_pipeline": {"id": id, "status": "success", "web_url": "u"},
        }))
    };
    let outcome = gitlab(&[
        step(&format!("{GET} {mr}"), &passed(99)),
        step(
            &format!("{GET} --paginate {}", jobs_path()),
            &jobs("success"),
        ),
        step(&format!("{GET} {mr}"), &passed(100)),
        step(
            &format!("{GET} {mr}"),
            &merge_request(json!({"state": "merged"})),
        ),
    ]);
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "pipeline_changed");
    assert_eq!(outcome.events[0]["expectedPipeline"], 99);
    assert_eq!(outcome.events[0]["observedPipeline"], 100);
}

#[test]
fn sha_waits_for_a_gitlab_pipeline_on_the_commit() {
    let mr = mr_path("merge_requests/7");
    let old = merge_request(json!({
        "head_pipeline": {"id": 98, "status": "success", "sha": "old000", "web_url": "u"},
    }));
    let current = |status: &str| {
        merge_request(json!({
            "head_pipeline": {"id": 99, "status": status, "sha": "abc123", "web_url": "u"},
        }))
    };
    let outcome = run_with(
        &[
            step(&format!("{GET} {mr}"), &old),
            step(
                &format!("{GET} {}", mr_path("repository/commits/old000")),
                r#"{"parent_ids": ["x"]}"#,
            ),
            step(&format!("{GET} {mr}"), &current("success")),
            step(
                &format!("{GET} --paginate {}", jobs_path()),
                &jobs("success"),
            ),
            step(&format!("{GET} {mr}"), &current("success")),
        ],
        "group/sub/project!7",
        &["--sha", "abc123"],
        &[("GITLAB_HOST", "gitlab.example.test")],
    );
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(
        reasons(&outcome)[..2],
        ["waiting_for_pipeline", "required_checks_passed"]
    );
}

#[test]
fn a_merged_results_pipeline_runs_the_commit_its_parent_names() {
    let mr = mr_path("merge_requests/7");
    let merged_results = merge_request(json!({
        "head_pipeline": {"id": 99, "status": "success", "sha": "merge1", "web_url": "u"},
    }));
    let outcome = run_with(
        &[
            step(&format!("{GET} {mr}"), &merged_results),
            step(
                &format!("{GET} {}", mr_path("repository/commits/merge1")),
                r#"{"parent_ids": ["abc123", "main1"]}"#,
            ),
            step(
                &format!("{GET} --paginate {}", jobs_path()),
                &jobs("success"),
            ),
            step(&format!("{GET} {mr}"), &merged_results),
        ],
        "group/sub/project!7",
        &["--sha", "abc123"],
        &[("GITLAB_HOST", "gitlab.example.test")],
    );
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "required_checks_passed");
}

#[test]
fn waits_for_a_gitlab_pipeline_by_url() {
    let pipeline =
        json!({"id": 99, "status": "success", "web_url": "https://gitlab.example.test/p/99"});
    let outcome = run_with(
        &[
            step(
                &format!("{GET} {}", mr_path("pipelines/99")),
                &pipeline.to_string(),
            ),
            step(
                &format!("{GET} --paginate {}", jobs_path()),
                &jobs("success"),
            ),
        ],
        "https://gitlab.example.test/group/sub/project/-/pipelines/99",
        &[],
        &[],
    );
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(
        outcome.events[0]["target"],
        "group/sub/project/-/pipelines/99"
    );
    assert_eq!(outcome.events[0]["pipeline"]["status"], "success");
}

#[test]
fn gitlab_permission_and_authentication_errors_are_named() {
    let cases = [
        ("glab: 403 Forbidden (HTTP 403)", "permission_error"),
        ("glab: 401 Unauthorized (HTTP 401)", "authentication_error"),
    ];
    for (stderr, reason) in cases {
        let outcome = gitlab(&[failing(&mr_path("merge_requests/7"), stderr, 1)]);
        assert_eq!(outcome.status, 2, "{}", outcome.stderr);
        assert_eq!(outcome.events[0]["reason"], reason);
    }
}

#[test]
fn pull_request_and_merge_request_arguments_may_be_urls_or_local() {
    let url = run_with(
        &[step(
            "PullChecks",
            &pull(json!({"state": "MERGED", "mergedAt": "x"}), &[]),
        )],
        "https://github.com/owner/repo/pull/42@abc123",
        &[],
        &[],
    );
    assert_eq!(url.status, 0, "{}", url.stderr);
    assert_eq!(url.events[0]["target"], "owner/repo#42");
}

fn bad_gateway() -> Step {
    failing("PullChecks", "gh: Bad Gateway (HTTP 502)", 1)
}

#[test]
fn four_transient_failures_then_success_end_normally() {
    let mut steps: Vec<Step> = (0..4).map(|_| bad_gateway()).collect();
    steps.push(step(
        "PullChecks",
        &open(&[run_node("test", "COMPLETED", Some("SUCCESS"))]),
    ));
    let outcome = run(&steps);
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(names(&outcome), ["initial", "final"]);
}

#[test]
fn the_count_resets_after_a_successful_poll() {
    let pending = open(&[run_node("test", "IN_PROGRESS", None)]);
    let mut steps: Vec<Step> = (0..4).map(|_| bad_gateway()).collect();
    steps.push(step("PullChecks", &pending));
    steps.extend((0..4).map(|_| bad_gateway()));
    steps.push(step(
        "PullChecks",
        &open(&[run_node("test", "COMPLETED", Some("SUCCESS"))]),
    ));
    let outcome = run(&steps);
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
}

#[test]
fn five_transient_failures_in_a_row_are_a_query_error() {
    let steps: Vec<Step> = (0..5).map(|_| bad_gateway()).collect();
    let outcome = run(&steps);
    assert_eq!(outcome.status, 2, "{}", outcome.stderr);
    assert_eq!(outcome.events[0]["reason"], "query_error");
    assert_eq!(names(&outcome), ["initial", "final"]);
}

#[test]
fn a_rate_limit_is_transient() {
    let limited = failing("PullChecks", "gh: API rate limit exceeded (HTTP 403)", 1);
    let outcome = run(&[
        limited,
        step(
            "PullChecks",
            &open(&[run_node("test", "COMPLETED", Some("SUCCESS"))]),
        ),
    ]);
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
}

#[test]
fn an_empty_rollup_is_pending_right_after_the_wait_starts() {
    let outcome = run(&[
        step("PullChecks", &open(&[])),
        step(
            "PullChecks",
            &open(&[run_node("test", "COMPLETED", Some("SUCCESS"))]),
        ),
    ]);
    assert_eq!(outcome.status, 0, "{}", outcome.stderr);
    assert_eq!(
        reasons(&outcome),
        [
            "checks_pending",
            "required_checks_passed",
            "checks_complete"
        ]
    );
}
