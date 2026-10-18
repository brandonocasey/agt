//! One poll of a target: the reads through `src/forge` and the state they add up to.

use serde_json::json;

use super::classify::{
    Head, State, blocked, classify, classify_gitlab, classify_pipeline, present,
};
use super::target::{Platform, Resolved};
use crate::forge::github::Github;
use crate::forge::gitlab::Gitlab;
use crate::forge::gitlab::ci::base_of_path;
use crate::forge::transport::{CliTransport, TransportError};

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

/// The state of `target` and the head it is pinned to: the one it was given or, when it had
/// none, the head read now. While `awaiting`, a head or pipeline that has not caught up with
/// the pinned commit is pending, not changed.
pub fn snapshot(
    target: &Resolved,
    head: Option<String>,
    awaiting: bool,
    waited_secs: i64,
) -> (State, Option<String>, bool) {
    let result = match target.platform {
        Platform::Github => github(target, head.clone(), awaiting, waited_secs),
        Platform::Gitlab if target.pipeline => pipeline(target).map(|state| (state, head.clone())),
        Platform::Gitlab => gitlab(target, head.clone(), awaiting, waited_secs),
    };
    match result {
        Ok((state, head)) => (state, head, false),
        Err(error) => {
            let (state, retry) = failure(&error);
            (state, head, retry)
        }
    }
}

type Polled = Result<(State, Option<String>), TransportError>;

fn github(target: &Resolved, head: Option<String>, awaiting: bool, waited_secs: i64) -> Polled {
    let github = Github::new(Box::new(CliTransport::gh(&target.host)));
    let pull = github.pull_checks(&target.repo, target.number)?;
    let head = head.or_else(|| pull.view["headRefOid"].as_str().map(str::to_owned));
    let expected = head.as_deref().unwrap_or("");
    let head_state = Head {
        expected,
        awaiting,
        waited_secs,
    };
    let state = classify(&head_state, &pull.view, pull.rollup, &pull.checks);
    Ok((state, head))
}

fn pipeline(target: &Resolved) -> Result<State, TransportError> {
    let gitlab = Gitlab::new(Box::new(CliTransport::glab(&target.host)));
    let base = base_of_path(&target.repo);
    let pipeline = gitlab.pipeline(&base, target.number)?;
    let jobs = gitlab.pipeline_jobs(&base, target.number, false)?;
    Ok(classify_pipeline(&pipeline, &jobs))
}

fn gitlab(target: &Resolved, head: Option<String>, awaiting: bool, waited_secs: i64) -> Polled {
    let gitlab = Gitlab::new(Box::new(CliTransport::glab(&target.host)));
    let base = base_of_path(&target.repo);
    let mr = gitlab.merge_request(&base, target.number)?;
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
            let state = json!({
                "status": "pending",
                "reason": "waiting_for_pipeline",
                "expectedHead": expected,
                "observedHead": mr["sha"],
                "mergeStatus": mr["detailed_merge_status"],
            });
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
    let latest = gitlab.merge_request(&base, target.number)?;
    let latest_id = &latest["head_pipeline"]["id"];
    if latest["state"] == "opened" && latest["sha"] == expected && latest_id != id {
        let state = json!({
            "status": "pending",
            "reason": "pipeline_changed",
            "expectedPipeline": id,
            "observedPipeline": latest_id,
            "mergeStatus": latest["detailed_merge_status"],
        });
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
