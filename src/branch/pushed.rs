//! `agt branch pushed`.

use serde_json::{Value, json};

use super::{NOT_PUSHED, tip};
use crate::core::error::{Result, fail};
use crate::forge::github::Github;
use crate::forge::gitlab::{Gitlab, ci::base_of_path};
use crate::forge::landed::{Origin, Pull, State};
use crate::forge::transport::CliTransport;
use crate::git::Repo;
use crate::store::model::Forge;

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
    let listing = repo.git(&["worktree", "list", "--porcelain"])?;
    let wanted = format!("branch refs/heads/{branch}");
    for block in listing.split("\n\n") {
        let lines: Vec<&str> = block.lines().collect();
        let Some(path) = lines.iter().find_map(|line| line.strip_prefix("worktree ")) else {
            continue;
        };
        // A worktree whose directory is gone has nothing to count.
        let gone = lines.iter().any(|line| line.starts_with("prunable"))
            || !std::path::Path::new(path).is_dir();
        if lines.contains(&wanted.as_str()) && !gone {
            let status = Repo::at(path).git(&["status", "--porcelain"])?;
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
        .find(|pull| pull.state == State::Open && pull.same_repo)
    else {
        return Ok(None);
    };
    Ok(Some(json!({
        "number": pull.number,
        "url": pull.url,
        "head": pull.head_sha,
        "head_matches": pull.head_sha == local,
        "pipeline": {"sha": pull.head_sha, "exists": pipeline_exists(&origin, &pull)?},
    })))
}

/// Whether a check run, status or pipeline exists for the head commit of `pull`.
fn pipeline_exists(origin: &Origin, pull: &Pull) -> Result<bool> {
    match origin.forge {
        Forge::Github => {
            let github = Github::new(Box::new(CliTransport::gh(&origin.host)));
            Ok(github.pull_checks(&origin.repo, pull.number)?.rollup > 0)
        }
        Forge::Gitlab => {
            let gitlab = Gitlab::new(Box::new(CliTransport::glab(&origin.host)));
            let base = base_of_path(&origin.repo);
            let request = gitlab.merge_request(&base, pull.number)?;
            let pipeline = &request["head_pipeline"];
            if pipeline.is_null() {
                return Ok(false);
            }
            Ok(gitlab.pipeline_runs(&base, pipeline, &pull.head_sha)?)
        }
    }
}
