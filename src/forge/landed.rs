//! Read-only evidence that a branch landed: the PRs/MRs of a repository whose source branch is
//! that branch, in every state.

use serde_json::Value;

use crate::core::error::{Error, Result};
use crate::forge::service::{Kind, Pagination, Request, SERVICES, Var};
use crate::forge::transport::Transport;
use crate::forge::{self, encode};
use crate::git::Repo;
use crate::store::model::Forge;

const GITHUB_QUERY: &str = "query LandedPulls($owner:String!,$name:String!,$branch:String!,$cursor:String){repository(owner:$owner,name:$name){pullRequests(headRefName:$branch,states:[OPEN,CLOSED,MERGED],first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{number url title state mergedAt closedAt baseRefName headRefName headRefOid isCrossRepository}}}}";

/// The forge repository `origin` points at.
pub struct Origin {
    pub forge: Forge,
    pub host: String,
    pub repo: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Open,
    /// Merged, with a merged timestamp.
    Merged,
    Closed,
    /// Any other state, or a merged one without a timestamp.
    Unknown,
}

pub struct Pull {
    pub number: u64,
    pub url: String,
    pub title: String,
    /// When it was closed, as the forge wrote it.
    pub closed_at: Option<String>,
    pub state: State,
    pub base: String,
    pub head_branch: String,
    pub head_sha: String,
    /// The head branch lives in the repository itself, not in a fork.
    pub same_repo: bool,
}

impl Origin {
    /// The forge `origin` is on; `None` when there is no origin or it is no GitHub or GitLab
    /// host that `gh` or `glab` knows.
    pub fn of(repo: &Repo) -> Result<Option<Self>> {
        let Some((host, path)) = repo.origin_location()? else {
            return Ok(None);
        };
        let by =
            |check: fn(&Kind) -> fn(&str) -> bool| SERVICES.iter().find(|kind| check(kind)(&host));
        let found = by(|kind| kind.well_known)
            .or_else(|| by(|kind| kind.configured))
            .or_else(|| by(|kind| kind.probe));
        Ok(found.map(|kind| Self {
            forge: kind.forge,
            host,
            repo: path,
        }))
    }

    /// Every PR/MR of the repository whose source branch is `branch`, all states, all pages.
    pub fn pulls(&self, branch: &str) -> Result<Vec<Pull>> {
        let service = (forge::service::kind(self.forge).connect)(&self.host);
        let transport = service.transport();
        match self.forge {
            Forge::Github => self.github(transport, branch),
            Forge::Gitlab => self.gitlab(transport, branch),
        }
    }

    /// The ref on `origin` that holds the head of PR/MR `number`.
    pub fn head_ref(&self, number: u64) -> String {
        match self.forge {
            Forge::Github => format!("refs/pull/{number}/head"),
            Forge::Gitlab => format!("refs/merge-requests/{number}/head"),
        }
    }

    fn github(&self, transport: &dyn Transport, branch: &str) -> Result<Vec<Pull>> {
        let (owner, name) = self.repo.split_once('/').unwrap_or_default();
        let mut pulls = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut vars = vec![
                Var::text("owner", owner),
                Var::text("name", name),
                Var::text("branch", branch),
                Var::text("query", GITHUB_QUERY),
            ];
            if let Some(cursor) = &cursor {
                vars.push(Var::text("cursor", cursor.clone()));
            }
            let response = transport.json(&Request::graphql(vars))?;
            let page = response
                .pointer("/data/repository/pullRequests")
                .ok_or_else(|| Error("GitHub returned no pull requests".into()))?;
            for node in page["nodes"].as_array().into_iter().flatten() {
                pulls.push(github_pull(node)?);
            }
            cursor = (page.pointer("/pageInfo/hasNextPage") == Some(&Value::Bool(true)))
                .then(|| forge::text(page, "/pageInfo/endCursor"))
                .flatten();
            if cursor.is_none() {
                return Ok(pulls);
            }
        }
    }

    fn gitlab(&self, transport: &dyn Transport, branch: &str) -> Result<Vec<Pull>> {
        let path = format!(
            "projects/{}/merge_requests?source_branch={}&state=all&per_page=100",
            encode(&self.repo),
            encode(branch)
        );
        transport
            .items(&Request::get(path), &Pagination::CliPaginate)?
            .iter()
            .map(gitlab_pull)
            .collect()
    }
}

fn github_pull(node: &Value) -> Result<Pull> {
    let merged = node["mergedAt"].as_str().is_some();
    let state = match node["state"].as_str() {
        Some("OPEN") => State::Open,
        Some("CLOSED") => State::Closed,
        Some("MERGED") if merged => State::Merged,
        _ => State::Unknown,
    };
    Ok(Pull {
        number: node["number"]
            .as_u64()
            .ok_or_else(|| Error("a GitHub pull request has no number".into()))?,
        url: forge::text(node, "/url").unwrap_or_default(),
        title: forge::text(node, "/title").unwrap_or_default(),
        closed_at: forge::text(node, "/closedAt"),
        state,
        base: forge::text(node, "/baseRefName").unwrap_or_default(),
        head_branch: forge::text(node, "/headRefName").unwrap_or_default(),
        head_sha: forge::text(node, "/headRefOid").unwrap_or_default(),
        same_repo: node["isCrossRepository"] == Value::Bool(false),
    })
}

fn gitlab_pull(node: &Value) -> Result<Pull> {
    let merged = node["merged_at"].as_str().is_some();
    let state = match node["state"].as_str() {
        Some("opened" | "locked") => State::Open,
        Some("closed") => State::Closed,
        Some("merged") if merged => State::Merged,
        _ => State::Unknown,
    };
    let (source, target) = (
        node["source_project_id"].as_u64(),
        node["target_project_id"].as_u64(),
    );
    Ok(Pull {
        number: node["iid"]
            .as_u64()
            .ok_or_else(|| Error("a GitLab merge request has no iid".into()))?,
        url: forge::text(node, "/web_url").unwrap_or_default(),
        title: forge::text(node, "/title").unwrap_or_default(),
        closed_at: forge::text(node, "/closed_at"),
        state,
        base: forge::text(node, "/target_branch").unwrap_or_default(),
        head_branch: forge::text(node, "/source_branch").unwrap_or_default(),
        head_sha: forge::text(node, "/sha").unwrap_or_default(),
        same_repo: source.is_some() && source == target,
    })
}
