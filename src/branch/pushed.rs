//! The `pushed` check of `agt branch check`.

use serde_json::{Value, json};

use super::{NOT_PUSHED, tip};
use crate::core::config::{Config, Kind, RemoteSet};
use crate::core::error::{Result, fail};
use crate::forge;
use crate::forge::landed::Source;
use crate::forge::select::{Selected, select_tracker};
use crate::git::Repo;
use crate::store::model::ReviewState;

pub fn run(repo: &Repo, branch: &str, remote: Option<&str>) -> Result<(Value, u8)> {
    let (remote, selected) = choose(repo, remote)?;
    let remote = remote.as_str();
    let local = tip(repo, branch)?;
    let remote_commit = fetch(repo, remote, branch)?;
    let (state, ahead, behind) = match &remote_commit {
        None => ("missing", None, None),
        Some(commit) => {
            let ahead = repo.distance(commit, &local)?;
            let behind = repo.distance(&local, commit)?;
            let state = match (ahead, behind) {
                (0, 0) => "equal",
                (_, 0) => "ahead",
                (0, _) => "behind",
                _ => "diverged",
            };
            (state, Some(ahead), Some(behind))
        }
    };
    // The git comparison stands without the forge: a failed lookup leaves `pr` null.
    let pr = pull_report(selected.as_ref(), branch, &local).unwrap_or_else(|error| {
        eprintln!("warning: PR/MR lookup failed, `pr` is null: {error}");
        None
    });
    let head_ok = pr
        .as_ref()
        .is_none_or(|pr| pr["head_matches"] == Value::Bool(true));
    let value = json!({
        "branch": branch,
        "remote": remote,
        "commit": local,
        "remote_commit": remote_commit,
        "state": state,
        "ahead": ahead,
        "behind": behind,
        "uncommitted": uncommitted(repo, branch)?,
        "pr": pr,
    });
    let code = if state == "equal" && head_ok {
        0
    } else {
        NOT_PUSHED
    };
    Ok((value, code))
}

/// The git remote to compare with and the tracker whose PR/MR is reported. `--remote` names a
/// git remote or a `host/repo`; without it the tracker is chosen as `agt pr` does, and the git
/// remote is its own, else `origin`. A git remote no tracker serves has no PR/MR to report.
fn choose(repo: &Repo, remote: Option<&str>) -> Result<(String, Option<Selected>)> {
    let config = Config::load_for_repo(repo)?;
    if let Some(name) = remote
        && repo.has_remote(name)?
        && !config
            .trackers_from(
                Kind::PullRequests,
                &repo.remote_locations()?,
                RemoteSet::All,
            )
            .iter()
            .any(|tracker| tracker.remote.as_deref() == Some(name))
    {
        return Ok((name.to_owned(), None));
    }
    let selected = select_tracker(repo, &config, Kind::PullRequests, remote, None)?;
    let named = selected
        .as_ref()
        .and_then(|selected| selected.tracker.as_ref())
        .and_then(|tracker| tracker.remote.clone());
    match (named, remote) {
        (Some(named), _) => Ok((named, selected)),
        (None, Some(name)) => fail(format!("{name} is no git remote of this repository")),
        (None, None) => Ok(("origin".to_owned(), selected)),
    }
}

/// The pinned commit of `refs/heads/<branch>` on `remote`; `None` when the remote lacks it.
fn fetch(repo: &Repo, remote: &str, branch: &str) -> Result<Option<String>> {
    let heads = format!("refs/heads/{branch}");
    let present = repo
        .remote_has(remote, &heads)
        .or_else(|error| fail(format!("cannot read {remote}: {error}")))?;
    if !present {
        return Ok(None);
    }
    Ok(Some(repo.fetch_commit(remote, &heads)?))
}

/// Changed paths in the worktree that has `branch` checked out, else `None`.
fn uncommitted(repo: &Repo, branch: &str) -> Result<Option<usize>> {
    for worktree in repo.worktrees()? {
        // A worktree whose directory is gone has nothing to count.
        if worktree.branch.as_deref() == Some(branch)
            && !worktree.prunable
            && worktree.path.is_dir()
        {
            return Ok(Some(Repo::at(&worktree.path).uncommitted_count()?));
        }
    }
    Ok(None)
}

/// The open PR/MR of `branch` in this repository, if any, with its head and pipeline.
fn pull_report(selected: Option<&Selected>, branch: &str, local: &str) -> Result<Option<Value>> {
    // A host agt cannot identify has no PRs/MRs to report.
    let Some(origin) = selected.and_then(|selected| Source::of(selected).ok()) else {
        return Ok(None);
    };
    let Some(pull) = origin
        .pulls(branch)?
        .into_iter()
        .find(|pull| pull.state == Some(ReviewState::Open) && pull.same_repo)
    else {
        return Ok(None);
    };
    // The PR stands without its checks: a failed check query leaves only `pipeline.exists` null.
    let exists = forge::head_has_pipeline(
        origin.forge,
        &origin.host,
        &origin.repo,
        pull.number,
        &pull.head_sha,
    )
    .map_or_else(
        |error| {
            eprintln!("warning: pipeline lookup failed, `pipeline.exists` is null: {error}");
            Value::Null
        },
        Value::Bool,
    );
    Ok(Some(json!({
        "number": pull.number,
        "url": pull.url,
        "head": pull.head_sha,
        "head_matches": pull.head_sha == local,
        "pipeline": {"sha": pull.head_sha, "exists": exists},
    })))
}
