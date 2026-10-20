//! `agt branch pushed`.

use serde_json::{Value, json};

use super::{NOT_PUSHED, tip};
use crate::core::error::{Result, fail};
use crate::forge;
use crate::forge::landed::Origin;
use crate::git::Repo;
use crate::store::model::ReviewState;

pub fn run(repo: &Repo, branch: &str, remote: &str) -> Result<(Value, u8)> {
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
    let pr = pull_report(repo, branch, &local).unwrap_or_else(|error| {
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

/// The pinned commit of `refs/heads/<branch>` on `remote`; `None` when the remote lacks it.
fn fetch(repo: &Repo, remote: &str, branch: &str) -> Result<Option<String>> {
    let heads = format!("refs/heads/{branch}");
    // `ls-remote --exit-code` exits 2 when the ref is absent, whatever the language of git.
    match repo.probe(&[
        "ls-remote",
        "--exit-code",
        "--end-of-options",
        remote,
        &heads,
    ])? {
        (Some(0), _) => {}
        (Some(2), _) => return Ok(None),
        (_, error) => return fail(format!("cannot read {remote}: {error}")),
    }
    // An empty refmap keeps the fetch from moving remote-tracking branches; FETCH_HEAD is read
    // at once so a later fetch cannot change it.
    repo.git(&["fetch", "--no-tags", "--quiet", "--refmap=", remote, &heads])?;
    Ok(Some(repo.git(&["rev-parse", "FETCH_HEAD"])?))
}

/// Changed paths in the worktree that has `branch` checked out, else `None`.
fn uncommitted(repo: &Repo, branch: &str) -> Result<Option<usize>> {
    for worktree in repo.worktrees()? {
        // A worktree whose directory is gone has nothing to count.
        if worktree.branch.as_deref() == Some(branch)
            && !worktree.prunable
            && worktree.path.is_dir()
        {
            let status = Repo::at(&worktree.path).git(&["status", "--porcelain"])?;
            return Ok(Some(status.lines().count()));
        }
    }
    Ok(None)
}

/// The open PR/MR of `branch` in this repository, if any, with its head and pipeline.
fn pull_report(repo: &Repo, branch: &str, local: &str) -> Result<Option<Value>> {
    let Some(origin) = Origin::of(repo)? else {
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
