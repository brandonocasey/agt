//! GitHub pull requests through GraphQL reads and REST writes.

use serde_json::{Value, json};

use super::{Changes, How, Pr, State};
use crate::core::error::{Error, Result, fail};
use crate::forge::service::{Method, Request, Var};
use crate::forge::transport::Transport;

const FIELDS: &str = "id number url title state isDraft headRefName headRefOid baseRefName author{login} labels(first:100){nodes{name}} autoMergeRequest{mergeMethod} mergeable mergeStateStatus mergeQueueEntry{state} isCrossRepository";

fn split(repo: &str) -> (&str, &str) {
    repo.split_once('/').unwrap_or_default()
}

fn pr(node: &Value) -> Pr {
    let text = |pointer: &str| {
        node.pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let merge_state = text("/mergeStateStatus").to_lowercase();
    let mergeable = match (text("/mergeable").as_str(), merge_state.as_str()) {
        ("CONFLICTING", _) | (_, "dirty") => "conflicting",
        ("MERGEABLE", "blocked" | "behind" | "draft") => "blocked",
        ("MERGEABLE", _) => "mergeable",
        _ => "unknown",
    };
    Pr {
        id: text("/id"),
        number: node["number"].as_u64().unwrap_or_default(),
        url: text("/url"),
        title: text("/title"),
        state: text("/state").to_lowercase(),
        draft: node["isDraft"].as_bool().unwrap_or(false),
        head_branch: text("/headRefName"),
        base_branch: text("/baseRefName"),
        head_sha: text("/headRefOid"),
        author: text("/author/login"),
        labels: node
            .pointer("/labels/nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|label| label["name"].as_str().map(str::to_owned))
            .collect(),
        auto_merge: node
            .pointer("/autoMergeRequest/mergeMethod")
            .and_then(Value::as_str)
            .map(str::to_lowercase),
        queue: node
            .pointer("/mergeQueueEntry/state")
            .and_then(Value::as_str)
            .map(str::to_lowercase),
        conflicts: mergeable == "conflicting",
        same_repo: node["isCrossRepository"].as_bool() != Some(true),
        squash: false,
        mergeable: mergeable.to_owned(),
        merge_state,
    }
}

fn graphql(t: &dyn Transport, query: &str, vars: Vec<Var>) -> Result<Value> {
    let mut all = vec![Var::text("query", query)];
    all.extend(vars);
    t.json(&Request::graphql(all))
}

pub fn view(t: &dyn Transport, repo: &str, number: u64) -> Result<Pr> {
    let (owner, name) = split(repo);
    let query = format!(
        "query PrView($owner:String!,$name:String!,$number:Int!){{repository(owner:$owner,name:$name){{pullRequest(number:$number){{{FIELDS}}}}}}}"
    );
    let reply = graphql(
        t,
        &query,
        vec![
            Var::text("owner", owner),
            Var::text("name", name),
            Var::typed("number", number.to_string()),
        ],
    )?;
    match reply.pointer("/data/repository/pullRequest") {
        Some(node) if !node.is_null() => Ok(pr(node)),
        _ => fail(format!("no pull request #{number} in {repo}")),
    }
}

/// Pull requests newest first; `branch` limits them to one head branch and `mine` to the
/// signed-in user's.
pub fn list(
    t: &dyn Transport,
    repo: &str,
    branch: Option<&str>,
    state: State,
    mine: bool,
) -> Result<Vec<Pr>> {
    if mine {
        return search_mine(t, repo, branch, state);
    }
    let (owner, name) = split(repo);
    let states = match state {
        State::Open => "OPEN",
        State::Merged => "MERGED",
        State::All => "OPEN,CLOSED,MERGED",
    };
    let query = format!(
        "query PrList($owner:String!,$name:String!,$branch:String){{viewer{{login}} repository(owner:$owner,name:$name){{pullRequests(first:100,headRefName:$branch,states:[{states}],orderBy:{{field:CREATED_AT,direction:DESC}}){{nodes{{{FIELDS}}}}}}}}}"
    );
    let mut vars = vec![Var::text("owner", owner), Var::text("name", name)];
    if let Some(branch) = branch {
        vars.push(Var::text("branch", branch));
    }
    let reply = graphql(t, &query, vars)?;
    let nodes = reply
        .pointer("/data/repository/pullRequests/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| Error(format!("GitHub returned no pull requests for {repo}")))?;
    Ok(nodes.iter().map(pr).collect())
}

/// The signed-in user's pull requests, filtered by the server so older ones are found too.
fn search_mine(
    t: &dyn Transport,
    repo: &str,
    branch: Option<&str>,
    state: State,
) -> Result<Vec<Pr>> {
    let mut terms = format!("is:pr repo:{repo} author:@me sort:created-desc");
    match state {
        State::Open => terms.push_str(" is:open"),
        State::Merged => terms.push_str(" is:merged"),
        State::All => {}
    }
    if let Some(branch) = branch {
        terms.push_str(&format!(" head:{branch}"));
    }
    let query = format!(
        "query PrSearch($q:String!){{search(query:$q,type:ISSUE,first:100){{nodes{{... on PullRequest{{{FIELDS}}}}}}}}}"
    );
    let reply = graphql(t, &query, vec![Var::text("q", terms)])?;
    let nodes = reply
        .pointer("/data/search/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| Error(format!("GitHub returned no pull requests for {repo}")))?;
    Ok(nodes
        .iter()
        .filter(|node| node["number"].is_u64())
        .map(pr)
        .collect())
}

pub fn create(
    t: &dyn Transport,
    repo: &str,
    head: &str,
    base: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<u64> {
    let body = json!({"title": title, "body": body, "head": head, "base": base, "draft": draft});
    let created = t.json(&Request::rest(
        Method::Post,
        format!("repos/{repo}/pulls"),
        Some(body),
    ))?;
    created["number"]
        .as_u64()
        .ok_or_else(|| Error("GitHub did not return the new pull request's number".into()))
}

pub fn update(t: &dyn Transport, repo: &str, current: &Pr, changes: &Changes) -> Result<()> {
    let mut body = serde_json::Map::new();
    if let Some(title) = &changes.title {
        body.insert("title".into(), json!(title));
    }
    if let Some(text) = &changes.body {
        body.insert("body".into(), json!(text));
    }
    if !changes.add_labels.is_empty() || !changes.remove_labels.is_empty() {
        let mut labels: Vec<&String> = current
            .labels
            .iter()
            .filter(|label| !changes.remove_labels.contains(label))
            .collect();
        for label in &changes.add_labels {
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        body.insert("labels".into(), json!(labels));
    }
    if !body.is_empty() {
        t.send(&Request::rest(
            Method::Patch,
            format!("repos/{repo}/issues/{}", current.number),
            Some(Value::Object(body)),
        ))?;
    }
    let mutation = match (changes.ready, changes.draft) {
        (true, _) => "markPullRequestReadyForReview",
        (_, true) => "convertPullRequestToDraft",
        _ => return Ok(()),
    };
    let query = format!(
        "mutation PrDraft($id:ID!){{{mutation}(input:{{pullRequestId:$id}}){{pullRequest{{number}}}}}}"
    );
    graphql(t, &query, vec![Var::text("id", current.id.clone())]).map(|_| ())
}

fn method(how: How) -> &'static str {
    match how {
        How::Merge => "merge",
        How::Squash => "squash",
        How::Rebase => "rebase",
    }
}

/// Merges at once; GitHub refuses when the head is no longer `sha`.
pub fn merge(
    t: &dyn Transport,
    repo: &str,
    number: u64,
    sha: &str,
    how: Option<How>,
) -> Result<()> {
    let mut body = json!({"sha": sha});
    if let Some(how) = how {
        body["merge_method"] = json!(method(how));
    }
    t.send(&Request::rest(
        Method::Put,
        format!("repos/{repo}/pulls/{number}/merge"),
        Some(body),
    ))
    .map(|_| ())
    .map_err(Error::from)
}

pub fn enable_auto(t: &dyn Transport, current: &Pr, sha: &str, how: How) -> Result<()> {
    let query = "mutation PrAuto($id:ID!,$method:PullRequestMergeMethod!,$sha:GitObjectID){enablePullRequestAutoMerge(input:{pullRequestId:$id,mergeMethod:$method,expectedHeadOid:$sha}){pullRequest{number}}}";
    graphql(
        t,
        query,
        vec![
            Var::text("id", current.id.clone()),
            Var::text("method", method(how).to_uppercase()),
            Var::text("sha", sha),
        ],
    )
    .map(|_| ())
}

pub fn disable_auto(t: &dyn Transport, current: &Pr) -> Result<()> {
    let query = "mutation PrAutoOff($id:ID!){disablePullRequestAutoMerge(input:{pullRequestId:$id}){pullRequest{number}}}";
    graphql(t, query, vec![Var::text("id", current.id.clone())]).map(|_| ())
}
