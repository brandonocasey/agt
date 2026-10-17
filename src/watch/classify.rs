//! Turns one poll's data into a state: `status` (`complete`, `pending` or `blocked`), a
//! `reason` and the evidence, in agent-watch's field order.

use serde_json::Value;

use super::args::Target;
use super::json::{Fields, J, get, is_str, locale_compare, truthy};

pub type State = Fields;

pub fn status(state: &State) -> &str {
    match state.iter().find(|(key, _)| *key == "status") {
        Some((_, J::Raw(Value::String(text)))) => text,
        _ => "",
    }
}

pub fn reason(state: &State) -> &str {
    match state.iter().find(|(key, _)| *key == "reason") {
        Some((_, J::Raw(Value::String(text)))) => text,
        _ => "",
    }
}

pub fn blocked(reason: &str) -> State {
    vec![("status", J::str("blocked")), ("reason", J::str(reason))]
}

fn with(status: &str, reason: &str, more: Fields) -> State {
    let mut state = vec![("status", J::str(status)), ("reason", J::str(reason))];
    state.extend(more);
    state
}

fn raw(value: &Value) -> J {
    J::Raw(value.clone())
}

fn sorted(mut items: Vec<J>) -> Vec<J> {
    // The collation order of the serialized form, as agent-watch sorts.
    let mut keyed: Vec<(String, J)> = items.drain(..).map(|item| (item.to_json(), item)).collect();
    keyed.sort_by(|a, b| locale_compare(&a.0, &b.0));
    keyed.into_iter().map(|(_, item)| item).collect()
}

fn normalize_checks(checks: &[Value]) -> Vec<J> {
    sorted(
        checks
            .iter()
            .map(|check| {
                J::Obj(vec![
                    ("name", raw(get(check, "name"))),
                    ("state", raw(get(check, "state"))),
                    ("bucket", raw(get(check, "bucket"))),
                    ("description", raw(get(check, "description"))),
                    ("link", raw(get(check, "link"))),
                    ("workflow", raw(get(check, "workflow"))),
                    ("event", raw(get(check, "event"))),
                ])
            })
            .collect(),
    )
}

fn field<'a>(item: &'a J, key: &str) -> &'a Value {
    static NULL: Value = Value::Null;
    match item {
        J::Obj(fields) => match fields.iter().find(|(name, _)| *name == key) {
            Some((_, J::Raw(value))) => value,
            _ => &NULL,
        },
        _ => &NULL,
    }
}

pub fn classify(target: &Target, view: &Value, checks: Option<&[Value]>) -> State {
    let url = || raw(get(view, "url"));
    let state_text = get(view, "state");
    if !get(view, "headRefOid").is_string() || !state_text.is_string() {
        return with(
            "blocked",
            "missing_pull_request_data",
            vec![("view", raw(view))],
        );
    }
    if !is_str(get(view, "headRefOid"), &target.head) {
        return with(
            "blocked",
            "head_changed",
            vec![
                ("expectedHead", J::str(&target.head)),
                ("observedHead", raw(get(view, "headRefOid"))),
                ("url", url()),
            ],
        );
    }
    if is_str(state_text, "MERGED") || truthy(get(view, "mergedAt")) {
        return with("complete", "merged", vec![("url", url())]);
    }
    if is_str(state_text, "CLOSED") || truthy(get(view, "closed")) {
        return with("blocked", "closed", vec![("url", url())]);
    }
    if !is_str(state_text, "OPEN") {
        return with(
            "blocked",
            "unknown_pull_request_state",
            vec![("state", raw(state_text))],
        );
    }
    let Some(checks) = checks else {
        return blocked("missing_check_data");
    };
    let normalized = normalize_checks(checks);
    if normalized.is_empty() {
        return with(
            "blocked",
            "no_required_checks",
            vec![("checks", J::Arr(Vec::new()))],
        );
    }
    let list = || J::Arr(normalized.clone());
    let incomplete = normalized.iter().any(|check| {
        !truthy(field(check, "name"))
            || !truthy(field(check, "state"))
            || !truthy(field(check, "bucket"))
    });
    if incomplete {
        return with("blocked", "missing_check_data", vec![("checks", list())]);
    }
    let bucket = |check: &J| field(check, "bucket").as_str().map(str::to_owned);
    let buckets: Vec<Option<String>> = normalized.iter().map(bucket).collect();
    let has = |wanted: &str| buckets.iter().any(|b| b.as_deref() == Some(wanted));
    if has("fail") || has("cancel") {
        let reason = if has("cancel") {
            "check_cancelled"
        } else {
            "check_failed"
        };
        return with("blocked", reason, vec![("checks", list())]);
    }
    let known = ["pass", "pending", "skipping"];
    if buckets
        .iter()
        .any(|b| !b.as_deref().is_some_and(|b| known.contains(&b)))
    {
        return with("blocked", "unknown_check_state", vec![("checks", list())]);
    }
    if has("pending") {
        return with("pending", "checks_pending", vec![("checks", list())]);
    }
    with(
        "complete",
        "required_checks_passed",
        vec![("checks", list())],
    )
}

fn normalize_jobs(jobs: &[Value]) -> Vec<J> {
    sorted(
        jobs.iter()
            .map(|job| {
                J::Obj(vec![
                    ("name", raw(get(job, "name"))),
                    ("state", raw(get(job, "status"))),
                    ("stage", raw(get(job, "stage"))),
                    ("link", raw(get(job, "web_url"))),
                    ("allowFailure", raw(get(job, "allow_failure"))),
                ])
            })
            .collect(),
    )
}

fn pipeline_reason(status: &str) -> (&'static str, &'static str) {
    match status {
        "success" => ("complete", "required_checks_passed"),
        "failed" => ("blocked", "check_failed"),
        "canceled" | "canceling" => ("blocked", "check_cancelled"),
        "manual" => ("blocked", "manual_action_required"),
        "created"
        | "waiting_for_resource"
        | "preparing"
        | "pending"
        | "running"
        | "scheduled"
        | "waiting_for_callback" => ("pending", "checks_pending"),
        _ => ("blocked", "unknown_check_state"),
    }
}

pub fn classify_gitlab(target: &Target, mr: &Value, jobs: Option<&[Value]>) -> State {
    let state_text = get(mr, "state");
    if !get(mr, "sha").is_string() || !state_text.is_string() {
        return with(
            "blocked",
            "missing_pull_request_data",
            vec![("view", raw(mr))],
        );
    }
    let url = || raw(get(mr, "web_url"));
    if !is_str(get(mr, "sha"), &target.head) {
        return with(
            "blocked",
            "head_changed",
            vec![
                ("expectedHead", J::str(&target.head)),
                ("observedHead", raw(get(mr, "sha"))),
                ("url", url()),
            ],
        );
    }
    if is_str(state_text, "merged") {
        return with("complete", "merged", vec![("url", url())]);
    }
    if is_str(state_text, "closed") {
        return with("blocked", "closed", vec![("url", url())]);
    }
    let merge_status = || raw(get(mr, "detailed_merge_status"));
    // GitLab reports `locked` while it merges; the next poll shows `merged`.
    if is_str(state_text, "locked") {
        return with(
            "pending",
            "merge_in_progress",
            vec![("url", url()), ("mergeStatus", merge_status())],
        );
    }
    if !is_str(state_text, "opened") {
        return with(
            "blocked",
            "unknown_pull_request_state",
            vec![("state", raw(state_text))],
        );
    }
    let head = get(mr, "head_pipeline");
    if head.is_null() {
        return with(
            "blocked",
            "no_required_checks",
            vec![
                ("checks", J::Arr(Vec::new())),
                ("mergeStatus", merge_status()),
            ],
        );
    }
    let pipeline = || {
        J::Obj(vec![
            ("id", raw(get(head, "id"))),
            ("status", raw(get(head, "status"))),
            ("link", raw(get(head, "web_url"))),
        ])
    };
    let Some(jobs) = jobs.filter(|_| truthy(get(head, "id")) && truthy(get(head, "status"))) else {
        return with(
            "blocked",
            "missing_check_data",
            vec![("pipeline", pipeline()), ("mergeStatus", merge_status())],
        );
    };
    let checks = normalize_jobs(jobs);
    if checks
        .iter()
        .any(|check| !truthy(field(check, "name")) || !truthy(field(check, "state")))
    {
        return with(
            "blocked",
            "missing_check_data",
            vec![
                ("pipeline", pipeline()),
                ("mergeStatus", merge_status()),
                ("checks", J::Arr(checks)),
            ],
        );
    }
    let (status, reason) = pipeline_reason(get(head, "status").as_str().unwrap_or(""));
    with(
        status,
        reason,
        vec![
            ("pipeline", pipeline()),
            ("mergeStatus", merge_status()),
            ("checks", J::Arr(checks)),
        ],
    )
}
