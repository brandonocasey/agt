//! `agt check run`: run a test or lint command unless it already passed on the same code.
//! The key is the git tree of a clean `HEAD`, the check name, the directory below the repository
//! root and the exact argv, so a commit that changes no content (an empty commit, a rebase that
//! moves nothing) hits. Only a pass is recorded; a dirty work tree always runs and records
//! nothing. Entries live in the main checkout's `.agt/checks/`, shared by linked worktrees.

pub mod prune;
mod run;

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::core::error::{Result, fail};
use crate::core::hash::sha256_hex;
use crate::core::output::Output;
use crate::git::Repo;
use crate::store::{Namespace, Store};

/// Run a command and cache its pass per git tree.
#[derive(Args)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a command unless it already passed on this exact git tree.
    ///
    /// With a clean work tree (untracked files that are not git-ignored count as dirty) the key is
    /// the `HEAD` tree, `--name`, the current directory below the repository root and the command.
    /// A recorded pass for the key prints JSON with `cached: true`, `key`, `ran_at` and `log`
    /// and exits 0 without running anything. Otherwise the command runs with inherited stdin; its
    /// output goes to stderr and to a log under the agt cache `output` directory, so stdout holds
    /// only the JSON summary, printed last. The exit status is the command's; a pass is recorded,
    /// a failure never. A dirty work tree always runs and records nothing.
    Run {
        /// Name of the check, part of the key (for example `test` or `lint`).
        #[arg(long)]
        name: String,
        /// Run even when a pass is recorded, and record the new pass.
        #[arg(long)]
        force: bool,
        /// The command and its arguments, after `--`.
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<OsString>,
    },
}

/// One recorded pass.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Entry {
    version: u32,
    pub(crate) key: String,
    name: String,
    tree: String,
    dir: String,
    command: Vec<String>,
    pub(crate) ran_at: String,
    duration_ms: u64,
    log: PathBuf,
}

pub fn run(cli: Cli) -> Result<Output> {
    let Command::Run {
        name,
        force,
        command,
    } = cli.command;
    if name.trim().is_empty() {
        return fail("--name must not be empty");
    }
    let argv = command
        .iter()
        .map(|arg| {
            arg.to_str()
                .map(str::to_owned)
                .ok_or_else(|| crate::core::error::Error("command arguments must be UTF-8".into()))
        })
        .collect::<Result<Vec<_>>>()?;
    let repo = Repo::discover()?;
    let clean = repo.strict_uncommitted_count()? == 0;
    let mut key = None;
    if clean {
        let tree = repo.tree_of("HEAD")?;
        let dir = relative_dir(&repo)?;
        let digest = sha256_hex(json!([tree, name, dir, argv]).to_string().as_bytes());
        key = Some((tree, dir, digest));
    }
    let mut warnings = Vec::new();
    let store = if key.is_some() {
        Store::open_repo(Namespace::Checks, &repo, false)
            .map_err(|error| warnings.push(format!("running uncached, no check store: {error}")))
            .ok()
    } else {
        None
    };
    let hit = match (&key, &store, force) {
        (Some((_, _, digest)), Some(store), false) => store
            .load_doc::<Entry>(store.root(), digest)
            .unwrap_or_else(|error| {
                warnings.push(format!("ignoring the cached pass: {error}"));
                None
            })
            .filter(|entry| entry.log.exists()),
        _ => None,
    };
    if let (Some((_, _, digest)), Some(entry)) = (&key, hit) {
        return Ok(Output::Json(json!({
            "cached": true,
            "name": name,
            "key": digest,
            "tree": entry.tree,
            "command": entry.command,
            "ran_at": entry.ran_at,
            "log": entry.log,
        })));
    }
    let outcome = run::execute(&name, &argv)?;
    let mut recorded = false;
    let mut reason = None;
    if let (Some((tree, dir, digest)), Some(store), 0) = (&key, &store, outcome.code) {
        if repo.strict_uncommitted_count()? == 0 && repo.tree_of("HEAD")? == *tree {
            let entry = Entry {
                version: 1,
                key: digest.clone(),
                name: name.clone(),
                tree: tree.clone(),
                dir: dir.clone(),
                command: argv.clone(),
                ran_at: outcome.ran_at.clone(),
                duration_ms: outcome.duration_ms,
                log: outcome.log.clone(),
            };
            let written = store.update_doc(
                store.root(),
                digest,
                || entry.clone(),
                |slot| {
                    *slot = entry.clone();
                    Ok(())
                },
            );
            match written {
                Ok(_) => recorded = true,
                Err(error) => warnings.push(format!("the pass was not recorded: {error}")),
            }
        } else {
            reason = Some("the work tree or HEAD changed during the run");
        }
    }
    let summary = json!({
        "cached": false,
        "name": name,
        "key": key.as_ref().map(|(_, _, digest)| digest),
        "dirty": !clean,
        "recorded": recorded,
        "reason": reason,
        "warnings": warnings,
        "command": argv,
        "exit": outcome.code,
        "ran_at": outcome.ran_at,
        "duration_ms": outcome.duration_ms,
        "log": outcome.log,
    });
    let code = u8::try_from(outcome.code.clamp(0, 255)).unwrap_or(1);
    Ok(Output::Json(summary).exiting(code))
}

/// The current directory below the repository root, empty at the root.
fn relative_dir(repo: &Repo) -> Result<String> {
    let top = std::fs::canonicalize(repo.toplevel()?).unwrap_or_default();
    let here = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .unwrap_or_default();
    Ok(here
        .strip_prefix(top)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default())
}
