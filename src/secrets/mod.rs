//! `agt secrets`: finds likely credentials in tracked files or staged blobs and reports
//! only `path`, `line` and rule name; matched text is never printed, not even in errors.

mod rules;

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::json;

use crate::core::error::{Result, ctx, fail};
use crate::core::output::Output;
use crate::git::Repo;

use rules::RULES;

/// The distinct findings of a scan: file, line and the rule that matched.
type Findings = BTreeSet<(String, usize, &'static str)>;

/// The index modes of a submodule and of a symlink.
const GITLINK_MODE: u32 = 0o160000;
const SYMLINK_MODE: u32 = 0o120000;

const ALLOW: &str = "agt-secrets: allow";
const FOUND: u8 = 1;
const FAILED: u8 = 2;

#[derive(Args)]
pub struct Cli {
    /// Limit the scan to these files or directories.
    paths: Vec<PathBuf>,
    /// Scan the staged blobs instead of the working tree.
    #[arg(long)]
    staged: bool,
    /// Print JSON rows instead of `path:line rule` lines.
    #[arg(long)]
    json: bool,
}

pub fn run(cli: Cli) -> Result<Output> {
    match scan(&cli) {
        Ok(rows) => report(&cli, &rows),
        Err(error) => Ok(Output::Failure(error, FAILED)),
    }
}

fn report(args: &Cli, rows: &Findings) -> Result<Output> {
    let output = if args.json {
        Output::Json(
            rows.iter()
                .map(|(path, line, rule)| json!({"path": path, "line": line, "rule": rule}))
                .collect(),
        )
    } else {
        Output::Text(
            rows.iter()
                .map(|(path, line, rule)| format!("{path}:{line} {rule}\n"))
                .collect(),
        )
    };
    Ok(output.exiting(if rows.is_empty() { 0 } else { FOUND }))
}

fn scan(args: &Cli) -> Result<Findings> {
    let repo = Repo::discover()?;
    let cwd = std::env::current_dir().map_err(ctx("read current directory"))?;
    let paths: Vec<PathBuf> = args.paths.iter().map(|path| cwd.join(path)).collect();
    // A missing path would match nothing and report clean, which a scanner MUST NOT do. A
    // staged scan also accepts a path in the index or in the staged diff, such as a staged
    // file since deleted from disk or a staged deletion that hooks pass along.
    for (path, full) in args.paths.iter().zip(&paths) {
        let found =
            full.symlink_metadata().is_ok() || (args.staged && repo.has_staged_path(full)?);
        if !found {
            return fail(format!("no such path: {}", path.display()));
        }
    }
    let wanted: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let mut rows = BTreeSet::new();
    let files: Vec<(Vec<u8>, Option<String>)> = if args.staged {
        repo.staged_changes(&wanted)?
            .into_iter()
            // Submodules and symlinks are not file content.
            .filter(|change| change.new_mode != GITLINK_MODE && change.new_mode != SYMLINK_MODE)
            .map(|change| (change.path, Some(change.new_id)))
            .collect()
    } else {
        repo.tracked_and_untracked(&wanted)?
            .into_iter()
            .map(|name| (name, None))
            .collect()
    };
    for (name, object) in files {
        let Some(content) = read(&repo, &name, object.as_deref())? else {
            continue;
        };
        scan_text(&String::from_utf8_lossy(&name), &content, &mut rows);
    }
    Ok(rows)
}

/// The file's bytes (the staged blob `object`, else the working-tree file), or `None` for a
/// file that is gone, binary or not a regular file (symlinks are never followed).
fn read(repo: &Repo, name: &[u8], object: Option<&str>) -> Result<Option<Vec<u8>>> {
    let content = if let Some(id) = object {
        repo.blob_content(id)?
    } else {
        let path = repo.root().join(OsStr::from_bytes(name));
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ctx(format!("inspect {}", path.display()))(error)),
        }
        match std::fs::read(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ctx(format!("read {}", path.display()))(error)),
        }
    };
    Ok((!content.iter().take(8000).any(|&byte| byte == 0)).then_some(content))
}

fn scan_text(name: &str, content: &[u8], rows: &mut Findings) {
    for (index, line) in String::from_utf8_lossy(content).lines().enumerate() {
        if line.contains(ALLOW) {
            continue;
        }
        let hits: Vec<&'static str> = RULES
            .iter()
            .filter(|rule| rule.matches(line))
            .map(|rule| rule.name)
            .collect();
        // A named format explains the line; the generic rule would only repeat it.
        let named = hits.iter().any(|rule| *rule != rules::GENERIC);
        for rule in hits
            .into_iter()
            .filter(|rule| !named || *rule != rules::GENERIC)
        {
            rows.insert((name.to_owned(), index + 1, rule));
        }
    }
}
