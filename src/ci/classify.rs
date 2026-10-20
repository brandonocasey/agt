//! Turns one poll's data into a state: `status` (`complete`, `pending` or `blocked`), a
//! `reason` and the evidence.

use serde_json::{Map, Value, json};

use crate::forge::{github_state, gitlab_state};
use crate::store::model::ReviewState;

/// A JSON object that starts with `status` and `reason`.
pub type State = Value;

pub fn status(state: &State) -> &str {
    state["status"].as_str().unwrap_or("")
}

pub fn reason(state: &State) -> &str {
    state["reason"].as_str().unwrap_or("")
}

/// The reasons that mean the target could not be read, not that it needs action.
pub fn is_query_error(state: &State) -> bool {
    status(state) == "blocked"
        && matches!(
            reason(state),
            "query_error" | "authentication_error" | "permission_error"
        )
}

/// A pipeline or head that has not caught up with the commit being awaited.
pub fn is_waiting(state: &State) -> bool {
    reason(state) == "waiting_for_pipeline"
}

fn with(status: &str, reason: &str, more: Value) -> State {
    let mut state = Map::new();
    state.insert("status".to_owned(), status.into());
    state.insert("reason".to_owned(), reason.into());
    if let Value::Object(more) = more {
        state.extend(more);
    }
    Value::Object(state)
}

pub fn blocked(reason: &str) -> State {
    with("blocked", reason, json!({}))
}

/// Whether a field is set: not null, false or an empty string.
pub fn present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

fn sorted(mut items: Vec<Value>) -> Vec<Value> {
    items.sort_by_cached_key(Value::to_string);
    items
}

fn normalize_checks(checks: &[Value]) -> Vec<Value> {
    sorted(
        checks
            .iter()
            .map(|check| {
                json!({
                    "name": check["name"],
                    "state": check["state"],
                    "bucket": check["bucket"],
                    "description": check["description"],
                    "link": check["link"],
                    "workflow": check["workflow"],
                    "event": check["event"],
                })
            })
            .collect(),
    )
}

/// The head a PR or MR must have, and whether it may still be catching up to it.
pub struct Head<'a> {
    pub expected: &'a str,
    pub awaiting: bool,
    /// Seconds since the wait began.
    pub waited_secs: i64,
}

/// Seconds after a wait begins during which a head with no checks at all is still pending:
/// right after a push the rollup can be empty before any check run exists.
const ROLLUP_GRACE_SECS: i64 = 120;

fn head_mismatch(head: &Head, observed: &Value, url: &Value) -> State {
    if head.awaiting {
        return with(
            "pending",
            "waiting_for_pipeline",
            json!({"expectedHead": head.expected, "observedHead": observed}),
        );
    }
    with(
        "blocked",
        "head_changed",
        json!({"expectedHead": head.expected, "observedHead": observed, "url": url}),
    )
}

/// The state of a PR from its `view`, the `rollup` size (every check, required or not) and its
/// required `checks`.
pub fn classify(head: &Head, view: &Value, rollup: usize, checks: &[Value]) -> State {
    let url = &view["url"];
    let state_text = &view["state"];
    if !view["headRefOid"].is_string() || !state_text.is_string() {
        return with(
            "blocked",
            "missing_pull_request_data",
            json!({"view": view}),
        );
    }
    if view["headRefOid"] != head.expected {
        return head_mismatch(head, &view["headRefOid"], url);
    }
    let state = state_text.as_str().and_then(github_state);
    if state == Some(ReviewState::Merged) || present(&view["mergedAt"]) {
        return with("complete", "merged", json!({"url": url}));
    }
    if state == Some(ReviewState::Closed) || present(&view["closed"]) {
        return with("blocked", "closed", json!({"url": url}));
    }
    if state != Some(ReviewState::Open) {
        return with(
            "blocked",
            "unknown_pull_request_state",
            json!({"state": state_text}),
        );
    }
    let normalized = normalize_checks(checks);
    if normalized.is_empty() {
        if rollup == 0 && head.waited_secs < ROLLUP_GRACE_SECS {
            return with("pending", "checks_pending", json!({"checks": []}));
        }
        return with("blocked", "no_required_checks", json!({"checks": []}));
    }
    let incomplete = normalized.iter().any(|check| {
        !present(&check["name"]) || !present(&check["state"]) || !present(&check["bucket"])
    });
    if incomplete {
        return with(
            "blocked",
            "missing_check_data",
            json!({"checks": normalized}),
        );
    }
    let has = |wanted: &str| normalized.iter().any(|check| check["bucket"] == wanted);
    if has("fail") || has("cancel") {
        let reason = if has("cancel") {
            "check_cancelled"
        } else {
            "check_failed"
        };
        return with("blocked", reason, json!({"checks": normalized}));
    }
    let known = ["pass", "pending", "skipping"];
    if normalized
        .iter()
        .any(|check| !check["bucket"].as_str().is_some_and(|b| known.contains(&b)))
    {
        return with(
            "blocked",
            "unknown_check_state",
            json!({"checks": normalized}),
        );
    }
    if has("pending") {
        return with("pending", "checks_pending", json!({"checks": normalized}));
    }
    with(
        "complete",
        "required_checks_passed",
        json!({"checks": normalized}),
    )
}

fn normalize_jobs(jobs: &[Value]) -> Vec<Value> {
    sorted(
        jobs.iter()
            .map(|job| {
                json!({
                    "name": job["name"],
                    "state": job["status"],
                    "stage": job["stage"],
                    "link": job["web_url"],
                    "allowFailure": job["allow_failure"],
                })
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

/// The state of a pipeline and its jobs. `merge_status` is the MR's `detailed_merge_status`;
/// a pipeline of its own has none.
fn pipeline_state(pipeline: &Value, jobs: &[Value], merge_status: Option<&Value>) -> State {
    let summary = json!({
        "id": pipeline["id"],
        "status": pipeline["status"],
        "link": pipeline["web_url"],
    });
    let head = |extra: Value| {
        let mut fields = json!({"pipeline": summary});
        if let Some(merge_status) = merge_status {
            fields["mergeStatus"] = merge_status.clone();
        }
        fields
            .as_object_mut()
            .expect("an object")
            .extend(extra.as_object().cloned().unwrap_or_default());
        fields
    };
    let checks = normalize_jobs(jobs);
    if !present(&pipeline["id"])
        || !present(&pipeline["status"])
        || checks
            .iter()
            .any(|check| !present(&check["name"]) || !present(&check["state"]))
    {
        return with(
            "blocked",
            "missing_check_data",
            head(json!({"checks": checks})),
        );
    }
    let (status, reason) = pipeline_reason(pipeline["status"].as_str().unwrap_or(""));
    with(status, reason, head(json!({"checks": checks})))
}

pub fn classify_pipeline(pipeline: &Value, jobs: &[Value]) -> State {
    pipeline_state(pipeline, jobs, None)
}

/// An MR whose head pipeline's jobs were read, or `None` when it has no pipeline.
pub fn classify_gitlab(head: &Head, mr: &Value, jobs: Option<&[Value]>) -> State {
    let state_text = &mr["state"];
    if !mr["sha"].is_string() || !state_text.is_string() {
        return with("blocked", "missing_pull_request_data", json!({"view": mr}));
    }
    let url = &mr["web_url"];
    if mr["sha"] != head.expected {
        return head_mismatch(head, &mr["sha"], url);
    }
    let state = state_text.as_str().and_then(gitlab_state);
    if state == Some(ReviewState::Merged) {
        return with("complete", "merged", json!({"url": url}));
    }
    if state == Some(ReviewState::Closed) {
        return with("blocked", "closed", json!({"url": url}));
    }
    let merge_status = &mr["detailed_merge_status"];
    // GitLab reports `locked` while it merges; the next poll shows `merged`.
    if state_text == "locked" {
        return with(
            "pending",
            "merge_in_progress",
            json!({"url": url, "mergeStatus": merge_status}),
        );
    }
    if state != Some(ReviewState::Open) {
        return with(
            "blocked",
            "unknown_pull_request_state",
            json!({"state": state_text}),
        );
    }
    let pipeline = &mr["head_pipeline"];
    if pipeline.is_null() {
        return with(
            "blocked",
            "no_required_checks",
            json!({"checks": [], "mergeStatus": merge_status}),
        );
    }
    let Some(jobs) = jobs else {
        return with(
            "blocked",
            "missing_check_data",
            json!({
                "pipeline": {"id": pipeline["id"], "status": pipeline["status"], "link": pipeline["web_url"]},
                "mergeStatus": merge_status,
            }),
        );
    };
    pipeline_state(pipeline, jobs, Some(merge_status))
}

/// The checks of a final state that failed or were canceled; GitLab jobs allowed to fail do not
/// count.
pub fn failed_checks(state: &State) -> Vec<Value> {
    state["checks"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter(|check| match check["bucket"].as_str() {
            Some(bucket) => matches!(bucket, "fail" | "cancel"),
            None => check["state"] == "failed" && check["allowFailure"] != true,
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason_at(waited_secs: i64, rollup: usize) -> (String, String) {
        let head = Head {
            expected: "abc",
            awaiting: false,
            waited_secs,
        };
        let view = json!({"headRefOid": "abc", "state": "OPEN", "url": "u"});
        let state = classify(&head, &view, rollup, &[]);
        (status(&state).to_owned(), reason(&state).to_owned())
    }

    #[test]
    fn an_empty_rollup_is_pending_for_the_first_two_minutes_only() {
        assert_eq!(reason_at(0, 0), ("pending".into(), "checks_pending".into()));
        assert_eq!(
            reason_at(119, 0),
            ("pending".into(), "checks_pending".into())
        );
        assert_eq!(
            reason_at(120, 0),
            ("blocked".into(), "no_required_checks".into())
        );
    }

    #[test]
    fn a_rollup_with_checks_but_none_required_is_no_required_checks_at_once() {
        assert_eq!(
            reason_at(0, 2),
            ("blocked".into(), "no_required_checks".into())
        );
    }
}
