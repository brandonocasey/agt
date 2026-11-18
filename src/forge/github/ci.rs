//! The state of a PR and its checks, for `agt ci wait`.

use serde_json::{Value, json};

use super::Github;
use crate::forge::service::{Pagination, Request, Var};
use crate::forge::split_repo_strict;
use crate::forge::transport::TransportError;

const QUERY: &str = "query PullChecks($owner:String!,$name:String!,$number:Int!,$cursor:String){repository(owner:$owner,name:$name){pullRequest(number:$number){number url baseRefName state mergedAt closed headRefOid commits(last:1){nodes{commit{statusCheckRollup{contexts(first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{__typename ... on CheckRun{name status conclusion title detailsUrl isRequired(pullRequestNumber:$number) checkSuite{workflowRun{event workflow{name}}}} ... on StatusContext{context state description targetUrl isRequired(pullRequestNumber:$number)}}}}}}}}}}";

const COMMIT_QUERY: &str = "query CommitChecks($owner:String!,$name:String!,$sha:String!,$cursor:String){repository(owner:$owner,name:$name){object(expression:$sha){__typename ... on Commit{oid url statusCheckRollup{contexts(first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{__typename ... on CheckRun{name status conclusion title detailsUrl checkSuite{workflowRun{event workflow{name}}}} ... on StatusContext{context state description targetUrl}}}}}}}}";

/// Jobs a REST list returns per page.
const JOBS_PER_PAGE: usize = 100;

/// A PR, commit or Actions run and its checks.
pub struct Pull {
    /// `number`, `url`, `state` (`OPEN`, `CLOSED` or `MERGED`), `mergedAt`, `closed` and
    /// `headRefOid`.
    pub view: Value,
    /// `bucket` is `pass`, `fail`, `pending`, `skipping` or `cancel`.
    pub checks: Vec<Value>,
    /// Every check and status, used when no required contexts are configured.
    pub all_checks: Vec<Value>,
    /// How many checks and statuses the rollup holds, required or not.
    pub rollup: usize,
    /// Whether any check or status in the rollup, required or not, has not completed.
    pub running: bool,
    /// The branch the PR merges into.
    pub base: String,
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

/// Whether a rollup entry is queued, in progress or otherwise not completed.
fn unfinished(node: &Value) -> bool {
    if node["__typename"] == "StatusContext" {
        return matches!(node["state"].as_str(), Some("PENDING" | "EXPECTED"));
    }
    node["status"] != "COMPLETED"
}

/// The check or status a rollup entry describes.
fn check(node: &Value) -> Option<Value> {
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

/// One read of a rollup: where its object and contexts are in the GraphQL reply.
struct RollupRead<'a> {
    query: &'a str,
    /// The variables that name the subject, after owner and name.
    subject: Vec<Var>,
    /// Pointer to the PR or commit in the reply.
    root: &'a str,
    /// Pointer to its `contexts` connection, below the root.
    contexts: &'a str,
    not_found: String,
}

/// The object a rollup read found and the contexts of every page.
struct Rollup {
    root: Value,
    /// The contexts GitHub marks required, as checks.
    required: Vec<Value>,
    all: Vec<Value>,
    /// How many contexts the rollup holds, of any kind.
    count: usize,
    running: bool,
}

impl Github {
    fn rollup(&self, repo: &str, read: &RollupRead) -> Result<Rollup, TransportError> {
        let (owner, name) = split_repo_strict(repo)
            .ok_or_else(|| TransportError::new(format!("not an owner/repo: {repo}")))?;
        let mut cursor: Option<String> = None;
        let mut rollup = Rollup {
            root: Value::Null,
            required: Vec::new(),
            all: Vec::new(),
            count: 0,
            running: false,
        };
        loop {
            let mut vars = vec![Var::text("owner", owner), Var::text("name", name)];
            vars.extend(read.subject.iter().cloned());
            vars.push(Var::text("query", read.query));
            vars.extend(cursor.take().map(|cursor| Var::text("cursor", cursor)));
            let reply = self.transport.send(&Request::graphql(vars))?.body;
            let Some(root) = reply.pointer(read.root).filter(|root| root.is_object()) else {
                return Err(TransportError::new(read.not_found.clone()));
            };
            let contexts = root.pointer(read.contexts);
            let nodes = contexts
                .and_then(|contexts| contexts["nodes"].as_array())
                .map_or(&[][..], Vec::as_slice);
            rollup.count += nodes.len();
            rollup.required.extend(
                nodes
                    .iter()
                    .filter(|node| node["isRequired"] == true)
                    .filter_map(check),
            );
            rollup.all.extend(nodes.iter().filter_map(check));
            rollup.running |= nodes.iter().any(unfinished);
            let next = contexts
                .filter(|contexts| contexts["pageInfo"]["hasNextPage"] == true)
                .and_then(|contexts| contexts["pageInfo"]["endCursor"].as_str());
            rollup.root = root.clone();
            match next {
                Some(next) => cursor = Some(next.to_owned()),
                None => return Ok(rollup),
            }
        }
    }

    /// The PR `number` of `repo` (`owner/name`) and its checks, in one read.
    pub fn pull_checks(&self, repo: &str, number: u64) -> Result<Pull, TransportError> {
        let rollup = self.rollup(
            repo,
            &RollupRead {
                query: QUERY,
                subject: vec![Var::typed("number", number.to_string())],
                root: "/data/repository/pullRequest",
                contexts: "/commits/nodes/0/commit/statusCheckRollup/contexts",
                not_found: format!("{repo}#{number} was not found"),
            },
        )?;
        let pull = &rollup.root;
        Ok(Pull {
            view: json!({
                "number": pull["number"],
                "url": pull["url"],
                "state": pull["state"],
                "mergedAt": pull["mergedAt"],
                "closed": pull["closed"],
                "headRefOid": pull["headRefOid"],
            }),
            rollup: rollup.count,
            running: rollup.running,
            base: pull["baseRefName"].as_str().unwrap_or("").to_owned(),
            checks: rollup.required,
            all_checks: rollup.all,
        })
    }

    /// The commit `sha` (full or abbreviated) of `repo` and every check and status of it. No
    /// context is required, so all of them count; `base` is empty.
    pub fn commit_checks(&self, repo: &str, sha: &str) -> Result<Pull, TransportError> {
        let rollup = self.rollup(
            repo,
            &RollupRead {
                query: COMMIT_QUERY,
                subject: vec![Var::text("sha", sha)],
                root: "/data/repository/object",
                contexts: "/statusCheckRollup/contexts",
                not_found: format!("{repo}@{sha} was not found"),
            },
        )?;
        if rollup.root["__typename"] != "Commit" {
            return Err(TransportError::new(format!("{repo}@{sha} is not a commit")));
        }
        Ok(Pull {
            view: json!({
                "number": null,
                "url": rollup.root["url"],
                "state": "OPEN",
                "mergedAt": null,
                "closed": false,
                "headRefOid": rollup.root["oid"],
            }),
            rollup: rollup.count,
            running: rollup.running,
            base: String::new(),
            checks: rollup.all.clone(),
            all_checks: rollup.all,
        })
    }

    /// The Actions run `id` of `repo` and its latest-attempt jobs, as checks. With no job, the run
    /// is one check.
    pub fn run_checks(&self, repo: &str, id: u64) -> Result<Pull, TransportError> {
        let path = format!("repos/{repo}/actions/runs/{id}");
        let run = self.transport.json(&Request::get(path.clone()))?;
        let mut jobs: Vec<Value> = Vec::new();
        for page in 1.. {
            let reply = self.transport.json(&Request::get(format!(
                "{path}/jobs?per_page={JOBS_PER_PAGE}&page={page}"
            )))?;
            let listed = reply["jobs"].as_array().cloned().unwrap_or_default();
            let last = listed.len() < JOBS_PER_PAGE;
            jobs.extend(listed);
            if last {
                break;
            }
        }
        let check_of = |job: &Value| {
            let upper = |field: &str| job[field].as_str().unwrap_or("").to_uppercase();
            let (status, conclusion) = (upper("status"), upper("conclusion"));
            json!({
                "name": job["name"],
                "state": job["conclusion"].as_str().unwrap_or(job["status"].as_str().unwrap_or("")),
                "bucket": bucket(&status, &conclusion),
                "description": "",
                "link": job["html_url"],
                "workflow": run["name"],
                "event": run["event"],
            })
        };
        let checks: Vec<Value> = if jobs.is_empty() {
            vec![check_of(&json!({
                "name": run["name"], "status": run["status"],
                "conclusion": run["conclusion"], "html_url": run["html_url"],
            }))]
        } else {
            jobs.iter().map(check_of).collect()
        };
        Ok(Pull {
            view: json!({
                "number": run["id"],
                "url": run["html_url"],
                "state": "OPEN",
                "mergedAt": null,
                "closed": false,
                "headRefOid": run["head_sha"],
            }),
            rollup: checks.len(),
            running: run["status"] != "completed",
            base: String::new(),
            checks: checks.clone(),
            all_checks: checks,
        })
    }
}

/// The contexts a `required_status_checks` rule or protection lists, in `list`.
fn contexts_of(list: &Value, out: &mut Vec<String>) {
    for entry in list.as_array().into_iter().flatten() {
        if let Some(context) = entry["context"].as_str().or_else(|| entry.as_str()) {
            out.push(context.to_owned());
        }
    }
}

impl Github {
    /// The status check contexts the active `required_status_checks` rules of the rulesets
    /// that apply to branch `branch` of `repo` require before a merge. An unreadable read is
    /// an error.
    pub fn rule_contexts(&self, repo: &str, branch: &str) -> Result<Vec<String>, TransportError> {
        let branch = crate::forge::encode(branch).replace("%2F", "/");
        let rules = self.transport.items(
            &Request::get(format!("repos/{repo}/rules/branches/{branch}?per_page=100")),
            &Pagination::CliPaginate,
        )?;
        let mut contexts = Vec::new();
        for rule in &rules {
            if rule["type"] == "required_status_checks" {
                contexts_of(&rule["parameters"]["required_status_checks"], &mut contexts);
            }
        }
        Ok(contexts)
    }

    /// The status check contexts the classic branch protection of branch `branch` of `repo`
    /// requires, as the branch itself reports them (readable with repository contents access,
    /// unlike the protection endpoints). An unreadable read is an error.
    pub fn protection_contexts(
        &self,
        repo: &str,
        branch: &str,
    ) -> Result<Vec<String>, TransportError> {
        let branch = crate::forge::encode(branch).replace("%2F", "/");
        let info = self
            .transport
            .send(&Request::get(format!("repos/{repo}/branches/{branch}")))?
            .body;
        let classic = &info["protection"]["required_status_checks"];
        let mut contexts = Vec::new();
        contexts_of(&classic["contexts"], &mut contexts);
        contexts_of(&classic["checks"], &mut contexts);
        Ok(contexts)
    }
}
