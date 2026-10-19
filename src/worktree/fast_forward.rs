//! Brings the local default branch up to its upstream's default, which `create` has just fetched.
//! Only a fast-forward: `git merge --ff-only` in the worktree that has the branch checked out,
//! which keeps local changes and refuses to overwrite them, ignored files included, else a ref
//! update that git refuses for a branch being rebased or bisected. Whatever stops it is a warning,
//! not an error, because the new worktree already exists.

use serde_json::{Value, json};

use super::create::Candidate;
use super::info;
use crate::core::error::Result;
use crate::core::shell::quote;
use crate::git::Repo;

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
    let tracked = merge.strip_prefix("refs/heads/").unwrap_or(&merge);
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
    advance_tracking(repo, &remote, tracked, to)?;
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
    let holder = info::entries(repo)?
        .into_iter()
        .find(|entry| entry.branch.as_deref() == Some(name));
    let Some(holder) = holder else {
        let spec = format!("{to}:refs/heads/{name}");
        let fetch = [
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            ".",
            &spec,
        ];
        return run(repo, &fetch, report);
    };
    report["path"] = json!(holder.path);
    // A merge leaves a moved submodule checked out at its old commit, so the checkout turns dirty.
    let raw = repo.git(&["diff", "--raw", "--ignore-submodules=none", from, to])?;
    let gitlink = raw.lines().any(|line| {
        line.split_whitespace()
            .take(2)
            .any(|mode| mode.trim_start_matches(':') == "160000")
    });
    if gitlink {
        return Ok(Some(format!(
            "a submodule moves, and a merge in {} would leave it at its old commit",
            holder.path.display()
        )));
    }
    run(
        &Repo::at(&holder.path),
        &[
            "merge",
            "--ff-only",
            "--no-overwrite-ignore",
            "--no-autostash",
            "--quiet",
            to,
        ],
        report,
    )
}

/// Moves the upstream's remote-tracking branch up to the fetched commit, as a fetch would, so
/// `git status` does not count the commits the local default just gained as unpushed.
fn advance_tracking(repo: &Repo, remote: &str, tracked: &str, to: &str) -> Result<()> {
    let tracking = format!("refs/remotes/{remote}/{tracked}");
    let spec = format!("{tracking}^{{commit}}");
    let Some(old) = repo.git_opt(&["rev-parse", "--verify", "--quiet", &spec])? else {
        return Ok(());
    };
    if old != to && repo.is_ancestor(&old, to)? {
        let message = "agt worktree create: fetched default";
        repo.git(&["update-ref", "-m", message, &tracking, to, &old])?;
    }
    Ok(())
}

/// Runs the update; when git refuses, as it does over local changes a merge would overwrite, the
/// reason is git's message and `retry` the command to run once that is resolved.
fn run(repo: &Repo, command: &[&str], report: &mut Value) -> Result<Option<String>> {
    let (code, error) = repo.probe(command)?;
    if code == Some(0) {
        report["status"] = json!("updated");
        return Ok(None);
    }
    let words: Vec<String> = command.iter().map(|word| quote(word)).collect();
    report["retry"] = json!(format!(
        "git -C {} {}",
        quote(&repo.root().display().to_string()),
        words.join(" ")
    ));
    let message: Vec<&str> = error
        .lines()
        .map(|line| {
            let line = line.trim();
            ["error: ", "fatal: "]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
                .unwrap_or(line)
        })
        .filter(|line| !line.is_empty())
        .collect();
    Ok(Some(format!(
        "git {} refused: {}",
        command[0],
        message.join(" ")
    )))
}
