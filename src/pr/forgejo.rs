//! Forgejo and Gitea pull requests through the REST API. The host does not say whether
//! auto-merge is on, so a PR reports it as unknown.

use serde_json::{Value, json};

use super::provider::{At, Capabilities, Filter, NewPr, Provider, open_first};
use super::remote::{self, Remote};
use super::upload::Uploader;
use super::{Changes, How, Native, Pr, State};
use crate::core::error::{Error, Result, fail};
use crate::forge::forgejo::{self, pull_node};
use crate::forge::issue::names;
use crate::forge::service::{Method, Request};
use crate::forge::transport::Transport;
use crate::store::model::ReviewState;

/// The title prefix that marks a draft.
const DRAFT_PREFIX: &str = "WIP: ";

/// The most PRs `list` returns.
const LIST_LIMIT: usize = 100;

fn base(repo: &str) -> String {
    format!("repos/{repo}/pulls")
}

/// The title without a draft marker.
fn plain(title: &str) -> &str {
    let lower = title.to_lowercase();
    for marker in ["wip: ", "wip:", "[wip] ", "[wip]"] {
        if lower.starts_with(marker) {
            return title[marker.len()..].trim_start();
        }
    }
    title
}

fn pr(node: &Value) -> Pr {
    let shared = pull_node(node);
    let draft = shared.draft || plain(&shared.title) != shared.title;
    let state = shared.state.unwrap_or(ReviewState::Open);
    let (mergeable, merge_state) = match (state, node["mergeable"].as_bool()) {
        (ReviewState::Open, Some(false)) => ("conflicting", "conflict"),
        (ReviewState::Open, Some(true)) if draft => ("blocked", "draft"),
        (ReviewState::Open, Some(true)) => ("mergeable", "mergeable"),
        (ReviewState::Open, None) => ("unknown", "unknown"),
        _ => ("unknown", ""),
    };
    Pr {
        id: node["id"].to_string(),
        number: Some(shared.number.unwrap_or_default()),
        url: Some(shared.url),
        title: shared.title,
        body: node["body"].as_str().unwrap_or_default().to_owned(),
        state,
        draft,
        head_branch: shared.head_branch,
        base_branch: shared.base,
        head_sha: shared.head_sha,
        author: shared.author.unwrap_or_default(),
        labels: names(node, "/labels", "name"),
        auto_merge: None,
        auto_merge_known: false,
        queue: None,
        mergeable: mergeable.to_owned(),
        merge_state: merge_state.to_owned(),
        conflicts: mergeable == "conflicting",
        same_repo: shared.same_repo.unwrap_or(true),
        native: Native::Forgejo,
    }
}

pub fn view(t: &dyn Transport, repo: &str, number: u64) -> Result<Pr> {
    Ok(pr(
        &t.json(&Request::get(format!("{}/{number}", base(repo))))?
    ))
}

/// What `list` keeps.
pub struct Query<'a> {
    pub branch: Option<&'a str>,
    pub state: State,
    /// Only the PRs this login opened.
    pub author: Option<&'a str>,
}

/// Pull requests newest first; the host filters by state only, so `branch` and `mine` are
/// applied to what it lists.
pub fn list(t: &dyn Transport, repo: &str, query: &Query) -> Result<Vec<Pr>> {
    let Query {
        branch,
        state,
        author,
    } = *query;
    let listed = match state {
        State::Open => "open",
        State::Merged => "closed",
        State::All => "all",
    };
    let mut found: Vec<Pr> = forgejo::pulls(t, repo, listed)?
        .iter()
        .map(pr)
        .filter(|found| branch.is_none_or(|branch| found.head_branch == branch))
        .filter(|found| author.is_none_or(|author| found.author == author))
        .filter(|found| !matches!(state, State::Merged) || found.state == ReviewState::Merged)
        .collect();
    found.truncate(LIST_LIMIT);
    Ok(found)
}

/// The PR of `branch` that is in `repo` itself, not a fork: the open ones, else those of every
/// state, newest first.
pub fn of_branch(t: &dyn Transport, repo: &str, branch: &str) -> Result<Vec<Pr>> {
    let in_repo = |node: &Value| pull_node(node).same_repo == Some(true);
    let found = forgejo::pulls_of_branch(t, repo, branch, in_repo)?;
    Ok(found.iter().map(pr).collect())
}

pub fn create(t: &dyn Transport, repo: &str, new: &NewPr) -> Result<u64> {
    let title = if new.draft {
        format!("{DRAFT_PREFIX}{}", new.title)
    } else {
        new.title.to_owned()
    };
    let body = json!({"head": new.head, "base": new.base, "title": title, "body": new.body});
    let created = t.json(&Request::rest(Method::Post, base(repo), Some(body)))?;
    created["number"]
        .as_u64()
        .ok_or_else(|| Error("Forgejo did not return the new pull request's number".into()))
}

pub fn update(t: &dyn Transport, repo: &str, current: &Pr, changes: &Changes) -> Result<()> {
    let mut body = serde_json::Map::new();
    let title = changes.title.as_deref().unwrap_or(&current.title);
    match changes.draft {
        Some(false) => body.insert("title".into(), json!(plain(title))),
        Some(true) => body.insert(
            "title".into(),
            json!(format!("{DRAFT_PREFIX}{}", plain(title))),
        ),
        None => changes
            .title
            .as_ref()
            .and_then(|title| body.insert("title".into(), json!(title))),
    };
    if let Some(text) = &changes.body {
        body.insert("body".into(), json!(text));
    }
    if !body.is_empty() {
        t.send(&Request::rest(
            Method::Patch,
            format!("{}/{}", base(repo), current.forge_number()),
            Some(Value::Object(body)),
        ))?;
    }
    let Some(labels) = changes.merged_labels(&current.labels) else {
        return Ok(());
    };
    t.send(&Request::rest(
        Method::Put,
        format!("repos/{repo}/issues/{}/labels", current.forge_number()),
        Some(json!({ "labels": labels })),
    ))?;
    Ok(())
}

/// How the host names a merge method; the same words `agt pr` takes.
pub fn method(how: Option<How>) -> &'static str {
    match how {
        Some(How::Squash) => "squash",
        Some(How::Rebase) => "rebase",
        Some(How::Merge) | None => "merge",
    }
}

/// The PR to merge and the head it must still have.
pub struct Merge<'a> {
    pub number: u64,
    pub sha: &'a str,
    pub how: Option<How>,
}

/// Merges now; the host refuses when the head is no longer `sha`.
pub fn merge(t: &dyn Transport, repo: &str, request: &Merge) -> Result<()> {
    post_merge(t, repo, request, false)
}

/// Schedules a merge for when the checks succeed. The host answers before it compares the head
/// with `sha`, so the only head guard is the caller's own check before this call.
pub fn merge_when_checks_succeed(t: &dyn Transport, repo: &str, request: &Merge) -> Result<()> {
    post_merge(t, repo, request, true)
}

fn post_merge(t: &dyn Transport, repo: &str, request: &Merge, auto: bool) -> Result<()> {
    let Merge { number, sha, how } = *request;
    let mut body = json!({
        "Do": method(how),
        "head_commit_id": sha,
        "delete_branch_after_merge": false,
    });
    if auto {
        body["merge_when_checks_succeed"] = json!(true);
    }
    t.send(&Request::rest(
        Method::Post,
        format!("{}/{number}/merge", base(repo)),
        Some(body),
    ))
    .map(|_| ())
    .map_err(Error::from)
}

/// Cancels a scheduled merge; false when none was scheduled (the host answers 404).
pub fn disable_auto(t: &dyn Transport, repo: &str, number: u64) -> Result<bool> {
    match t.send(&Request::rest(
        Method::Delete,
        format!("{}/{number}/merge", base(repo)),
        None,
    )) {
        Ok(_) => Ok(true),
        Err(error) if error.status == Some(404) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub struct Forgejo(pub Remote);

impl Provider for Forgejo {
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
        view(t, repo, remote::number(at))
    }

    fn list(&self, filter: &Filter) -> Result<Vec<Pr>> {
        let (t, repo) = self.0.api();
        let login = if filter.mine {
            Some(self.0.service.login()?)
        } else {
            None
        };
        list(
            t,
            repo,
            &Query {
                branch: filter.branch,
                state: filter.state,
                author: login.as_deref(),
            },
        )
    }

    /// Forgejo is not asked to filter by head branch and lists the whole repository, so it is
    /// asked for the open ones first.
    fn of_branch(&self, branch: &str) -> Result<Option<Pr>> {
        let (t, repo) = self.0.api();
        Ok(open_first(of_branch(t, repo, branch)?))
    }

    fn create(&self, new: &NewPr) -> Result<At> {
        let (t, repo) = self.0.api();
        Ok(At::Number(create(t, repo, new)?))
    }

    fn update(&self, current: &Pr, changes: &Changes) -> Result<()> {
        let (t, repo) = self.0.api();
        update(t, repo, current, changes)
    }

    fn uploader(&self) -> Result<Uploader<'_>> {
        fail(
            "Forgejo can attach an image only to an existing issue or comment, \
             not to a PR body being written; upload it in the web interface",
        )
    }

    fn merge(&self, pr: &Pr, sha: &str, how: Option<How>) -> Result<()> {
        let (t, repo) = self.0.api();
        let request = Merge {
            number: pr.forge_number(),
            sha,
            how,
        };
        merge(t, repo, &request)
    }

    fn enable_auto(&self, pr: &Pr, sha: &str, how: Option<How>) -> Result<()> {
        let (t, repo) = self.0.api();
        let request = Merge {
            number: pr.forge_number(),
            sha,
            how,
        };
        merge_when_checks_succeed(t, repo, &request)?;
        // The host cannot be asked how auto-merge was set, so pausing it needs this.
        self.record(&pr.at(), method(how))
    }

    fn disable_auto(&self, pr: &Pr) -> Result<bool> {
        let (t, repo) = self.0.api();
        disable_auto(t, repo, pr.forge_number())
    }

    /// The host does not say whether auto-merge is on, so cancelling it is the probe; the method
    /// is the one `agt pr merge --auto` recorded, else a plain merge.
    fn auto_method(&self, _pr: &Pr, recorded: Option<&str>) -> Option<String> {
        Some(recorded.unwrap_or_else(|| method(None)).to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pull_request_reports_auto_merge_as_unknown() {
        let found = pr(&json!({
            "id": 41, "number": 7, "html_url": "https://h/o/r/pulls/7", "title": "WIP: add x",
            "body": "b", "state": "open", "merged": false, "mergeable": true,
            "labels": [{"name": "bug"}], "user": {"login": "me"},
            "base": {"ref": "main", "repo": {"full_name": "o/r"}},
            "head": {"ref": "topic", "sha": "abc", "repo": {"full_name": "o/r"}}
        }));
        assert!(!found.auto_merge_known);
        assert_eq!(found.auto_merge, None);
        assert!(found.draft);
        assert_eq!(
            (found.mergeable.as_str(), found.merge_state.as_str()),
            ("blocked", "draft")
        );
        assert_eq!(found.labels, ["bug"]);
        assert_eq!(found.id, "41");
        assert!(found.same_repo);
        let conflicting = pr(&json!({"state": "open", "mergeable": false}));
        assert!(conflicting.conflicts);
        let merged = pr(&json!({"state": "closed", "merged": true, "mergeable": false}));
        assert_eq!(
            (merged.state, merged.mergeable.as_str()),
            (ReviewState::Merged, "unknown")
        );
    }

    #[test]
    fn a_draft_marker_is_taken_off_and_put_on() {
        assert_eq!(plain("WIP: fix"), "fix");
        assert_eq!(plain("[wip] fix"), "fix");
        assert_eq!(plain("fix"), "fix");
    }

    #[test]
    fn merge_methods_use_the_hosts_words() {
        assert_eq!(method(None), "merge");
        assert_eq!(method(Some(How::Squash)), "squash");
        assert_eq!(method(Some(How::Rebase)), "rebase");
    }
}
