//! `agt secrets`: finds likely credentials in tracked files or staged blobs and reports
//! only `path`, `line` and rule name; matched text is never printed, not even in errors.

mod rules;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::json;

use crate::core::error::{Result, ctx, fail};
use crate::core::output::{self, Output};
use crate::git::Repo;

use rules::RULES;

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
        Err(error) => {
            eprintln!("agt: error: {error}");
            Ok(Output::Exit(FAILED))
        }
    }
}

fn report(args: &Cli, rows: &BTreeSet<(String, usize, &'static str)>) -> Result<Output> {
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
    output::print(output)?;
    Ok(Output::Exit(if rows.is_empty() { 0 } else { FOUND }))
}

fn scan(args: &Cli) -> Result<BTreeSet<(String, usize, &'static str)>> {
    let repo = Repo::discover()?;
    let cwd = std::env::current_dir().map_err(ctx("read current directory"))?;
    let paths: Vec<String> = args
        .paths
        .iter()
        .map(|path| cwd.join(path).to_string_lossy().into_owned())
        .collect();
    // A missing path would match nothing and report clean, which a scanner MUST NOT do. A
    // staged scan also accepts a path that is only in the index, such as a staged file since
    // deleted from disk.
    for (path, full) in args.paths.iter().zip(&paths) {
        let found = Path::new(full).symlink_metadata().is_ok()
            || (args.staged && repo.succeeds(&["ls-files", "--error-unmatch", "--", full])?);
        if !found {
            return fail(format!("no such path: {}", path.display()));
        }
    }
    let mut git: Vec<&str> = if args.staged {
        vec![
            "diff",
            "--cached",
            "--raw",
            "--no-abbrev",
            "--no-renames",
            "-z",
            // `T` keeps a symlink replaced by a regular file; the mode check skips the reverse.
            "--diff-filter=ACMRT",
        ]
    } else {
        vec!["ls-files", "-co", "--exclude-standard", "-z"]
    };
    git.push("--");
    git.extend(paths.iter().map(String::as_str));
    let listing = repo.checked(&git, None, &[])?;
    let mut fields = listing
        .split(|&byte| byte == 0)
        .filter(|field| !field.is_empty());
    let mut rows = BTreeSet::new();
    while let Some(field) = fields.next() {
        let (name, object) = if args.staged {
            // `:<old mode> <new mode> <old id> <new id> <status>`, then the path.
            let meta = String::from_utf8_lossy(field).into_owned();
            let Some(name) = fields.next() else { break };
            let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
            let [_, mode, _, id, _] = parts[..] else {
                continue;
            };
            // Submodules and symlinks are not file content.
            if mode == "160000" || mode == "120000" {
                continue;
            }
            (
                String::from_utf8_lossy(name).into_owned(),
                Some(id.to_owned()),
            )
        } else {
            (String::from_utf8_lossy(field).into_owned(), None)
        };
        let Some(content) = read(&repo, &name, object.as_deref())? else {
            continue;
        };
        scan_text(&name, &content, &mut rows);
    }
    Ok(rows)
}

/// The file's bytes (the staged blob `object`, else the working-tree file), or `None` for a
/// file that is gone, binary or not a regular file (symlinks are never followed).
fn read(repo: &Repo, name: &str, object: Option<&str>) -> Result<Option<Vec<u8>>> {
    let content = if let Some(id) = object {
        repo.checked(&["cat-file", "blob", id], None, &[])?
    } else {
        let path: &Path = &repo.root().join(name);
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_file() => {}
            _ => return Ok(None),
        }
        match std::fs::read(path) {
            Ok(content) => content,
            Err(_) => return Ok(None),
        }
    };
    Ok((!content.iter().take(8000).any(|&byte| byte == 0)).then_some(content))
}

fn scan_text(name: &str, content: &[u8], rows: &mut BTreeSet<(String, usize, &'static str)>) {
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
