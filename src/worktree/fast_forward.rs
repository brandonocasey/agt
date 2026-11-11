//! Brings the local default branch up to its upstream's default, which `create` has just fetched.
//! Only a fast-forward: `git merge --ff-only` in the worktree that has the branch checked out,
//! which keeps local changes and refuses to overwrite them, ignored files included, else a ref
//! update that git refuses for a branch being rebased or bisected. Whatever stops it is a warning,
//! not an error, because the new worktree already exists.

use crate::core::error::Result;
use crate::git::base::Candidate;
use crate::git::fast_forward::{Update, fast_forward};
use crate::git::{Repo, local_branch_name};
use serde_json::{Value, json};

/// The report on the local default branch and, when it may still be behind, the warning; `None`
/// when there is no local default branch or no remote to bring it up to.
pub(super) fn local_default(repo: &Repo, found: &[Candidate]) -> Option<(Value, Option<String>)> {
    let local = found.iter().find(|c| c.remote.is_none())?;
    if found.iter().all(|c| c.remote.is_none()) {
        return None;
    }
    let mut report = json!({"branch": local.branch, "from": local.commit});
    let reason = match update(repo, found, local, &mut report) {
        Ok(None) => return Some((report, None)),
        Ok(Some(reason)) => reason,
        Err(error) => error.to_string(),
    };
    report["status"] = json!("skipped");
    report["reason"] = json!(reason);
    let warning = format!("local {} not updated: {reason}", local.branch);
    Some((report, Some(warning)))
}

/// Fast-forwards `local` when it is behind its upstream and fills in `report`; the reason when it
/// stays put while it may be behind.
fn update(
    repo: &Repo,
    found: &[Candidate],
    local: &Candidate,
    report: &mut Value,
) -> Result<Option<String>> {
    let name = local.branch.as_str();
    let config = |key: &str| repo.git_opt(&["config", "--get", &format!("branch.{name}.{key}")]);
    // Without an upstream, origin's branch of the same name is the one `create` weighed it against.
    let remote = config("remote")?.unwrap_or_else(|| "origin".to_owned());
    let merge = config("merge")?.unwrap_or_else(|| format!("refs/heads/{name}"));
    let tracked = local_branch_name(&merge);
    let Some(upstream) = found
        .iter()
        .find(|c| c.remote.as_deref() == Some(remote.as_str()))
    else {
        return Ok(Some(format!(
            "its upstream remote {remote} is not one agt fetched"
        )));
    };
    if upstream.branch != tracked {
        return Ok(Some(format!(
            "it tracks {remote}/{tracked}, but {remote}'s default is {}",
            upstream.branch
        )));
    }
    let (from, to) = (local.commit.as_str(), upstream.commit.as_str());
    report["upstream"] = json!(format!("{remote}/{tracked}"));
    report["to"] = json!(to);
    advance_tracking(repo, &remote, tracked, to);
    if from == to {
        report["status"] = json!("current");
        return Ok(None);
    }
    if repo.is_ancestor(to, from)? {
        report["status"] = json!("ahead");
        report["ahead"] = json!(repo.distance(to, from)?);
        return Ok(None);
    }
    let behind = repo.distance(from, to)?;
    report["behind"] = json!(behind);
    if !repo.is_ancestor(from, to)? {
        let ahead = repo.distance(to, from)?;
        report["ahead"] = json!(ahead);
        return Ok(Some(format!(
            "it has diverged from {remote}/{tracked} ({ahead} ahead, {behind} behind)"
        )));
    }
    match fast_forward(repo, name, from, to)? {
        Update::Moved { path } => {
            if let Some(path) = path {
                report["path"] = json!(path);
            }
            report["status"] = json!("updated");
            Ok(None)
        }
        Update::Refused(refused) => {
            if let Some(path) = refused.path {
                report["path"] = json!(path);
            }
            if let Some(retry) = refused.retry {
                report["retry"] = json!(retry);
            }
            Ok(Some(refused.reason))
        }
    }
}

/// Moves the upstream's remote-tracking branch up to the fetched commit, as a fetch would, so
/// `git status` does not count the commits the local default just gained as unpushed. Best
/// effort: losing the ref lock to a parallel fetch only leaves that count stale, which must not
/// stop the local default from moving.
fn advance_tracking(repo: &Repo, remote: &str, tracked: &str, to: &str) {
    let tracking = format!("refs/remotes/{remote}/{tracked}");
    let Ok(Some(old)) = repo.ref_commit(&tracking) else {
        return;
    };
    if old != to && repo.is_ancestor(&old, to).unwrap_or(false) {
        let message = "agt worktree create: fetched default";
        let _ = repo.succeeds(&["update-ref", "-m", message, &tracking, to, &old]);
    }
}
