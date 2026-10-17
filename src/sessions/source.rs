//! Finding transcripts and reading their records, in the order the script walked them: the
//! order decides which file a response copied into a resumed session counts toward.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::Value;

use crate::core::transcripts::{claude_profiles, codex_roots};
use crate::sessions::codex::{self, Adapter};
use crate::sessions::py::{self, get, key, object, text, truthy};
use crate::sessions::{Harness, Scope};

static SCRATCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-cache-agents-scratch|-cache-claude-tmp|^-private-tmp|^-tmp")
        .expect("valid pattern")
});

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Claude,
    Codex,
}

pub struct Transcript {
    pub path: PathBuf,
    pub project: String,
    pub session: String,
    pub agent: String,
    pub kind: Kind,
}

/// One API response: its last record's usage and model, and how many tools it called.
pub struct Call {
    pub usage: Value,
    pub model: String,
    pub tools: usize,
}

fn lines(path: &Path, mut each: impl FnMut(&Value)) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if let Ok(record) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&line)) {
            each(&record);
        }
    }
}

fn first_line(path: &Path) -> Value {
    let Ok(file) = File::open(path) else {
        return Value::Null;
    };
    let mut line = Vec::new();
    if BufReader::new(file).read_until(b'\n', &mut line).is_err() {
        return Value::Null;
    }
    serde_json::from_str(&String::from_utf8_lossy(&line)).unwrap_or(Value::Null)
}

impl Transcript {
    pub fn harness(&self) -> &'static str {
        match self.kind {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
        }
    }

    /// Calls `each` with every record, skipping those stamped before `since` (`YYYY-MM-DD`).
    pub fn records(&self, since: &str, mut each: impl FnMut(&Value)) {
        let mut keep = |record: &Value| {
            let stamp = get(record, "timestamp");
            if since.is_empty() || stamp.as_str().is_none_or(|stamp| stamp >= since) {
                each(record);
            }
        };
        match self.kind {
            Kind::Claude => lines(&self.path, keep),
            Kind::Codex => {
                let mut adapter = Adapter::new(self.path.to_string_lossy().into_owned());
                lines(&self.path, |record| adapter.feed(record, &mut keep));
                adapter.finish(&mut keep);
            }
        }
    }

    /// One `Call` per API response. Transcripts write one record per content block, so tool
    /// calls are counted across records sharing the response id. Responses whose id is in `seen`
    /// were counted from an earlier file, such as those copied into a resumed session.
    pub fn api_calls(&self, since: &str, seen: &mut HashSet<String>) -> Vec<Call> {
        let mut calls: IndexMap<String, Call> = IndexMap::new();
        let mut tools: IndexMap<String, HashSet<String>> = IndexMap::new();
        let mut anonymous = 0usize;
        self.records(since, |record| {
            let message = object(record, "message");
            if text(record, "type") != "assistant"
                || !truthy(get(message, "usage"))
                || text(message, "model") == "<synthetic>"
            {
                return;
            }
            let id = get(message, "id");
            let name = if truthy(id) {
                key(id)
            } else {
                anonymous += 1;
                format!("a{anonymous}")
            };
            let model = match message.get("model") {
                Some(Value::String(model)) => model.clone(),
                Some(other) => other.to_string(),
                None => "?".into(),
            };
            calls.insert(
                name.clone(),
                Call {
                    usage: get(message, "usage").clone(),
                    model,
                    tools: 0,
                },
            );
            let used = tools.entry(name).or_default();
            if let Some(blocks) = get(message, "content").as_array() {
                for block in blocks {
                    if text(block, "type") == "tool_use" {
                        used.insert(key(get(block, "id")));
                    }
                }
            }
        });
        calls
            .into_iter()
            .filter(|(name, _)| {
                // Only string ids are shared between files.
                !name.starts_with('s') || seen.insert(name.clone())
            })
            .map(|(name, mut call)| {
                call.tools = tools.get(&name).map_or(0, HashSet::len);
                call
            })
            .collect()
    }
}

/// The `.jsonl` entries of `directory`, in `readdir` order.
fn jsonl_in(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".jsonl"))
        .map(|entry| entry.path())
        .collect()
}

/// `Path.rglob('*.jsonl')` as Python 3.13+ walks it: a directory's matches are listed when its
/// parent is scanned, then directories are scanned last-found first, without following links.
fn rglob(root: &Path) -> Vec<PathBuf> {
    let mut found = jsonl_in(root);
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                let path = entry.path();
                found.extend(jsonl_in(&path));
                stack.push(path);
            }
        }
    }
    found
}

/// `Path(text).expanduser()` as a string: `~` expanded, `.` parts, repeated and trailing
/// slashes dropped.
fn normalize(text: &str, home: &Path) -> PathBuf {
    let expanded = match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            format!("{}{rest}", home.display())
        }
        _ => text.to_owned(),
    };
    let absolute = expanded.starts_with('/');
    let parts: Vec<&str> = expanded
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    let joined = parts.join("/");
    PathBuf::from(match (absolute, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".into(),
        (false, false) => joined,
    })
}

fn modified(path: &Path) -> Option<f64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(match modified.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_secs_f64(),
        Err(before) => -before.duration().as_secs_f64(),
    })
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn wanted(scope: &Scope, kind: Kind) -> bool {
    matches!(
        (scope.harness, kind),
        (Harness::All, _) | (Harness::Claude, Kind::Claude) | (Harness::Codex, Kind::Codex)
    )
}

/// A file found under a root: which harness wrote it and, for Codex, its session line.
fn codex_meta(path: &Path, kind: Option<Kind>) -> (Kind, Option<codex::Meta>) {
    if kind == Some(Kind::Claude) {
        return (Kind::Claude, None);
    }
    let meta = codex::meta(&first_line(path));
    match (kind, meta) {
        (_, Some(meta)) => (Kind::Codex, Some(meta)),
        (Some(Kind::Codex), None) => (Kind::Codex, None),
        (_, None) => (Kind::Claude, None),
    }
}

/// The transcripts in scope, in the script's order. Prints how many scratch files it skipped.
pub fn transcripts(scope: &Scope) -> Vec<Transcript> {
    let home = home();
    let mut found = Vec::new();
    if !scope.files.is_empty() {
        for file in &scope.files {
            let path = PathBuf::from(file);
            let (kind, meta) = codex_meta(&path, None);
            if !wanted(scope, kind) {
                continue;
            }
            let project = path
                .parent()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            found.push(match meta {
                Some(meta) => Transcript {
                    project: codex::project(&meta.cwd),
                    session: meta.session,
                    agent: meta.agent,
                    path,
                    kind,
                },
                None => Transcript {
                    session: stem(&path),
                    agent: "main".into(),
                    project,
                    path,
                    kind,
                },
            });
        }
        return found;
    }
    // A root named on the command line can hold either harness; each file says which.
    let roots: Vec<(PathBuf, Option<Kind>)> = if scope.roots.is_empty() {
        let mut roots: Vec<(PathBuf, Option<Kind>)> = claude_profiles(&home)
            .into_iter()
            .map(|profile| profile.join("projects"))
            .filter(|root| root.is_dir())
            .map(|root| (root, Some(Kind::Claude)))
            .collect();
        roots.extend(
            codex_roots(&home)
                .into_iter()
                .map(|root| (root, Some(Kind::Codex))),
        );
        roots
    } else {
        scope
            .roots
            .iter()
            .map(|root| (normalize(root, &home), None))
            .collect()
    };
    let mut skipped = 0;
    for (root, kind) in roots {
        if kind.is_some_and(|kind| !wanted(scope, kind)) {
            continue;
        }
        for path in rglob(&root) {
            let Ok(relative) = path.strip_prefix(&root) else {
                continue;
            };
            let parts: Vec<String> = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            let outside = |project: &str| {
                scope
                    .project
                    .as_ref()
                    .is_some_and(|text| !project.contains(text.as_str()))
            };
            // Default Claude Code files are filtered before they are opened, as the script did.
            if kind == Some(Kind::Claude) {
                if outside(&parts[0]) {
                    continue;
                }
                if !scope.scratch && SCRATCH.is_match(&parts[0]) {
                    skipped += 1;
                    continue;
                }
            }
            if modified(&path).is_none_or(|time| time < scope.cutoff) {
                continue;
            }
            let (kind, meta) = codex_meta(&path, kind);
            let project = match (&meta, kind) {
                (Some(meta), _) => codex::project(&meta.cwd),
                (None, Kind::Codex) => String::new(),
                (None, Kind::Claude) => parts[0].clone(),
            };
            if !wanted(scope, kind) || outside(&project) {
                continue;
            }
            if !scope.scratch && SCRATCH.is_match(&project) {
                skipped += 1;
                continue;
            }
            let transcript = if kind == Kind::Codex {
                let meta = meta.unwrap_or_else(|| codex::Meta {
                    cwd: String::new(),
                    session: py::head(py::tail(&stem(&path), 36), 8).to_owned(),
                    agent: "main".into(),
                });
                Transcript {
                    path,
                    project,
                    session: meta.session,
                    agent: meta.agent,
                    kind,
                }
            } else {
                let agent = if parts.iter().any(|part| part == "subagents") {
                    fs::read_to_string(path.with_extension("meta.json"))
                        .ok()
                        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                        .map_or_else(
                            || "?".to_owned(),
                            |meta| match get(&meta, "agentType") {
                                Value::String(agent) if !agent.is_empty() => agent.clone(),
                                _ => "?".to_owned(),
                            },
                        )
                } else if parts.len() > 2 {
                    "other".to_owned()
                } else {
                    "main".to_owned()
                };
                let session = if parts.len() > 2 {
                    parts[1].clone()
                } else {
                    stem(&path)
                };
                Transcript {
                    project,
                    session: py::head(&session, 8).to_owned(),
                    agent,
                    path,
                    kind,
                }
            };
            found.push(transcript);
        }
    }
    if skipped > 0 {
        eprintln!("(skipped {skipped} scratch transcripts; --scratch includes them)");
    }
    found
}
