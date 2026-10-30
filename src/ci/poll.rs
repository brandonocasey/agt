//! One poll of a target: the reads through `src/forge` and the state they add up to.

use serde_json::json;

use super::classify::{
    Head, State, blocked, classify, classify_gitlab, classify_pipeline, present, rollup_grace_secs,
    with,
};
use super::target::Resolved;
use crate::forge::gitlab::project_path;
use crate::forge::transport::TransportError;
use crate::forge::{github, gitlab};
use crate::store::model::Forge;

/// Whether repeating the read may succeed: a rate limit, a timeout, a server error or a failure
/// with no HTTP status (the tool or network). Authentication and permission errors are not.
fn transient(error: &TransportError) -> bool {
    match error.status {
        Some(status) => matches!(status, 408 | 425 | 429) || status >= 500,
        None => true,
    }
}

/// A blocked state for a read that failed, named by what the host or tool said, and whether
/// the failure is transient.
fn failure(error: &TransportError) -> (State, bool) {
    let detail = error.message.trim();
    let normalized = detail.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| normalized.contains(word));
    let rate_limited = error.status == Some(429);
    let reason = if rate_limited {
        "query_error"
    } else if error.status == Some(401) || has(&["auth", "login", "token", "credential"]) {
        "authentication_error"
    } else if error.status == Some(403)
        || has(&[
            "forbidden",
            "permission",
            "resource not accessible",
            "denied",
        ])
    {
        "permission_error"
    } else {
        "query_error"
    };
    let mut state = blocked(reason);
    state["detail"] = json!(detail);
    let retry = reason == "query_error" && transient(error);
    (state, retry)
}

/// The state of `target`, the head it is pinned to (the one it was given or, when it had none,
/// the head read now), whether the failure of the read is transient, and when the rollup went
/// idle with a required context missing (`idle_since`, carried from the previous poll). While
/// `awaiting`, a head or pipeline that has not caught up with the pinned commit is pending, not
/// changed.
pub fn snapshot(
    target: &Resolved,
    head: Option<String>,
    awaiting: bool,
    waited_secs: i64,
    idle_since: Option<i64>,
) -> (State, Option<String>, bool, Option<i64>) {
    let result = match target.target.platform {
        Forge::Github => github(target, head.clone(), awaiting, waited_secs, idle_since),
        Forge::Gitlab if target.target.pipeline => {
            pipeline(target).map(|state| (state, head.clone(), None))
        }
        Forge::Gitlab => gitlab(target, head.clone(), awaiting, waited_secs)
            .map(|(state, head)| (state, head, None)),
    };
    match result {
        Ok((state, head, idle_since)) => (state, head, false, idle_since),
        Err(error) => {
            let (state, retry) = failure(&error);
            (state, head, retry, idle_since)
        }
    }
}

type Polled = Result<(State, Option<String>), TransportError>;

fn github(
    target: &Resolved,
    head: Option<String>,
    awaiting: bool,
    waited_secs: i64,
    idle_since: Option<i64>,
) -> Result<(State, Option<String>, Option<i64>), TransportError> {
    let github = github::connect(&target.host);
    let pull = github.pull_checks(&target.repo, target.target.number)?;
    let head = head.or_else(|| pull.view["headRefOid"].as_str().map(str::to_owned));
    let expected = head.as_deref().unwrap_or("");
    let head_state = Head {
        expected,
        awaiting,
        waited_secs,
    };
    let state = classify(&head_state, &pull.view, pull.rollup, &pull.checks);
    let (state, idle_since) =
        require_reported(&github, target, &pull, state, (waited_secs, idle_since))?;
    Ok((state, head, idle_since))
}

/// A passed state held back while a context the base branch requires has not reported: the
/// rollup lists only checks that exist, so a required job that starts late is absent from it.
/// Each source of required contexts (rulesets, classic branch protection) is read on its own, so
/// one source refusing the token never hides what the other lists. A context still missing holds
/// the state back. When a source cannot be read for want of permission and nothing known is
/// missing, the state stands, marked `requiredContexts`; any other failure of a read fails the
/// poll, which is retried.
///
/// A missing context is held only while something can still produce it: while any check in the
/// rollup is not completed the hold stands with `idle: false`. Once none is, the rollup is idle,
/// and a context that stays missing for `rollup_grace_secs` of continuous idleness (a required job
/// that starts late gets that grace) ends the wait as `required_checks_missing`. `clock` is the
/// seconds into the wait and when the idleness began; the returned value is the idleness start to
/// carry to the next poll, `None` when the rollup is not idle with a context missing.
fn require_reported(
    github: &github::Github,
    target: &Resolved,
    pull: &github::ci::Pull,
    state: State,
    clock: (i64, Option<i64>),
) -> Result<(State, Option<i64>), TransportError> {
    let (waited_secs, idle_since) = clock;
    let base = pull.base.as_str();
    if state["reason"] != "required_checks_passed" || base.is_empty() {
        return Ok((state, None));
    }
    let mut contexts = Vec::new();
    let mut unreadable_source = false;
    for read in [
        github.rule_contexts(&target.repo, base),
        github.protection_contexts(&target.repo, base),
    ] {
        match read {
            Ok(found) => contexts.extend(found),
            Err(error) if unreadable(&error) => unreadable_source = true,
            Err(error) => return Err(error),
        }
    }
    contexts.sort();
    contexts.dedup();
    let missing: Vec<String> = contexts
        .into_iter()
        .filter(|context| !pull.names.contains(context))
        .collect();
    let mut state = state;
    if missing.is_empty() {
        if unreadable_source {
            state["requiredContexts"] = json!("unreadable");
        }
        return Ok((state, None));
    }
    let idle = !pull.running;
    let since = idle.then(|| idle_since.unwrap_or(waited_secs));
    if since.is_some_and(|since| waited_secs - since >= rollup_grace_secs()) {
        state["status"] = json!("blocked");
        state["reason"] = json!("required_checks_missing");
    } else {
        state["status"] = json!("pending");
        state["reason"] = json!("checks_pending");
        state["idle"] = json!(idle);
    }
    state["missingChecks"] = json!(missing);
    Ok((state, since))
}

/// Whether a read failed because the token may not see it (or it does not exist), as opposed
/// to a failure that repeating the poll can cure.
fn unreadable(error: &TransportError) -> bool {
    matches!(error.status, Some(403 | 404))
        || error
            .message
            .to_lowercase()
            .contains("resource not accessible")
}

fn pipeline(target: &Resolved) -> Result<State, TransportError> {
    let gitlab = gitlab::connect(&target.host);
    let base = project_path(&target.repo);
    let pipeline = gitlab.pipeline(&base, target.target.number)?;
    let jobs = gitlab.pipeline_jobs(&base, target.target.number, false)?;
    Ok(classify_pipeline(&pipeline, &jobs))
}

fn gitlab(target: &Resolved, head: Option<String>, awaiting: bool, waited_secs: i64) -> Polled {
    let gitlab = gitlab::connect(&target.host);
    let base = project_path(&target.repo);
    let mr = gitlab.merge_request(&base, target.target.number)?;
    let head = head.or_else(|| mr["sha"].as_str().map(str::to_owned));
    let expected = head.as_deref().unwrap_or("");
    let pending = Head {
        expected,
        awaiting,
        waited_secs,
    };
    let id = &mr["head_pipeline"]["id"];
    if awaiting && mr["state"] == "opened" {
        let caught_up =
            mr["sha"] == expected && gitlab.pipeline_runs(&base, &mr["head_pipeline"], expected)?;
        if !caught_up {
            let state = with(
                "pending",
                "waiting_for_pipeline",
                json!({
                    "expectedHead": expected,
                    "observedHead": mr["sha"],
                    "mergeStatus": mr["detailed_merge_status"],
                }),
            );
            return Ok((state, head));
        }
    }
    if mr["sha"] != expected || mr["state"] != "opened" || !present(id) {
        return Ok((classify_gitlab(&pending, &mr, None), head));
    }
    // A fork MR pipeline runs in the source project, so its jobs are not under the target project.
    let pipeline_base = mr["head_pipeline"]["project_id"]
        .as_i64()
        .map_or_else(|| base.clone(), crate::forge::gitlab::ci::base_of_id);
    let jobs = gitlab.pipeline_jobs(&pipeline_base, id.as_u64().unwrap_or(0), false)?;
    let state = classify_gitlab(&pending, &mr, Some(&jobs));
    if super::classify::status(&state) != "complete" {
        return Ok((state, head));
    }
    let latest = gitlab.merge_request(&base, target.target.number)?;
    let latest_id = &latest["head_pipeline"]["id"];
    if latest["state"] == "opened" && latest["sha"] == expected && latest_id != id {
        let state = with(
            "pending",
            "pipeline_changed",
            json!({
                "expectedPipeline": id,
                "observedPipeline": latest_id,
                "mergeStatus": latest["detailed_merge_status"],
            }),
        );
        return Ok((state, head));
    }
    Ok((classify_gitlab(&pending, &latest, Some(&jobs)), head))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_a_failure_by_what_the_host_said() {
        let error = |status, message: &str| TransportError {
            status,
            message: message.to_owned(),
        };
        let reason = |status, message: &str| failure(&error(status, message)).0["reason"].clone();
        assert_eq!(
            reason(Some(401), "gh api failed: nope"),
            "authentication_error"
        );
        assert_eq!(
            reason(None, "glab: 401 Unauthorized, run glab auth login"),
            "authentication_error"
        );
        assert_eq!(reason(Some(403), "gh api failed: nope"), "permission_error");
        assert_eq!(
            reason(None, "Resource not accessible by integration"),
            "permission_error"
        );
        assert_eq!(reason(Some(500), "gh api failed: boom"), "query_error");
        assert_eq!(
            reason(Some(429), "rate limit exceeded for token"),
            "query_error"
        );
        let retried = |status, message: &str| failure(&error(status, message)).1;
        assert!(retried(Some(500), "boom"));
        assert!(retried(Some(429), "rate limit"));
        assert!(retried(None, "run gh: No such file"));
        assert!(!retried(Some(404), "Not Found"));
        assert!(!retried(Some(401), "nope"));
        assert!(!retried(Some(403), "forbidden"));
    }
}
