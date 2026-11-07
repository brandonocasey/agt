//! The state of a PR and the commit statuses of its head, for `agt ci wait`. Forgejo Actions
//! and external CI both report as commit statuses, so the combined status holds all of them.

use serde_json::{Value, json};

use super::{Forgejo, PAGE_SIZE, state_of};
use crate::forge::service::Request;
use crate::forge::transport::{TransportError, with_page};
use crate::store::model::ReviewState;

/// A PR and the statuses of its head commit.
pub struct Pull {
    /// `number`, `url`, `state` (`OPEN`, `CLOSED` or `MERGED`), `mergedAt`, `closed` and
    /// `headRefOid`, as the GitHub view names them.
    pub view: Value,
    /// `name`, `state`, `bucket` (`pass`, `fail`, `pending` or `skipping`), `description`,
    /// `link`, `workflow` and `event` of each status.
    pub checks: Vec<Value>,
}

fn bucket(state: &str) -> String {
    match state {
        "success" => "pass",
        "error" | "failure" => "fail",
        "pending" => "pending",
        "warning" | "skipped" => "skipping",
        other => return other.to_owned(),
    }
    .to_owned()
}

fn check(status: &Value) -> Value {
    let state = status["status"].as_str().unwrap_or_default();
    json!({
        "name": status["context"],
        "state": status["status"],
        "bucket": bucket(state),
        "description": status["description"],
        "link": status["target_url"],
        "workflow": "",
        "event": "",
    })
}

fn view(pull: &Value) -> Value {
    let state = match state_of(pull) {
        Some(ReviewState::Merged) => "MERGED",
        Some(ReviewState::Closed) => "CLOSED",
        _ => "OPEN",
    };
    json!({
        "number": pull["number"],
        "url": pull["html_url"],
        "state": state,
        "mergedAt": pull["merged_at"],
        "closed": pull["state"] == "closed",
        "headRefOid": pull["head"]["sha"],
    })
}

impl Forgejo {
    /// The PR `number` of `repo` (`owner/name`) and the statuses of its head, in two reads.
    pub fn pull_checks(&self, repo: &str, number: u64) -> Result<Pull, TransportError> {
        let pull = self
            .transport
            .json(&Request::get(format!("repos/{repo}/pulls/{number}")))?;
        let head = pull["head"]["sha"]
            .as_str()
            .ok_or_else(|| TransportError::new(format!("{repo}#{number} has no head commit")))?;
        let checks = self.statuses(repo, head)?.iter().map(check).collect();
        Ok(Pull {
            view: view(&pull),
            checks,
        })
    }

    /// Whether any status exists for `sha` of `repo`.
    pub fn has_statuses(&self, repo: &str, sha: &str) -> Result<bool, TransportError> {
        Ok(!self.statuses(repo, sha)?.is_empty())
    }

    /// The latest status of each context of `sha`, every page: the combined status is one
    /// object, so its `statuses` are paged by hand.
    fn statuses(&self, repo: &str, sha: &str) -> Result<Vec<Value>, TransportError> {
        let path = format!("repos/{repo}/commits/{sha}/status");
        let mut all = Vec::new();
        for page in 1.. {
            let reply = self
                .transport
                .json(&Request::get(with_page(&path, page, PAGE_SIZE)))?;
            let statuses = reply["statuses"].as_array().cloned().unwrap_or_default();
            let last = statuses.len() < PAGE_SIZE;
            all.extend(statuses);
            if last {
                break;
            }
        }
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_become_checks_with_a_bucket() {
        let status = |state: &str| {
            check(
                &json!({"context": "ci/build", "status": state, "description": "d",
                "target_url": "https://h/o/r/actions/runs/4/jobs/0"}),
            )
        };
        let buckets: Vec<_> = ["success", "failure", "error", "pending", "warning", "odd"]
            .iter()
            .map(|state| status(state)["bucket"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            buckets,
            ["pass", "fail", "fail", "pending", "skipping", "odd"]
        );
        let one = status("success");
        assert_eq!(one["name"], "ci/build");
        assert_eq!(one["link"], "https://h/o/r/actions/runs/4/jobs/0");
    }

    #[test]
    fn the_view_names_the_head_and_the_state_as_github_does() {
        let merged = view(&json!({"number": 3, "html_url": "u", "state": "closed",
            "merged": true, "merged_at": "t", "head": {"sha": "abc"}}));
        assert_eq!(merged["state"], "MERGED");
        assert_eq!(merged["headRefOid"], "abc");
        let closed = view(&json!({"state": "closed", "merged": false, "head": {"sha": "abc"}}));
        assert_eq!(
            (closed["state"].clone(), closed["closed"].clone()),
            ("CLOSED".into(), true.into())
        );
        let open = view(&json!({"state": "open", "head": {"sha": "abc"}}));
        assert_eq!(
            (open["state"].clone(), open["closed"].clone()),
            ("OPEN".into(), false.into())
        );
    }
}
