//! `agt worktree trust`: the gate on a repository's `[worktree.hooks]`. A main checkout's
//! `.config/agt.toml` runs its hooks only while its canonical main checkout path is recorded with the SHA-256
//! of its exact bytes in `$XDG_STATE_HOME/agents/worktrees/trust.json`; any edit untrusts it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::core::hash::sha256_hex;
use clap::Args;
use serde_json::json;

use super::info::{dir_or_cwd, main_checkout, toplevel};
use super::port;
use crate::core::config::{Config, repo_file};
use crate::core::error::{Result, fail};
use crate::core::local::{read_json, update_json};
use crate::core::output::Output;
use crate::git::Repo;

#[derive(Args)]
pub struct Cli {
    /// Any checkout of the repository; default: the current directory.
    path: Option<PathBuf>,
    /// Forget the trust of the repository's config file.
    #[arg(long, conflicts_with = "list")]
    remove: bool,
    /// List every trusted file and whether it still matches.
    #[arg(long, conflicts_with = "path")]
    list: bool,
}

type Trusted = BTreeMap<String, String>;

pub fn run(cli: Cli) -> Result<Output> {
    if cli.list {
        return list();
    }
    let dir = dir_or_cwd(cli.path)?;
    let main = main_checkout(&Repo::at(&toplevel(&dir)?))?.path;
    let file = repo_file(&main);
    if cli.remove {
        let key = key(&main);
        let removed = update(|trusted| trusted.remove(&key).is_some())?;
        return Ok(Output::Json(json!({"file": key, "removed": removed})));
    }
    if let Some(reason) = symlink_reason(&main) {
        return fail(format!("refusing to trust: {reason}"));
    }
    let bytes = fs::read(&file).map_err(|_| {
        crate::core::error::Error(format!(
            "{} does not exist; nothing to trust",
            file.display()
        ))
    })?;
    if !has_hooks(&bytes) {
        return fail(format!("{} has no [worktree.hooks]", file.display()));
    }
    let hooks = Config::load_from(&file)?.worktree.hooks;
    let hash = sha256_hex(&bytes);
    let key = key(&main);
    update(|trusted| trusted.insert(key.clone(), hash.clone()).as_deref() != Some(&hash))?;
    Ok(Output::Json(json!({
        "file": key,
        "sha256": hash,
        "create": hooks.create,
        "remove": hooks.remove,
    })))
}

fn list() -> Result<Output> {
    let files: Vec<_> = load(&store()?)?
        .into_iter()
        .map(|(file, sha256)| {
            // Keys are `<main>/.config/agt.toml`; a symlink there is never trusted.
            let main = Path::new(&file).parent().and_then(Path::parent);
            if let Some(reason) = main.and_then(symlink_reason) {
                return json!({"file": file, "sha256": sha256, "current": false, "reason": reason});
            }
            let current = match fs::read(&file) {
                Ok(bytes) => json!(sha256_hex(&bytes) == sha256),
                Err(_) => json!("missing"),
            };
            json!({"file": file, "sha256": sha256, "current": current})
        })
        .collect();
    Ok(Output::Json(json!({"trusted": files})))
}

/// The trust key of a repository: its canonical main checkout joined with the literal
/// `.config/agt.toml`, so a symlinked config cannot borrow another repository's trust.
fn key(main: &Path) -> String {
    let root = main.canonicalize().unwrap_or_else(|_| main.to_path_buf());
    repo_file(&root).to_string_lossy().into_owned()
}

/// Why the repository config cannot be trusted: `.config` or `.config/agt.toml` is a symlink.
pub(super) fn symlink_reason(main: &Path) -> Option<String> {
    let file = repo_file(main);
    [file.parent()?, file.as_path()]
        .into_iter()
        .find(|path| fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()))
        .map(|path| format!("{} is a symlink", path.display()))
}

/// Whether the file text has a `[worktree.hooks]` table.
pub(super) fn has_hooks(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| table.get("worktree")?.get("hooks").cloned())
        .is_some()
}

/// Whether the repository `main` is trusted with exactly the config content `bytes`.
pub(super) fn is_trusted(main: &Path, bytes: &[u8]) -> Result<bool> {
    Ok(load(&store()?)?.get(&key(main)) == Some(&sha256_hex(bytes)))
}

fn store() -> Result<PathBuf> {
    Ok(port::folder()?.join("trust.json"))
}

/// Applies `change` under the lock and saves when it reports a change; returns that flag.
fn update(change: impl FnOnce(&mut Trusted) -> bool) -> Result<bool> {
    update_json(&store()?, |trusted| Ok(change(trusted)))
}

fn load(file: &Path) -> Result<Trusted> {
    Ok(read_json(file)?.unwrap_or_default())
}
