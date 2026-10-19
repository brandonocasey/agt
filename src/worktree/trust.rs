//! `agt worktree trust`: the gate on a repository's `[worktree.hooks]`. A main checkout's
//! `.config/agt.toml` runs its hooks only while its canonical path is recorded with the SHA-256
//! of its exact bytes in `$XDG_STATE_HOME/agents/worktrees/trust.json`; any edit untrusts it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::info::{main_checkout, toplevel};
use super::port;
use crate::core::config::Config;
use crate::core::error::{Result, ctx, fail};
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
    let dir = match cli.path {
        Some(path) => path,
        None => std::env::current_dir().map_err(ctx("read the current directory"))?,
    };
    let main = main_checkout(&Repo::at(&toplevel(&dir)?))?.path;
    let file = config_file(&main);
    if cli.remove {
        let key = key(&file);
        let removed = update(|trusted| trusted.remove(&key).is_some())?;
        return Ok(Output::Json(json!({"file": key, "removed": removed})));
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
    let hash = digest(&bytes);
    let key = key(&file);
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
            let current = match fs::read(&file) {
                Ok(bytes) => json!(digest(&bytes) == sha256),
                Err(_) => json!("missing"),
            };
            json!({"file": file, "sha256": sha256, "current": current})
        })
        .collect();
    Ok(Output::Json(json!({"trusted": files})))
}

/// A main checkout's repository config file.
pub(super) fn config_file(main: &Path) -> PathBuf {
    main.join(".config").join("agt.toml")
}

/// The trust key of `file`: its canonical path.
fn key(file: &Path) -> String {
    file.canonicalize()
        .unwrap_or_else(|_| file.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Whether the file text has a `[worktree.hooks]` table.
pub(super) fn has_hooks(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| table.get("worktree")?.get("hooks").cloned())
        .is_some()
}

/// Whether `file` is trusted with exactly the content `bytes`.
pub(super) fn is_trusted(file: &Path, bytes: &[u8]) -> Result<bool> {
    Ok(load(&store()?)?.get(&key(file)) == Some(&digest(bytes)))
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn store() -> Result<PathBuf> {
    Ok(port::folder()?.join("trust.json"))
}

/// Applies `change` under the lock and saves when it reports a change; returns that flag.
fn update(change: impl FnOnce(&mut Trusted) -> bool) -> Result<bool> {
    let file = store()?;
    let _lock = port::lock(&file.with_extension("lock"))?;
    let mut trusted = load(&file)?;
    let changed = change(&mut trusted);
    if changed {
        save(&file, &trusted)?;
    }
    Ok(changed)
}

fn load(file: &Path) -> Result<Trusted> {
    match fs::read(file) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(ctx(format!("parse {}", file.display())))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Trusted::new()),
        Err(error) => fail(format!("read {}: {error}", file.display())),
    }
}

fn save(file: &Path, trusted: &Trusted) -> Result<()> {
    let temp = file.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(trusted).map_err(ctx("serialize trust"))?;
    fs::write(&temp, text + "\n").map_err(ctx(format!("write {}", temp.display())))?;
    fs::rename(&temp, file).map_err(ctx(format!("replace {}", file.display())))
}
