//! Read-only evidence that a branch landed: the PRs/MRs of a repository whose source branch is
//! that branch, in every state.

use serde_json::Value;

use crate::core::error::{Error, Result};
use crate::forge::service::{self, Pagination, Request, Var};
use crate::forge::transport::Transport;
use crate::forge::{self, PullNode, encode, github, gitlab};
use crate::git::Repo;
use crate::store::model::{Forge, ReviewState};

const GITHUB_QUERY: &str = "query LandedPulls($owner:String!,$name:String!,$branch:String!,$cursor:String){repository(owner:$owner,name:$name){pullRequests(headRefName:$branch,states:[OPEN,CLOSED,MERGED],first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{number url title state mergedAt closedAt baseRefName headRefName headRefOid isCrossRepository}}}}";

/// The forge repository `origin` points at.
pub struct Origin {
    pub forge: Forge,
    pub host: String,
    pub repo: String,
}

pub struct Pull {
    pub number: u64,
    pub url: String,
    pub title: String,
    /// When it was closed, as the forge wrote it.
    pub closed_at: Option<String>,
    /// `None` for a state this build does not know, or a merged one without a timestamp.
    pub state: Option<ReviewState>,
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
        Ok(service::detect(&host, None, None).map(|forge| Self {
            forge,
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
        format!(
            "{}{number}/head",
            forge::service::kind(self.forge).head_ref_prefix
        )
    }

    fn github(&self, transport: &dyn Transport, branch: &str) -> Result<Vec<Pull>> {
        let (owner, name) = forge::split_repo(&self.repo);
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
            "{}/merge_requests?source_branch={}&state=all&per_page=100",
            forge::gitlab::project_path(&self.repo),
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
    pull(
        github::pull_node(node),
        "a GitHub pull request has no number",
    )
}

fn gitlab_pull(node: &Value) -> Result<Pull> {
    pull(gitlab::pull_node(node), "a GitLab merge request has no iid")
}

/// A merged state needs a merge timestamp, and a head branch counts as in the repository only
/// when the node says so.
fn pull(node: PullNode, no_number: &str) -> Result<Pull> {
    Ok(Pull {
        number: node.number.ok_or_else(|| Error(no_number.into()))?,
        url: node.url,
        title: node.title,
        closed_at: node.closed_at,
        state: node
            .state
            .filter(|state| node.merged || *state != ReviewState::Merged),
        base: node.base,
        head_branch: node.head_branch,
        head_sha: node.head_sha,
        same_repo: node.same_repo == Some(true),
    })
}
