//! Which commit a new branch starts from: the newest default branch among every remote's live
//! default and the local one.

use crate::core::error::{Error, Result, fail};
use crate::git::Repo;

/// The default branch of one remote (fetched just now) or of this repository.
pub struct Candidate {
    pub remote: Option<String>,
    pub branch: String,
    pub commit: String,
}

/// The names the default branch may have, best first: the branch `origin/HEAD` names, `main`,
/// `master`.
pub fn default_names(repo: &Repo) -> Result<Vec<String>> {
    let mut names = Vec::new();
    if let Some(remote) = repo.git_opt(&[
        "symbolic-ref",
        "--quiet",
        "--short",
        "refs/remotes/origin/HEAD",
    ])? {
        names.push(remote.strip_prefix("origin/").unwrap_or(&remote).to_owned());
    }
    names.extend(["main".to_owned(), "master".to_owned()]);
    Ok(names)
}

/// Every remote's live default branch, fetched, then the local default branch. `flag` names the
/// option the caller offers to name the base itself, for the error when none is found.
pub fn candidates(repo: &Repo, flag: &str) -> Result<Vec<Candidate>> {
    let mut found = Vec::new();
    for remote in repo.git(&["remote"])?.lines() {
        let named = |error: Error| Error(format!("remote {remote}: {error}"));
        let heads = repo
            .git(&["ls-remote", "--symref", remote, "HEAD"])
            .map_err(named)?;
        let branch = heads
            .lines()
            .find_map(|line| line.strip_prefix("ref: refs/heads/")?.split('\t').next())
            .ok_or_else(|| Error(format!("remote {remote}: no default branch advertised")))?;
        // An empty refmap keeps the fetch from also moving remote-tracking branches.
        repo.git(&[
            "fetch",
            "--no-tags",
            "--quiet",
            "--refmap=",
            remote,
            &format!("refs/heads/{branch}"),
        ])
        .map_err(named)?;
        found.push(Candidate {
            remote: Some(remote.to_owned()),
            branch: branch.to_owned(),
            commit: repo.git(&["rev-parse", "FETCH_HEAD"]).map_err(named)?,
        });
    }
    for name in default_names(repo)? {
        let spec = format!("refs/heads/{name}^{{commit}}");
        if let Some(commit) = repo.git_opt(&["rev-parse", "--verify", "--quiet", &spec])? {
            found.push(Candidate {
                remote: None,
                branch: name,
                commit,
            });
            break;
        }
    }
    if found.is_empty() {
        return fail(format!("no default branch found; pass {flag}"));
    }
    Ok(found)
}

/// The candidate that contains all the others, else origin's (or the first remote's) when they
/// diverge. The flag says they diverged.
pub fn choose(repo: &Repo, candidates: &[Candidate]) -> Result<(usize, bool)> {
    for (index, candidate) in candidates.iter().enumerate() {
        let mut newest = true;
        for other in candidates {
            newest &= repo.is_ancestor(&other.commit, &candidate.commit)?;
        }
        if newest {
            return Ok((index, false));
        }
    }
    let origin = candidates
        .iter()
        .position(|c| c.remote.as_deref() == Some("origin"))
        .or_else(|| candidates.iter().position(|c| c.remote.is_some()));
    Ok((origin.unwrap_or(0), true))
}
