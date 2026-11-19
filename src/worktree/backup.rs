//! Worktree backups are one `agents/backups/<repo>/<id>.tar.gz` each (see `archive`). Older
//! directory backups, `<id>/backup.json` with the ref `refs/agt/backups/<id>`, are still read.

use std::fs;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::archive;
use crate::core::error::{Error, Result, ctx, fail};
use crate::core::local;
use crate::core::output::Output;
use crate::git::Repo;

pub(super) const FILE: &str = "backup.json";

#[derive(Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    pub head: String,
    /// The snapshot commit of a directory backup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// The ref that pins `snapshot`.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    pub created: String,
    /// Ignored files copied under `files/`. Only backups from agt 0.20.0 and older have any;
    /// restore still puts them back.
    pub copied: Vec<String>,
    /// The ignored paths the backup left out.
    pub skipped: Vec<String>,
    /// Modes of a directory backup's regular files; an archive keeps them itself.
    #[serde(default)]
    pub file_modes: Vec<FileMode>,
    /// The tracked paths the worktree had deleted.
    #[serde(default, with = "paths", skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
pub struct FileMode {
    /// Paths MUST remain byte-exact because Git filenames need not be UTF-8.
    pub path: Vec<u8>,
    pub mode: u32,
}

/// Paths as text when they are UTF-8 and as bytes when not: they MUST remain byte-exact.
mod paths {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum Path {
        Text(String),
        Bytes(Vec<u8>),
    }

    pub fn serialize<S: Serializer>(paths: &[Vec<u8>], out: S) -> Result<S::Ok, S::Error> {
        out.collect_seq(
            paths
                .iter()
                .map(|path| match String::from_utf8(path.clone()) {
                    Ok(text) => Path::Text(text),
                    Err(error) => Path::Bytes(error.into_bytes()),
                }),
        )
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(input: D) -> Result<Vec<Vec<u8>>, D::Error> {
        Ok(Vec::<Path>::deserialize(input)?
            .into_iter()
            .map(|path| match path {
                Path::Text(text) => text.into_bytes(),
                Path::Bytes(bytes) => bytes,
            })
            .collect())
    }
}

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    /// Only this backup, in full; an error when there is none. Default: every backup, with
    /// `file_mode_count` in place of `file_modes`.
    id: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Delete a backup's archive, or an older backup's ref and directory. Without `--yes` only
    /// say what would go.
    Rm {
        id: String,
        #[arg(long)]
        yes: bool,
    },
}

pub fn run(cli: Cli) -> Result<Output> {
    match cli.command {
        None => {
            let listing = cli.id.is_none();
            let backups: Vec<Value> = match cli.id {
                Some(id) => vec![find(&id)?],
                None => all()?,
            }
            .iter()
            .map(|(record, dir)| summary(record, dir, listing))
            .collect();
            Ok(Output::Json(json!({ "backups": backups })))
        }
        Some(Command::Rm { id, yes }) => {
            let mut done = match find(&id) {
                Ok((record, path)) => {
                    if yes {
                        delete(&record, &path)?;
                    }
                    location(&record, &path)
                }
                // An archive that cannot be read is no backup to restore, but its file can go.
                Err(error) => {
                    let Some(path) = unreadable_archive(&id) else {
                        return Err(error);
                    };
                    if yes {
                        fs::remove_file(&path)
                            .map_err(ctx(format!("remove {}", path.display())))?;
                    }
                    json!({ "backup": path })
                }
            };
            if yes {
                done["removed"] = json!(id);
            } else {
                done["plan"] = json!(true);
                done["id"] = json!(id);
            }
            Ok(Output::Json(done))
        }
    }
}

/// Where a backup lives: its archive, or an older backup's directory and ref.
fn location(record: &Record, path: &Path) -> Value {
    if is_archive(path) {
        json!({ "backup": path })
    } else {
        json!({ "ref": record.reference, "dir": path })
    }
}

fn delete(record: &Record, path: &Path) -> Result<()> {
    if is_archive(path) {
        return fs::remove_file(path).map_err(ctx(format!("remove {}", path.display())));
    }
    if let (true, Some(reference)) = (record.repository.is_dir(), &record.reference) {
        Repo::at(&record.repository).delete_ref(reference)?;
    }
    fs::remove_dir_all(path).map_err(ctx(format!("remove {}", path.display())))
}

/// One backup's record plus where it lives. In the list view, `file_mode_count` and
/// `deleted_count` replace `file_modes` and `deleted`. Those two can run to megabytes across
/// many backups.
fn summary(record: &Record, path: &Path, listing: bool) -> Value {
    let mut value = serde_json::to_value(record).unwrap_or(Value::Null);
    if let (Some(fields), Some(place)) = (value.as_object_mut(), location(record, path).as_object())
    {
        fields.extend(place.clone());
    }
    if listing && let Some(fields) = value.as_object_mut() {
        fields.remove("file_modes");
        fields.remove("deleted");
        fields.insert("file_mode_count".to_owned(), json!(record.file_modes.len()));
        fields.insert("deleted_count".to_owned(), json!(record.deleted.len()));
    }
    value
}

/// Whether the backup at `path` is a `.tar.gz` file rather than an older backup's directory.
pub fn is_archive(path: &Path) -> bool {
    path.is_file()
}

pub fn root() -> Result<PathBuf> {
    local::backups_dir()
}

/// Whether any repository's backups already use `id`: ids are looked up across all of them.
pub fn id_taken(id: &str) -> bool {
    let Ok(repos) = root().and_then(|root| fs::read_dir(&root).map_err(ctx("read backups"))) else {
        return false;
    };
    repos.flatten().any(|repo| {
        repo.path().join(id).exists() || repo.path().join(format!("{id}{}", archive::EXT)).exists()
    })
}

/// Every backup with its archive file or directory, oldest first.
pub(super) fn all() -> Result<Vec<(Record, PathBuf)>> {
    let mut found = Vec::new();
    let Ok(repos) = fs::read_dir(root()?) else {
        return Ok(found);
    };
    for repo in repos.flatten().filter(|entry| entry.path().is_dir()) {
        for entry in fs::read_dir(repo.path())
            .map_err(ctx("read backups"))?
            .flatten()
        {
            if let Some(record) = read(&entry) {
                found.push((record, entry.path()));
            }
        }
    }
    found.sort_by(|a, b| a.0.created.cmp(&b.0.created).then(a.0.id.cmp(&b.0.id)));
    Ok(found)
}

/// The record of the backup at `entry`: an archive, or a directory holding `backup.json`.
/// Anything else, an unfinished `.partial` archive included, is not a backup.
fn read(entry: &fs::DirEntry) -> Option<Record> {
    let path = entry.path();
    if path.is_dir() {
        return serde_json::from_slice(&fs::read(path.join(FILE)).ok()?).ok();
    }
    let name = entry.file_name().to_string_lossy().into_owned();
    if name.starts_with('.') || !name.ends_with(archive::EXT) {
        return None;
    }
    let mut record = archive::read_record(&path).ok()?;
    // The file name is the id, so a renamed or copied archive is its own backup.
    record.id = name.strip_suffix(archive::EXT)?.to_owned();
    Some(record)
}

/// The archive file named `id` in any repository's backups, whether or not it can be read.
fn unreadable_archive(id: &str) -> Option<PathBuf> {
    fs::read_dir(root().ok()?)
        .ok()?
        .flatten()
        .map(|repo| repo.path().join(format!("{id}{}", archive::EXT)))
        .find(|path| path.is_file())
}

pub fn find(id: &str) -> Result<(Record, PathBuf)> {
    let mut matches: Vec<_> = all()?
        .into_iter()
        .filter(|(record, _)| record.id == id)
        .collect();
    match matches.len() {
        0 => match unreadable_archive(id) {
            Some(path) => fail(format!(
                "{} cannot be read; `agt worktree backups rm {id} --yes` deletes it",
                path.display()
            )),
            None => fail(format!("no worktree backup {id}")),
        },
        1 => Ok(matches.remove(0)),
        _ => Err(Error(format!("several worktree backups are named {id}"))),
    }
}
