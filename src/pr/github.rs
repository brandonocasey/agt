//! GitHub pull requests through GraphQL reads and REST writes.

use serde_json::{Value, json};

use super::provider::{At, Capabilities, Filter, NewPr, Outcome, Provider};
use super::remote::{self, Remote};
use super::upload::{Image, Uploaded, Uploader};
use super::{Changes, How, Native, Pr, State};
use crate::core::error::{Error, Result, fail};
use crate::forge::service::{Method, Request, Var};
use crate::forge::transport::{Transport, TransportError};
use crate::forge::{self, github};
use crate::store::model::ReviewState;

/// The only host whose upload endpoint is known; GitHub Enterprise Server's is not.
const UPLOAD_HOST: &str = "github.com";

const FIELDS: &str = "id number url title state isDraft headRefName headRefOid baseRefName author{login} labels(first:100){nodes{name}} autoMergeRequest{mergeMethod} mergeable mergeStateStatus mergeQueueEntry{state} isMergeQueueEnabled isCrossRepository";

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
    let shared = github::pull_node(node);
    Pr {
        id: text("/id"),
        number: Some(shared.number.unwrap_or_default()),
        url: Some(shared.url),
        title: shared.title,
        body: text("/body"),
        state: shared.state.unwrap_or(ReviewState::Open),
        draft: node["isDraft"].as_bool().unwrap_or(false),
        head_branch: shared.head_branch,
        base_branch: shared.base,
        head_sha: shared.head_sha,
        author: text("/author/login"),
        labels: node
            .pointer("/labels/nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|label| label["name"].as_str().map(str::to_owned))
            .collect(),
        auto_merge_known: true,
        auto_merge: node
            .pointer("/autoMergeRequest/mergeMethod")
            .and_then(Value::as_str)
            .map(str::to_lowercase),
        queue: node
            .pointer("/mergeQueueEntry/state")
            .and_then(Value::as_str)
            .map(str::to_lowercase),
        conflicts: mergeable == "conflicting",
        same_repo: shared.same_repo.unwrap_or(true),
        mergeable: mergeable.to_owned(),
        merge_state,
        native: Native::Github {
            merge_queue: node["isMergeQueueEnabled"].as_bool().unwrap_or(false),
        },
    }
}

fn graphql(t: &dyn Transport, query: &str, vars: Vec<Var>) -> Result<Value> {
    let mut all = vec![Var::text("query", query)];
    all.extend(vars);
    Ok(t.json(&Request::graphql(all))?)
}

pub fn view(t: &dyn Transport, repo: &str, number: u64) -> Result<Pr> {
    let (owner, name) = forge::split_repo(repo);
    let query = format!(
        "query PrView($owner:String!,$name:String!,$number:Int!){{repository(owner:$owner,name:$name){{pullRequest(number:$number){{{FIELDS} body}}}}}}"
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
    let (owner, name) = forge::split_repo(repo);
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
    // Search does not follow renames the way `repository(owner,name)` does, so a remote that
    // still names an old slug would match nothing.
    let (owner, name) = forge::split_repo(repo);
    let renamed = graphql(
        t,
        "query PrRepo($owner:String!,$name:String!){repository(owner:$owner,name:$name){nameWithOwner}}",
        vec![Var::text("owner", owner), Var::text("name", name)],
    )?;
    let current = renamed
        .pointer("/data/repository/nameWithOwner")
        .and_then(Value::as_str)
        .unwrap_or(repo);
    let mut terms = format!("is:pr repo:{current} author:@me sort:created-desc");
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
        // `head:` matches branch names that start with the text; keep exact heads only.
        .filter(|found| branch.is_none_or(|branch| found.head_branch == branch))
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

/// The repository's numeric id, which the upload endpoint asks for.
pub fn repository_id(t: &dyn Transport, repo: &str) -> Result<u64> {
    t.json(&Request::get(format!("repos/{repo}")))
        .map_err(refused)?["id"]
        .as_u64()
        .ok_or_else(|| Error(format!("GitHub returned no repository id for {repo}")))
}

/// Uploads through GitHub's undocumented `user-attachments` endpoint, which takes a user token
/// only.
pub fn upload(t: &dyn Transport, repository_id: u64, image: &Image) -> Result<Uploaded> {
    let path = format!(
        "https://uploads.github.com/user-attachments/assets?name={}&content_type={}&repository_id={repository_id}",
        forge::encode(&image.name),
        forge::encode(image.content_type)
    );
    let reply = t
        .json(&Request::upload(path, image.path.clone()))
        .map_err(refused)?;
    let url = reply["url"]
        .as_str()
        .filter(|url| !url.is_empty())
        .ok_or_else(|| Error("GitHub did not return the uploaded image's URL".into()))?;
    Ok(Uploaded {
        markdown: format!("![{}]({url})", image.alt()),
        url: url.to_owned(),
    })
}

fn refused(error: TransportError) -> Error {
    match error.status {
        Some(401 | 403) => Error(format!(
            "{error}\nGitHub image uploads need a user token (a user's `gh` login or GH_TOKEN), not an app or installation token"
        )),
        _ => error.into(),
    }
}

pub fn update(t: &dyn Transport, repo: &str, current: &Pr, changes: &Changes) -> Result<()> {
    let mut body = serde_json::Map::new();
    if let Some(title) = &changes.title {
        body.insert("title".into(), json!(title));
    }
    if let Some(text) = &changes.body {
        body.insert("body".into(), json!(text));
    }
    if let Some(labels) = changes.merged_labels(&current.labels) {
        body.insert("labels".into(), json!(labels));
    }
    if !body.is_empty() {
        t.send(&Request::rest(
            Method::Patch,
            format!("repos/{repo}/issues/{}", current.forge_number()),
            Some(Value::Object(body)),
        ))?;
    }
    let mutation = match changes.draft {
        Some(false) => "markPullRequestReadyForReview",
        Some(true) => "convertPullRequestToDraft",
        None => return Ok(()),
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

/// Whether the base branch requires a merge queue, which takes a PR only through auto-merge.
fn queue_required(pr: &Pr) -> bool {
    matches!(pr.native, Native::Github { merge_queue: true })
}

pub struct Github(pub Remote);

impl Provider for Github {
    fn at(&self, text: &str) -> Result<At> {
        self.0.at(text)
    }

    fn key(&self, at: &At) -> String {
        self.0.key(at)
    }

    fn place(&self) -> String {
        self.0.origin.repo.clone()
    }

    fn allow_write(&self, what: &str) -> Result<()> {
        self.0.allow_write(what)
    }

    fn explain(&self, error: Error) -> Error {
        remote::explain(error)
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            resumes_auto_merge: false,
            no_text: None,
        }
    }

    fn view(&self, at: &At) -> Result<Pr> {
        let (t, repo) = self.0.api();
        view(t, repo, remote::number(at)?)
    }

    fn list(&self, filter: &Filter) -> Result<Vec<Pr>> {
        let (t, repo) = self.0.api();
        list(t, repo, filter.branch, filter.state, filter.mine)
    }

    fn create(&self, new: &NewPr) -> Result<At> {
        let (t, repo) = self.0.api();
        let number = create(t, repo, new.head, new.base, new.title, new.body, new.draft)?;
        Ok(At::Number(number))
    }

    fn update(&self, current: &Pr, changes: &Changes) -> Result<()> {
        let (t, repo) = self.0.api();
        update(t, repo, current, changes)
    }

    fn upload_check(&self) -> Result<()> {
        if self.0.origin.host != UPLOAD_HOST {
            return fail(format!(
                "agt pr upload supports GitHub only on {UPLOAD_HOST}, not {}",
                self.0.origin.host
            ));
        }
        Ok(())
    }

    fn uploader(&self) -> Result<Uploader<'_>> {
        let (t, repo) = self.0.api();
        let id = repository_id(t, repo)?;
        Ok(Box::new(move |image| upload(t, id, image)))
    }

    /// A base that requires a merge queue takes a PR only through auto-merge.
    /// That queues it, as `gh pr merge` does.
    fn merge(&self, pr: &Pr, sha: &str, how: Option<How>) -> Result<Outcome> {
        if queue_required(pr) {
            return self.enable_auto(pr, sha, how);
        }
        let (t, repo) = self.0.api();
        merge(t, repo, pr.forge_number(), sha, how)?;
        Ok(Outcome::Done)
    }

    fn enable_auto(&self, pr: &Pr, sha: &str, how: Option<How>) -> Result<Outcome> {
        let how = how
            .or_else(|| pr.auto_merge.as_deref().and_then(How::named))
            .ok_or_else(|| Error("GitHub auto-merge needs --method".into()))?;
        let (t, repo) = self.0.api();
        // GitHub offers auto-merge only while a PR cannot merge yet; like gh, merge a PR that
        // can merge now, unless the base requires a merge queue: auto-merge is what queues it.
        if !queue_required(pr)
            && matches!(pr.merge_state.as_str(), "clean" | "has_hooks" | "unstable")
        {
            merge(t, repo, pr.forge_number(), sha, Some(how))?;
            return Ok(Outcome::Done);
        }
        enable_auto(t, pr, sha, how)?;
        Ok(Outcome::Done)
    }

    fn disable_auto(&self, pr: &Pr) -> Result<bool> {
        let (t, _) = self.0.api();
        disable_auto(t, pr).map(|()| true)
    }

    fn auto_method(&self, pr: &Pr, _recorded: Option<&str>) -> Option<String> {
        pr.auto_merge.clone()
    }
}
