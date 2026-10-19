//! `agt worktree create`: a worktree for a branch under the main checkout's `.worktrees/`. A new
//! branch starts at the newest default base among every remote's live default branch and the
//! local one; the only existing branch that moves is the local default, fast-forwarded to its
//! upstream's fetched default when it can be. A branch that exists only as `origin/<branch>` gets
//! a local branch tracking it.

use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::{Value, json};

use super::info::{self, main_checkout};
use super::setup::{self, Step};
use super::{emit, fast_forward, hooks, port, prepare};
use crate::core::error::{Result, ctx, fail};
use crate::core::output::Output;
use crate::git::Repo;
use crate::git::base::{Candidate, candidates, choose};

#[derive(Args)]
pub struct Cli {
    /// The branch to check out; created when it does not exist (tracking `origin/<branch>` when
    /// that exists).
    branch: String,
    /// Start a new branch here instead of at the newest default base.
    #[arg(long, value_name = "COMMIT-ISH")]
    base: Option<String>,
    /// Where to put the worktree; default: `<main checkout>/.worktrees/<branch>`.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Install dependencies afterwards (`agt worktree prepare`).
    #[arg(long)]
    prepare: bool,
    /// Record a dev-server port afterwards (`agt worktree port`).
    #[arg(long)]
    port: bool,
    /// Skip the `[worktree.hooks] create` commands.
    #[arg(long)]
    no_hooks: bool,
    /// Run the install (with `--prepare`) and the create hooks in a detached process and return
    /// at once; `agt worktree prepare --wait` collects the result.
    #[arg(long)]
    background: bool,
}

pub fn run(cli: Cli) -> Result<Output> {
    let branch = cli.branch.as_str();
    let current = std::env::current_dir().map_err(ctx("read the current directory"))?;
    let repo = Repo::at(&current);
    let full = format!("refs/heads/{branch}");
    if branch.starts_with('-') || !repo.succeeds(&["check-ref-format", &full])? {
        return fail(format!("{branch} is not a valid branch name"));
    }
    let exists = repo.succeeds(&["show-ref", "--verify", "--quiet", &full])?;
    if exists && cli.base.is_some() {
        return fail(format!(
            "{branch} exists; --base applies to a new branch only"
        ));
    }
    let checked_out = info::entries(&repo)?
        .into_iter()
        .find(|entry| entry.branch.as_deref() == Some(branch));
    let mut fetched = Vec::new();
    let mut value = match checked_out {
        Some(entry) => {
            let commit = Repo::at(&entry.path).git(&["rev-parse", "HEAD"])?;
            json!({"path": entry.path, "branch": branch, "commit": commit, "reused": true})
        }
        None => {
            let dir = target(&repo, &cli)?;
            let dir_arg = dir.to_string_lossy();
            let mut value = json!({"reused": false});
            if exists {
                repo.git(&["worktree", "add", &dir_arg, branch])?;
            } else if cli.base.is_none() && on_origin(&repo, branch)? {
                let upstream = format!("origin/{branch}");
                repo.git(&[
                    "worktree", "add", "--track", "-b", branch, &dir_arg, &upstream,
                ])?;
                value["tracking"] = json!(upstream);
            } else {
                let (base, found) = add_new(&repo, branch, &dir_arg, cli.base.as_deref())?;
                value["base"] = base;
                fetched = found;
            }
            finish(&dir, branch, value)?
        }
    };
    let mut warnings = Vec::new();
    if let Some((report, warning)) = fast_forward::local_default(&repo, &fetched) {
        value["local_default"] = report;
        warnings.extend(warning);
    }
    value["warnings"] = json!(warnings);
    let dir = PathBuf::from(value["path"].as_str().unwrap_or_default());
    let mut ok = true;
    let fresh = value["reused"] == false;
    let run_hooks = fresh && !cli.no_hooks && {
        let main = main_checkout(&repo)?.path;
        !hooks::configured(&main, hooks::Kind::Create)?.is_empty()
    };
    if cli.background {
        // The port is recorded first so the hooks see it as PORT.
        if cli.port {
            let (path, number) = port::assign(&dir)?;
            value["port"] = port::report(&path, number);
        }
        let mut steps = Vec::new();
        if cli.prepare {
            steps.push(Step::Prepare);
        }
        if run_hooks {
            steps.push(Step::Hooks);
        }
        if steps.is_empty() {
            value["background"] = json!({"started": false, "reason": "nothing to run"});
        } else {
            value["background"] = setup::spawn(&dir, &steps)?;
        }
        return emit(value, ok);
    }
    if cli.prepare {
        let (report, passed) = prepare::prepare(&dir, false)?;
        value["prepare"] = report;
        ok = passed;
    }
    if cli.port {
        let (path, number) = port::assign(&dir)?;
        value["port"] = port::report(&path, number);
    }
    if run_hooks {
        let (report, passed) = hooks::run(&dir, hooks::Kind::Create)?;
        value["hooks"] = report;
        ok &= passed;
    }
    emit(value, ok)
}

/// Whether `origin` has `branch`, as `refs/remotes/origin/<branch>`. A branch pushed after the
/// last fetch is fetched on its own first; a ref origin does not have means a new branch, and
/// any failure to ask is an error rather than a silent new branch.
fn on_origin(repo: &Repo, branch: &str) -> Result<bool> {
    let tracking = format!("refs/remotes/origin/{branch}");
    let known = || repo.succeeds(&["show-ref", "--verify", "--quiet", &tracking]);
    if known()? {
        return Ok(true);
    }
    if !repo.succeeds(&["remote", "get-url", "origin"])? {
        return Ok(false);
    }
    // `ls-remote --exit-code` exits 2 when the ref is absent and otherwise non-zero only on
    // failure, whatever the language of git's messages.
    let heads = format!("refs/heads/{branch}");
    match repo.probe(&["ls-remote", "--exit-code", "origin", &heads])? {
        (Some(0), _) => {}
        (Some(2), _) => return Ok(false),
        (_, error) => {
            return fail(format!(
                "cannot tell whether {branch} exists on origin: {error}; pass --base to start a new branch anyway"
            ));
        }
    }
    let spec = format!("+{heads}:{tracking}");
    repo.git(&["fetch", "--quiet", "--no-tags", "origin", &spec])?;
    known()
}

fn target(repo: &Repo, cli: &Cli) -> Result<PathBuf> {
    if let Some(path) = &cli.path {
        return std::path::absolute(path).map_err(ctx("resolve --path"));
    }
    let main = main_checkout(repo)?;
    if main.bare {
        return fail("the main checkout is bare; pass --path for the worktree");
    }
    Ok(main.path.join(".worktrees").join(&cli.branch))
}

/// Adds the worktree on a new branch and returns what it started from, with the default-branch
/// candidates it weighed (none for an explicit base).
fn add_new(
    repo: &Repo,
    branch: &str,
    dir: &str,
    base: Option<&str>,
) -> Result<(Value, Vec<Candidate>)> {
    let (commit, mut info, found) = match base {
        Some(base) => (repo.commit(base)?, json!({"ref": base}), Vec::new()),
        None => {
            let found = candidates(repo, "--base")?;
            let (chosen, diverged) = choose(repo, &found)?;
            let listed: Vec<Value> = found
                .iter()
                .map(|c| json!({"remote": c.remote, "branch": c.branch, "commit": c.commit}))
                .collect();
            (
                found[chosen].commit.clone(),
                json!({"candidates": listed, "diverged": diverged}),
                found,
            )
        }
    };
    repo.git(&["worktree", "add", "--no-track", "-b", branch, dir, &commit])?;
    info["commit"] = json!(commit);
    Ok((info, found))
}

/// Initializes submodules and describes the new worktree.
fn finish(dir: &Path, branch: &str, mut value: Value) -> Result<Value> {
    let inside = Repo::at(dir);
    let submodules = dir.join(".gitmodules").exists();
    if submodules {
        inside.git(&["submodule", "update", "--init", "--recursive"])?;
    }
    value["path"] = json!(dir);
    value["branch"] = json!(branch);
    value["commit"] = json!(inside.git(&["rev-parse", "HEAD"])?);
    value["submodules"] = json!(submodules);
    Ok(value)
}
