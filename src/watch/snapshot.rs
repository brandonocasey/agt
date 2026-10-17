//! One poll of a target: the read-only `gh` or `glab api` queries, in agent-watch's order, and
//! the state they add up to.

use std::time::Duration;

use serde_json::Value;

use super::args::Target;
use super::classify::{State, blocked, classify, classify_gitlab};
use super::json::{Fields, J, get, is_str, parse_ordered};

const VIEW_FIELDS: &str = "number,url,state,mergedAt,closed,headRefOid";
const CHECK_FIELDS: &str = "name,state,bucket,description,link,workflow,event";

/// What a tool run left behind, or that the deadline passed before it could start.
pub struct CmdResult {
    pub deadline_exceeded: bool,
    /// The exit status, `1` when a signal ended the tool, or an errno name when it did not start.
    pub code: Value,
    pub signal: Option<&'static str>,
    pub stdout: String,
    pub stderr: String,
    pub invocation: Vec<String>,
    pub timed_out: bool,
}

impl CmdResult {
    pub fn deadline() -> Self {
        Self {
            deadline_exceeded: true,
            code: Value::Null,
            signal: None,
            stdout: String::new(),
            stderr: String::new(),
            invocation: Vec::new(),
            timed_out: false,
        }
    }

    fn exit_code(&self) -> Option<i64> {
        self.code.as_i64()
    }
}

/// Runs the platform's tool with these arguments.
pub trait Runner {
    fn run(&mut self, args: &[String]) -> CmdResult;
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn command_failure(result: &CmdResult) -> State {
    let detail = [result.stderr.as_str(), result.stdout.as_str()]
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();
    let normalized = detail.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| normalized.contains(word));
    let reason = if result.timed_out {
        "query_timeout"
    } else if has(&["auth", "login", "token", "credential"]) {
        "authentication_error"
    } else if has(&[
        "forbidden",
        "permission",
        "resource not accessible",
        "denied",
    ]) {
        "permission_error"
    } else {
        "query_error"
    };
    let detail = if detail.is_empty() {
        format!(
            "{} exited with status {}",
            result.invocation.first().map_or("", String::as_str),
            display_code(&result.code)
        )
    } else {
        detail
    };
    vec![
        ("status", J::str("blocked")),
        ("reason", J::str(reason)),
        ("detail", J::str(&detail)),
        ("command", J::Raw(Value::from(result.invocation.clone()))),
        ("exitCode", J::Raw(result.code.clone())),
        ("signal", result.signal.map_or_else(J::null, J::str)),
    ]
}

fn display_code(code: &Value) -> String {
    code.as_str()
        .map_or_else(|| code.to_string(), str::to_owned)
}

enum Expect {
    Array,
    Object,
}

/// The parsed response and its text.
type Doc = (Value, String);

fn parse_json(result: &CmdResult, expected: &Expect, allowed: &[i64]) -> Result<Doc, State> {
    if result.deadline_exceeded {
        return Err(blocked("deadline_exceeded"));
    }
    if !result
        .exit_code()
        .is_some_and(|code| allowed.contains(&code))
    {
        return Err(command_failure(result));
    }
    let parsed = serde_json::from_str::<Value>(&result.stdout)
        .map_err(|error| error.to_string())
        .and_then(|value| match (expected, &value) {
            (Expect::Array, Value::Array(_)) | (Expect::Object, Value::Object(_)) => {
                Ok((value, result.stdout.clone()))
            }
            (Expect::Array, _) => Err("expected an array".to_owned()),
            (Expect::Object, _) => Err("expected an object".to_owned()),
        });
    parsed.map_err(|message| {
        if result.exit_code() != Some(0) {
            return command_failure(result);
        }
        let detail = format!(
            "{message}; stdout: {}; stderr: {}",
            result.stdout, result.stderr
        );
        vec![
            ("status", J::str("blocked")),
            ("reason", J::str("invalid_response")),
            ("detail", J::str(&detail)),
            ("command", J::Raw(Value::from(result.invocation.clone()))),
            ("exitCode", J::Raw(result.code.clone())),
        ]
    })
}

/// `missing_pull_request_data` echoes the response; print it in the order the host sent it.
fn echo_in_order(mut state: State, text: &str) -> State {
    if let Some(slot) = state.iter_mut().find(|(key, _)| *key == "view")
        && let Some(ordered) = parse_ordered(text)
    {
        slot.1 = ordered;
    }
    state
}

fn items(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

pub fn snapshot_github(target: &Target, gh: &mut dyn Runner) -> State {
    let number = target.number.to_string();
    let view_args = strings(&[
        "pr",
        "view",
        &number,
        "--repo",
        &target.repo,
        "--json",
        VIEW_FIELDS,
    ]);
    let (view, view_text) = match parse_json(&gh.run(&view_args), &Expect::Object, &[0]) {
        Ok(doc) => doc,
        Err(state) => return state,
    };
    if !is_str(get(&view, "headRefOid"), &target.head)
        || !is_str(get(&view, "state"), "OPEN")
        || super::json::truthy(get(&view, "closed"))
        || super::json::truthy(get(&view, "mergedAt"))
    {
        return echo_in_order(classify(target, &view, None), &view_text);
    }
    let checks_args = strings(&[
        "pr",
        "checks",
        &number,
        "--repo",
        &target.repo,
        "--required",
        "--json",
        CHECK_FIELDS,
    ]);
    // `gh pr checks` returns 1 for failed checks and 8 for pending checks while still writing
    // valid JSON. Other gh commands keep their strict exit status.
    let (checks, _) = match parse_json(&gh.run(&checks_args), &Expect::Array, &[0, 1, 8]) {
        Ok(doc) => doc,
        Err(state) => return state,
    };
    let state = classify(target, &view, Some(items(&checks)));
    if super::classify::status(&state) != "complete" {
        return state;
    }
    let (final_view, final_text) = match parse_json(&gh.run(&view_args), &Expect::Object, &[0]) {
        Ok(doc) => doc,
        Err(state) => return state,
    };
    echo_in_order(
        classify(target, &final_view, Some(items(&checks))),
        &final_text,
    )
}

/// The GitLab host: `GITLAB_HOST`, else what `glab config get host` prints.
fn resolve_host(glab: &mut dyn Runner) -> Result<String, State> {
    if let Some(host) = std::env::var("GITLAB_HOST")
        .ok()
        .filter(|host| !host.is_empty())
    {
        return Ok(host);
    }
    let result = glab.run(&strings(&["config", "get", "host"]));
    if result.deadline_exceeded {
        return Err(blocked("deadline_exceeded"));
    }
    if result.exit_code() != Some(0) {
        return Err(command_failure(&result));
    }
    let host = result.stdout.trim().to_owned();
    if host.is_empty() {
        let mut state = blocked("missing_gitlab_host");
        state.push(("command", J::Raw(Value::from(result.invocation))));
        return Err(state);
    }
    Ok(host)
}

/// `encodeURIComponent`.
fn encode_component(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn snapshot_gitlab(target: &Target, glab: &mut dyn Runner, host: &mut Option<String>) -> State {
    if host.is_none() {
        match resolve_host(glab) {
            Ok(resolved) => *host = Some(resolved),
            Err(state) => return state,
        }
    }
    let host = host.clone().unwrap_or_default();
    let project = format!("projects/{}", encode_component(&target.repo));
    let mut api = |endpoint: &str, paginate: bool| {
        let mut args = strings(&["api", "--hostname", &host, "--method", "GET"]);
        if paginate {
            args.push("--paginate".into());
        }
        args.push(endpoint.to_owned());
        glab.run(&args)
    };
    let mr_endpoint = format!("{project}/merge_requests/{}", target.number);
    let (mr, mr_text) = match parse_json(&api(&mr_endpoint, false), &Expect::Object, &[0]) {
        Ok(doc) => doc,
        Err(state) => return state,
    };
    let pipeline_id = get(get(&mr, "head_pipeline"), "id").clone();
    if !is_str(get(&mr, "sha"), &target.head)
        || !is_str(get(&mr, "state"), "opened")
        || !super::json::truthy(&pipeline_id)
    {
        return echo_in_order(classify_gitlab(target, &mr, None), &mr_text);
    }
    // A fork MR pipeline runs in the source project, so its jobs are not under the target project.
    let pipeline_project = match get(get(&mr, "head_pipeline"), "project_id").as_i64() {
        Some(id) => format!("projects/{id}"),
        None => project,
    };
    let jobs_endpoint = format!(
        "{pipeline_project}/pipelines/{}/jobs?per_page=100",
        display_code(&pipeline_id)
    );
    let (jobs, _) = match parse_json(&api(&jobs_endpoint, true), &Expect::Array, &[0]) {
        Ok(doc) => doc,
        Err(state) => return state,
    };
    let state = classify_gitlab(target, &mr, Some(items(&jobs)));
    if super::classify::status(&state) != "complete" {
        return state;
    }
    let (final_mr, final_text) = match parse_json(&api(&mr_endpoint, false), &Expect::Object, &[0])
    {
        Ok(doc) => doc,
        Err(state) => return state,
    };
    let final_id = get(get(&final_mr, "head_pipeline"), "id");
    if is_str(get(&final_mr, "state"), "opened")
        && is_str(get(&final_mr, "sha"), &target.head)
        && final_id != &pipeline_id
    {
        let fields: Fields = vec![
            ("status", J::str("pending")),
            ("reason", J::str("pipeline_changed")),
            ("expectedPipeline", J::Raw(pipeline_id)),
            ("observedPipeline", J::Raw(final_id.clone())),
            (
                "mergeStatus",
                J::Raw(get(&final_mr, "detailed_merge_status").clone()),
            ),
        ];
        return fields;
    }
    echo_in_order(
        classify_gitlab(target, &final_mr, Some(items(&jobs))),
        &final_text,
    )
}

/// The most a single tool run may take.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
