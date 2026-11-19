//! `agt worktree create`: a worktree for a branch under the main checkout's `.worktrees/`. A new
//! branch starts at the newest default base among every remote's live default branch and the
//! local one; the only existing branch that moves is the local default, fast-forwarded to its
//! upstream's fetched default when it can be. A branch that exists only as `origin/<branch>` gets
//! a local branch tracking it. `--detach` checks out a commit with no branch, as review
//! checkouts do.

use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::{Value, json};

use super::info::main_checkout;
use super::nested;
use super::setup::{self, Step};
use super::{emit, fast_forward, hooks};
use crate::core::error::{Result, ctx, fail};
use crate::core::output::Output;
use crate::core::shell::quote;
use crate::forge::auto::Offline;
use crate::git::base::{Candidate, candidates, choose};
use crate::git::{Checkout, Repo, Transports, Worktree};

#[derive(Args)]
pub struct Cli {
    /// The branch to check out; created when it does not exist (tracking `origin/<branch>` when
    /// that exists). With `--detach`, only the worktree's name.
    branch: String,
    /// Start a new branch here instead of at the newest default base.
    #[arg(long, value_name = "COMMIT-ISH")]
    base: Option<String>,
    /// Check out this commit detached, with no branch, in `.worktrees/<BRANCH>`.
    #[arg(long, value_name = "COMMIT-ISH", conflicts_with = "base")]
    detach: Option<String>,
    /// Where to put the worktree; default: `<main checkout>/.worktrees/<branch>`.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Install dependencies afterwards (`agt worktree prepare`).
    #[arg(long)]
    prepare: bool,
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
    if branch.starts_with('-') || !Repo::is_valid_ref_name(&full) {
        return fail(format!("{branch} is not a valid branch name"));
    }
    let detach = cli
        .detach
        .as_deref()
        .map(|rev| repo.commit(rev))
        .transpose()?;
    let exists = detach.is_none() && repo.ref_commit(&full)?.is_some();
    if exists && cli.base.is_some() {
        return fail(format!(
            "{branch} exists; --base applies to a new branch only"
        ));
    }
    let checked_out = match &detach {
        Some(commit) => reusable_detached(&repo, &target(&repo, &cli)?, commit)?,
        None => repo
            .worktrees()?
            .into_iter()
            .find(|entry| entry.branch.as_deref() == Some(branch)),
    };
    let named = detach.is_none().then_some(branch);
    let mut fetched = Vec::new();
    // Validated before the worktree exists: an invalid config must not leave one whose create
    // hooks never ran (a rerun would find it reused).
    let resolved = if checked_out.is_none() && !cli.no_hooks {
        Some(hooks::resolve(&main_checkout(&repo)?.path)?)
    } else {
        None
    };
    let has_hooks = resolved
        .as_ref()
        .is_some_and(|resolved| !resolved.is_empty(hooks::Kind::Create));
    let mut value = match checked_out {
        Some(entry) => {
            let commit = Repo::at(&entry.path).head()?;
            json!({"path": entry.path, "branch": named, "commit": commit, "reused": true})
        }
        None => {
            let dir = target(&repo, &cli)?;
            let mut value = json!({"reused": false});
            if let Some(commit) = &detach {
                repo.add_worktree(&dir, &Checkout::Detached(commit))?;
            } else if exists {
                repo.add_worktree(&dir, &Checkout::Branch(branch))?;
            } else if cli.base.is_none() && on_origin(&repo, branch)? {
                let upstream = format!("origin/{branch}");
                repo.add_worktree(
                    &dir,
                    &Checkout::Tracking {
                        name: branch,
                        upstream: &upstream,
                    },
                )?;
                value["tracking"] = json!(upstream);
            } else {
                let (base, found) = add_new(&repo, branch, &dir, cli.base.as_deref())?;
                value["base"] = base;
                fetched = found;
            }
            finish(&dir, named, value)?
        }
    };
    let mut warnings: Vec<String> = value["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|warning| warning.as_str().map(str::to_owned))
        .collect();
    if let Some((report, warning)) = fast_forward::local_default(&repo, &fetched) {
        value["local_default"] = report;
        warnings.extend(warning);
    }
    value["warnings"] = json!(warnings);
    let dir = PathBuf::from(value["path"].as_str().unwrap_or_default());
    let fresh = value["reused"] == false;
    let run_hooks = fresh && has_hooks;
    let mut steps = Vec::new();
    if cli.prepare {
        steps.push(Step::Prepare);
    }
    if run_hooks {
        steps.push(Step::Hooks);
    }
    if cli.background {
        // Reported now too: a caller that never waits must still see it.
        if run_hooks && let Some(untrusted) = resolved.and_then(|resolved| resolved.untrusted) {
            value["hooks"] = json!({"untrusted": untrusted});
        }
        value["background"] = if !steps.is_empty() {
            setup::spawn(&dir, &steps)?
        } else if let Some(report) = setup::running(&dir)? {
            report
        } else {
            json!({"started": false, "reason": "nothing to run"})
        };
        return emit(value, true);
    }
    if cli.prepare {
        setup::refuse_while_running(&dir)?;
    }
    let ok = setup::execute(&dir, &steps, &mut value)?;
    emit(value, ok)
}

/// Whether `origin` has `branch`, as `refs/remotes/origin/<branch>`. A branch pushed after the
/// last fetch is fetched on its own first; a ref origin does not have means a new branch, and
/// any failure to ask is an error rather than a silent new branch.
fn on_origin(repo: &Repo, branch: &str) -> Result<bool> {
    let tracking = format!("refs/remotes/origin/{branch}");
    let known = || repo.ref_commit(&tracking).map(|tip| tip.is_some());
    if known()? {
        return Ok(true);
    }
    if !repo.origin_url()?.is_some() {
        return Ok(false);
    }
    let heads = format!("refs/heads/{branch}");
    // Whether origin has the branch cannot be answered from a local copy that lacks it.
    Offline::current().forbid(&format!(
        "asking origin whether {branch} exists; pass --base to start a new branch"
    ))?;
    let present = repo.remote_has("origin", &heads).or_else(|error| {
        fail(format!(
            "cannot tell whether {branch} exists on origin: {error}; pass --base to start a new branch anyway"
        ))
    })?;
    if !present {
        return Ok(false);
    }
    let spec = format!("+{heads}:{tracking}");
    repo.fetch("origin", &[&spec])?;
    known()
}

/// The worktree at `dir` when it is detached at `commit`. One on a branch or at another commit is
/// refused: moving it could discard work, and a branch must not be reported as detached.
fn reusable_detached(repo: &Repo, dir: &Path, commit: &str) -> Result<Option<Worktree>> {
    // `--path` may hold `..` or a trailing slash; git lists canonical paths.
    let real = std::fs::canonicalize(dir).ok();
    let Some(entry) = repo.worktrees()?.into_iter().find(|entry| {
        entry.path == dir || real.is_some() && std::fs::canonicalize(&entry.path).ok() == real
    }) else {
        return Ok(None);
    };
    let shown = dir.display();
    if entry.prunable {
        return fail(format!(
            "{shown} is a registered worktree whose directory is gone; run `git worktree prune` first"
        ));
    }
    if let Some(branch) = &entry.branch {
        return fail(format!(
            "{shown} has {branch} checked out; pick another name for --detach"
        ));
    }
    if entry.head != commit {
        return fail(format!(
            "{shown} is detached at {}, not {commit}; when it is clean, run `git -C {} switch --detach {commit}`, or pick another name",
            entry.head,
            quote(&dir.display().to_string())
        ));
    }
    Ok(Some(entry))
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
    dir: &Path,
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
    repo.add_worktree(
        dir,
        &Checkout::NewBranch {
            name: branch,
            at: &commit,
        },
    )?;
    info["commit"] = json!(commit);
    Ok((info, found))
}

/// Initializes submodules and describes the new worktree; `branch` is `None` when detached.
fn finish(dir: &Path, branch: Option<&str>, mut value: Value) -> Result<Value> {
    let inside = Repo::at(dir);
    let submodules = dir.join(".gitmodules").exists();
    let mut warnings = Vec::new();
    if submodules && Offline::current().is_set() {
        // Git takes a submodule's URL from the shared config when the main checkout has
        // registered it, else from `.gitmodules`; offline only `file` URLs can be cloned, so
        // the others are skipped with a warning and the worktree and its hooks carry on.
        // An empty or unreadable `.gitmodules` means no submodules rather than a failure that
        // would stop `finish` and the create hooks.
        let listed = nested::submodule_paths(dir).unwrap_or_default();
        for path in &listed {
            if let Err(error) = inside.update_submodules(Some(path), Transports::FileOnly) {
                warnings.push(format!(
                    "submodule {path} not initialized offline; run `git submodule update --init` online: {}",
                    error.0.lines().last().unwrap_or_default()
                ));
            }
        }
    } else if submodules {
        inside.update_submodules(None, Transports::Any)?;
    }
    value["warnings"] = json!(warnings);
    value["path"] = json!(dir);
    value["branch"] = json!(branch);
    value["commit"] = json!(inside.head()?);
    value["submodules"] = json!(submodules);
    Ok(value)
}
