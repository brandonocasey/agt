//! Worktree backups: `agents/backups/<repo>/<id>/backup.json`, plus the ref
//! `refs/agt/backups/<id>` that keeps the snapshot commit alive. Backups are never pruned
//! automatically.

use std::fs;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::core::error::{Error, Result, ctx, fail};
use crate::core::local;
use crate::core::output::Output;
use crate::git::Repo;

const FILE: &str = "backup.json";
pub const REF_PREFIX: &str = "refs/agt/backups/";

#[derive(Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    pub head: String,
    pub snapshot: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub created: String,
    /// Ignored files copied under `files/`. Only backups from agt 0.20.0 and older have any;
    /// restore still puts them back.
    pub copied: Vec<String>,
    /// The ignored paths the backup left out.
    pub skipped: Vec<String>,
    #[serde(default)]
    pub file_modes: Vec<FileMode>,
}

#[derive(Serialize, Deserialize)]
pub struct FileMode {
    /// Paths MUST remain byte-exact because Git filenames need not be UTF-8.
    pub path: Vec<u8>,
    pub mode: u32,
}

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    /// Only this backup (an error when there is none); default: every backup.
    id: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Delete a backup's ref and directory. Without `--yes` only say what would go.
    Rm {
        id: String,
        #[arg(long)]
        yes: bool,
    },
}

pub fn run(cli: Cli) -> Result<Output> {
    match cli.command {
        None => {
            let backups: Vec<Value> = match cli.id {
                Some(id) => vec![find(&id)?],
                None => all()?,
            }
            .iter()
            .map(|(record, dir)| summary(record, dir))
            .collect();
            Ok(Output::Json(json!({ "backups": backups })))
        }
        Some(Command::Rm { id, yes }) => {
            let (record, dir) = find(&id)?;
            if !yes {
                return Ok(Output::Json(
                    json!({ "plan": true, "id": id, "ref": record.reference, "dir": dir }),
                ));
            }
            if record.repository.is_dir() {
                Repo::at(&record.repository).git(&["update-ref", "-d", &record.reference])?;
            }
            fs::remove_dir_all(&dir).map_err(ctx(format!("remove {}", dir.display())))?;
            Ok(Output::Json(
                json!({ "removed": id, "ref": record.reference, "dir": dir }),
            ))
        }
    }
}

fn summary(record: &Record, dir: &Path) -> Value {
    let mut value = serde_json::to_value(record).unwrap_or(Value::Null);
    value["dir"] = json!(dir);
    value
}

pub fn root() -> Result<PathBuf> {
    local::backups_dir()
}

pub fn write(dir: &Path, record: &Record) -> Result<()> {
    local::write_json(&dir.join(FILE), record, false)
}

/// Whether any repository's backups already use `id`: ids are looked up across all of them.
pub fn id_taken(id: &str) -> bool {
    let Ok(repos) = root().and_then(|root| fs::read_dir(&root).map_err(ctx("read backups"))) else {
        return false;
    };
    repos.flatten().any(|repo| repo.path().join(id).exists())
}

/// Every backup with its directory, oldest first.
pub(super) fn all() -> Result<Vec<(Record, PathBuf)>> {
    let mut found = Vec::new();
    let Ok(repos) = fs::read_dir(root()?) else {
        return Ok(found);
    };
    for repo in repos.flatten().filter(|entry| entry.path().is_dir()) {
        for dir in fs::read_dir(repo.path())
            .map_err(ctx("read backups"))?
            .flatten()
        {
            let Ok(bytes) = fs::read(dir.path().join(FILE)) else {
                continue;
            };
            if let Ok(record) = serde_json::from_slice::<Record>(&bytes) {
                found.push((record, dir.path()));
            }
        }
    }
    found.sort_by(|a, b| a.0.created.cmp(&b.0.created).then(a.0.id.cmp(&b.0.id)));
    Ok(found)
}

pub fn find(id: &str) -> Result<(Record, PathBuf)> {
    let mut matches: Vec<_> = all()?
        .into_iter()
        .filter(|(record, _)| record.id == id)
        .collect();
    match matches.len() {
        0 => fail(format!("no worktree backup {id}")),
        1 => Ok(matches.remove(0)),
        _ => Err(Error(format!("several worktree backups are named {id}"))),
    }
}
