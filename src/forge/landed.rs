//! Read-only evidence that a branch landed: the PRs/MRs of a repository whose source branch is
//! that branch, in every state.

use serde_json::Value;

use crate::core::config::{Config, Kind};
use crate::core::error::{Error, Result};
use crate::forge::select::{self, Selected};
use crate::forge::service::{self, Pagination, Request, Var};
use crate::forge::transport::{Transport, TransportError};
use crate::forge::{self, PullNode, encode, github, gitlab, hosts};
use crate::git::Repo;
use crate::store::model::{Forge, ReviewState};

const GITHUB_QUERY: &str = "query LandedPulls($owner:String!,$name:String!,$branch:String!,$cursor:String){repository(owner:$owner,name:$name){pullRequests(headRefName:$branch,states:[OPEN,CLOSED,MERGED],first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{number url title state mergedAt closedAt baseRefName headRefName headRefOid isCrossRepository createdAt author{login} isDraft}}}}";

const GITHUB_OPEN_QUERY: &str = "query OpenPulls($owner:String!,$name:String!,$cursor:String){repository(owner:$owner,name:$name){pullRequests(states:[OPEN],first:100,after:$cursor){pageInfo{hasNextPage endCursor} nodes{number url title state mergedAt closedAt baseRefName headRefName headRefOid isCrossRepository createdAt author{login} isDraft}}}}";

const GITHUB_ONE_QUERY: &str = "query OnePull($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){number url title state mergedAt closedAt baseRefName headRefName headRefOid isCrossRepository createdAt author{login} isDraft}}}";

/// A forge repository that serves pull requests, and the git remote that fetches from it.
pub struct Source {
    pub forge: Forge,
    pub host: String,
    pub repo: String,
    /// `None` for a tracker no git remote points at.
    pub remote: Option<String>,
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
    pub created_at: Option<String>,
    pub author: Option<String>,
    pub draft: bool,
}

impl Source {
    /// The repository a selected tracker names, on the forge of its tracker or, for a repository
    /// no tracker names, the forge agt identifies from its host.
    pub fn of(selected: &Selected) -> Result<Self> {
        let forge = match &selected.tracker {
            Some(tracker) => tracker.forge,
            None => service::detect(&selected.host, hosts::named(&selected.host)?, None)
                .ok_or_else(|| {
                    Error(format!(
                        "origin is not a GitHub or GitLab host that gh or glab is logged in to ({} is an unknown forge, or gh/glab is not authenticated for it)",
                        selected.host
                    ))
                })?,
        };
        Ok(Self {
            forge,
            host: selected.host.clone(),
            repo: selected.repo.clone(),
            remote: selected.tracker.as_ref().and_then(|t| t.remote.clone()),
        })
    }

    /// The repository of `origin`, with the hosts file as the first hint; `None` when there is
    /// no origin or it is no GitHub or GitLab host that agt can identify.
    pub fn origin(repo: &Repo) -> Result<Option<Self>> {
        let Some((host, path)) = repo.origin_location()? else {
            return Ok(None);
        };
        Ok(
            service::detect(&host, hosts::named(&host)?, None).map(|forge| Self {
                forge,
                host,
                repo: path,
                remote: Some("origin".into()),
            }),
        )
    }

    /// Every tracker that serves pull requests, one source each.
    ///
    /// Besides them, `origin` when only a CLI login identifies its host (GitHub Enterprise, a
    /// self-hosted GitLab), as for every other command acting on pull requests.
    pub fn trackers(repo: &Repo, config: &Config) -> Result<Vec<Self>> {
        let mut sources: Vec<Self> = config
            .trackers_for(repo, Kind::PullRequests)?
            .into_iter()
            .map(|tracker| Self {
                forge: tracker.forge,
                host: tracker.host,
                repo: tracker.repo,
                remote: tracker.remote,
            })
            .collect();
        if let Some(origin) = select::origin_fallback(repo, config, Kind::PullRequests)?
            && !sources.iter().any(|source| {
                source.host.eq_ignore_ascii_case(&origin.host)
                    && source.repo.eq_ignore_ascii_case(&origin.repo)
            })
            && let Ok(source) = Self::of(&origin)
        {
            sources.push(Self {
                remote: Some("origin".into()),
                ..source
            });
        }
        Ok(sources)
    }

    /// Every PR/MR of the repository whose source branch is `branch`, all states, all pages.
    pub fn pulls(&self, branch: &str) -> Result<Vec<Pull>> {
        let service = (forge::service::kind(self.forge).connect)(&self.host);
        let transport = service.transport();
        match self.forge {
            Forge::Github => self.github(transport, Some(branch)),
            Forge::Gitlab => self.gitlab(transport, Some(branch)),
        }
    }

    /// Every open PR/MR of the repository, all pages.
    pub fn open_pulls(&self) -> Result<Vec<Pull>> {
        let service = (forge::service::kind(self.forge).connect)(&self.host);
        let transport = service.transport();
        match self.forge {
            Forge::Github => self.github(transport, None),
            Forge::Gitlab => self.gitlab(transport, None),
        }
    }

    /// PR/MR `number` in any state; `None` when the repository has no such number.
    pub fn pull(&self, number: u64) -> Result<Option<Pull>> {
        let service = (forge::service::kind(self.forge).connect)(&self.host);
        let transport = service.transport();
        // Only GitHub says which object is missing; a GitLab 404 may be the project itself, so
        // it stays an error the caller can report.
        let missing = |error: &TransportError| {
            self.forge == Forge::Github
                && error.message.contains("Could not resolve to a PullRequest")
        };
        match self.forge {
            Forge::Github => {
                let (owner, name) = forge::split_repo(&self.repo);
                let vars = vec![
                    Var::text("owner", owner),
                    Var::text("name", name),
                    Var::typed("number", number.to_string()),
                    Var::text("query", GITHUB_ONE_QUERY),
                ];
                match transport.json(&Request::graphql(vars)) {
                    Ok(response) => response
                        .pointer("/data/repository/pullRequest")
                        .filter(|node| !node.is_null())
                        .map(github_pull)
                        .transpose(),
                    Err(error) if missing(&error) => Ok(None),
                    Err(error) => Err(error.into()),
                }
            }
            Forge::Gitlab => {
                let path = format!(
                    "{}/merge_requests/{number}",
                    forge::gitlab::project_path(&self.repo)
                );
                match transport.json(&Request::get(path)) {
                    Ok(node) => gitlab_pull(&node).map(Some),
                    Err(error) if missing(&error) => Ok(None),
                    Err(error) => Err(error.into()),
                }
            }
        }
    }

    /// The ref on its git remote that holds the head of PR/MR `number`.
    pub fn head_ref(&self, number: u64) -> String {
        format!(
            "{}{number}/head",
            forge::service::kind(self.forge).head_ref_prefix
        )
    }

    /// `branch` limits the list to one source branch in every state; `None` lists the open ones.
    fn github(&self, transport: &dyn Transport, branch: Option<&str>) -> Result<Vec<Pull>> {
        let (owner, name) = forge::split_repo(&self.repo);
        let mut pulls = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut vars = vec![Var::text("owner", owner), Var::text("name", name)];
            match branch {
                Some(branch) => {
                    vars.push(Var::text("branch", branch));
                    vars.push(Var::text("query", GITHUB_QUERY));
                }
                None => vars.push(Var::text("query", GITHUB_OPEN_QUERY)),
            }
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

    fn gitlab(&self, transport: &dyn Transport, branch: Option<&str>) -> Result<Vec<Pull>> {
        let project = forge::gitlab::project_path(&self.repo);
        let path = match branch {
            Some(branch) => format!(
                "{project}/merge_requests?source_branch={}&state=all&per_page=100",
                encode(branch)
            ),
            None => format!("{project}/merge_requests?state=opened&per_page=100"),
        };
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
        created_at: node.created_at,
        author: node.author,
        draft: node.draft,
    })
}
