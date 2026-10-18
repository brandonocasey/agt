//! The state of a PR and its required checks, for `agt ci wait`.

use serde_json::{Value, json};

use super::Github;
use crate::forge::service::{Request, Var};
use crate::forge::transport::TransportError;

const QUERY: &str = "query PullChecks($owner:String!,$name:String!,$number:Int!,$cursor:String){repository(owner:$owner,name:$name){pullRequest(number:$number){number url state mergedAt closed headRefOid commits(last:1){nodes{commit{statusCheckRollup{contexts(first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{__typename ... on CheckRun{name status conclusion title detailsUrl isRequired(pullRequestNumber:$number) checkSuite{workflowRun{event workflow{name}}}} ... on StatusContext{context state description targetUrl isRequired(pullRequestNumber:$number)}}}}}}}}}}";

/// A PR and its required checks of the head commit.
pub struct Pull {
    /// `number`, `url`, `state` (`OPEN`, `CLOSED` or `MERGED`), `mergedAt`, `closed` and
    /// `headRefOid`.
    pub view: Value,
    /// `name`, `state`, `bucket` (`pass`, `fail`, `pending`, `skipping` or `cancel`),
    /// `description`, `link`, `workflow` and `event` of each.
    pub checks: Vec<Value>,
    /// How many checks and statuses the rollup holds, required or not.
    pub rollup: usize,
}

fn bucket(status: &str, conclusion: &str) -> String {
    if status != "COMPLETED" {
        return "pending".to_owned();
    }
    match conclusion {
        "SUCCESS" => "pass",
        "NEUTRAL" | "SKIPPED" => "skipping",
        "CANCELLED" => "cancel",
        "FAILURE" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE" | "STALE" => "fail",
        other => return other.to_lowercase(),
    }
    .to_owned()
}

fn status_bucket(state: &str) -> String {
    match state {
        "SUCCESS" => "pass",
        "ERROR" | "FAILURE" => "fail",
        "PENDING" | "EXPECTED" => "pending",
        other => return other.to_lowercase(),
    }
    .to_owned()
}

/// The required check a rollup entry describes, if it is one.
fn check(node: &Value) -> Option<Value> {
    if node["isRequired"] != true {
        return None;
    }
    let text = |pointer: &str| node.pointer(pointer).and_then(Value::as_str).unwrap_or("");
    match text("/__typename") {
        "CheckRun" => {
            let state = [text("/conclusion"), text("/status")]
                .into_iter()
                .find(|state| !state.is_empty())
                .unwrap_or("");
            Some(json!({
                "name": node["name"],
                "state": state,
                "bucket": bucket(text("/status"), text("/conclusion")),
                "description": node["title"],
                "link": node["detailsUrl"],
                "workflow": node.pointer("/checkSuite/workflowRun/workflow/name"),
                "event": node.pointer("/checkSuite/workflowRun/event"),
            }))
        }
        "StatusContext" => Some(json!({
            "name": node["context"],
            "state": node["state"],
            "bucket": status_bucket(text("/state")),
            "description": node["description"],
            "link": node["targetUrl"],
            "workflow": "",
            "event": "",
        })),
        _ => None,
    }
}

impl Github {
    /// The PR `number` of `repo` (`owner/name`) and its required checks, in one read.
    pub fn pull_checks(&self, repo: &str, number: u64) -> Result<Pull, TransportError> {
        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| TransportError::new(format!("not an owner/repo: {repo}")))?;
        let mut cursor: Option<String> = None;
        let mut checks = Vec::new();
        let mut rollup = 0;
        loop {
            let mut vars = vec![
                Var::text("owner", owner),
                Var::text("name", name),
                Var::typed("number", number.to_string()),
                Var::text("query", QUERY),
            ];
            vars.extend(cursor.take().map(|cursor| Var::text("cursor", cursor)));
            let reply = self.transport.send(&Request::graphql(vars))?.body;
            let Some(pull) = reply
                .pointer("/data/repository/pullRequest")
                .filter(|pull| pull.is_object())
            else {
                return Err(TransportError::new(format!(
                    "{repo}#{number} was not found"
                )));
            };
            let contexts = pull.pointer("/commits/nodes/0/commit/statusCheckRollup/contexts");
            let nodes = contexts
                .and_then(|contexts| contexts["nodes"].as_array())
                .map_or(&[][..], Vec::as_slice);
            rollup += nodes.len();
            checks.extend(nodes.iter().filter_map(check));
            let next = contexts
                .filter(|contexts| contexts["pageInfo"]["hasNextPage"] == true)
                .and_then(|contexts| contexts["pageInfo"]["endCursor"].as_str());
            if let Some(next) = next {
                cursor = Some(next.to_owned());
                continue;
            }
            return Ok(Pull {
                view: json!({
                    "number": pull["number"],
                    "url": pull["url"],
                    "state": pull["state"],
                    "mergedAt": pull["mergedAt"],
                    "closed": pull["closed"],
                    "headRefOid": pull["headRefOid"],
                }),
                checks,
                rollup,
            });
        }
    }
}
