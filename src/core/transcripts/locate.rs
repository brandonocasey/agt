//! Finding transcript files and telling which harness, project, session and agent wrote each.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use super::codex;
use super::json::{get, head, tail, text};
use crate::core::clock;

/// The longest first line read when looking for a session id.
const MAX_FIRST_LINE: u64 = 1024 * 1024;

static SCRATCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-cache-agents-scratch|-cache-claude-tmp|^-private-tmp|^-tmp")
        .expect("valid pattern")
});

/// Whether a project directory name is a scratch or probe directory, such as one under `/tmp`.
pub fn is_scratch(project: &str) -> bool {
    SCRATCH.is_match(project)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Claude,
    Codex,
}

/// A transcript file and what its location and first line say about it.
pub struct Transcript {
    pub path: PathBuf,
    pub project: String,
    /// The first 8 characters of a Claude Code session id, or the last 8 of a Codex thread id
    /// (UUIDv7, whose first characters are a timestamp shared by sessions started together).
    pub session: String,
    pub agent: String,
    pub kind: Kind,
    /// A `codex exec` run, whose prompts no person typed.
    pub scripted: bool,
}

/// Claude Code profile directories in `home`: `.claude` and `.claude-<name>`, links followed,
/// sorted by name.
pub fn claude_profiles(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(home) else {
        return Vec::new();
    };
    let mut profiles: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".claude"))
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    profiles.sort();
    profiles
}

/// Every Claude Code profile's `projects` directory.
pub fn claude_projects(home: &Path) -> Vec<PathBuf> {
    claude_profiles(home)
        .into_iter()
        .map(|profile| profile.join("projects"))
        .filter(|projects| projects.is_dir())
        .collect()
}

/// Codex session roots: live sessions, then archived ones.
pub fn codex_roots(home: &Path) -> [PathBuf; 2] {
    let codex = home.join(".codex");
    [codex.join("sessions"), codex.join("archived_sessions")]
}

/// Every directory that can hold transcripts: Codex's roots, then each Claude Code profile's
/// `archive` and `archives`, and its `projects` when `projects` is set.
pub fn all_roots(home: &Path, projects: bool) -> Vec<PathBuf> {
    let mut roots = codex_roots(home).to_vec();
    for profile in claude_profiles(home) {
        if projects {
            roots.push(profile.join("projects"));
        }
        roots.extend(["archive", "archives"].map(|name| profile.join(name)));
    }
    roots
}

/// The prompt history files that exist: Codex's, then each Claude Code profile's.
pub fn history_files(home: &Path) -> Vec<PathBuf> {
    std::iter::once(home.join(".codex").join("history.jsonl"))
        .chain(
            claude_profiles(home)
                .into_iter()
                .map(|profile| profile.join("history.jsonl")),
        )
        .filter(|file| file.exists())
        .collect()
}

/// What one step of a [`Walker`] found.
pub enum Step {
    File(PathBuf),
    Unreadable(PathBuf, std::io::Error),
}

/// The `.jsonl` files under some roots, depth first without following links: a directory's
/// files in name order, then its subdirectories in name order, roots in the order given.
pub struct Walker {
    pending: Vec<PathBuf>,
    files: VecDeque<PathBuf>,
}

impl Walker {
    pub fn new(mut roots: Vec<PathBuf>) -> Self {
        roots.reverse();
        Self {
            pending: roots,
            files: VecDeque::new(),
        }
    }

    /// How many directories have not been read yet.
    pub fn unscanned(&self) -> usize {
        self.pending.len()
    }
}

impl Iterator for Walker {
    type Item = Step;

    fn next(&mut self) -> Option<Step> {
        loop {
            if let Some(file) = self.files.pop_front() {
                return Some(Step::File(file));
            }
            let directory = self.pending.pop()?;
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) => return Some(Step::Unreadable(directory, error)),
            };
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(fs::DirEntry::file_name);
            let mut directories = Vec::new();
            for entry in entries {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_dir() {
                    directories.push(entry.path());
                } else if kind.is_file() && entry.file_name().to_string_lossy().ends_with(".jsonl")
                {
                    self.files.push_back(entry.path());
                }
            }
            self.pending.extend(directories.into_iter().rev());
        }
    }
}

/// The first line of `path`, up to a limit.
fn first_line(path: &Path) -> Vec<u8> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut line = Vec::new();
    let _ = BufReader::new(file.take(MAX_FIRST_LINE)).read_until(b'\n', &mut line);
    line
}

/// The first line of `path` as JSON, or `Null`.
pub(super) fn first_record(path: &Path) -> Value {
    serde_json::from_slice(&first_line(path)).unwrap_or(Value::Null)
}

/// Whether `path` is a transcript of `session`: its name holds the id, or its first line names
/// the id as its own, its session, or its parent thread.
pub fn belongs_to(path: &Path, session: &str) -> bool {
    if path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().contains(session))
    {
        return true;
    }
    let line = first_line(path);
    let text = String::from_utf8_lossy(&line);
    let encoded = serde_json::to_string(session).unwrap_or_default();
    if ["id", "sessionId", "session_id", "parent_thread_id"]
        .iter()
        .any(|key| text.contains(&format!("\"{key}\":{encoded}")))
    {
        return true;
    }
    let Ok(entry) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    let own = |value: &Value| value.as_str() == Some(session);
    let payload = get(&entry, "payload");
    let spawn = get(get(get(payload, "source"), "subagent"), "thread_spawn");
    [
        get(payload, "id"),
        get(payload, "parent_thread_id"),
        get(spawn, "parent_thread_id"),
        get(&entry, "sessionId"),
        get(&entry, "session_id"),
    ]
    .into_iter()
    .any(own)
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl Transcript {
    /// A file named on its own: its project is the directory it is in.
    pub fn of_file(path: PathBuf) -> Self {
        let parts: Vec<String> = [path.parent().and_then(Path::file_name), path.file_name()]
            .into_iter()
            .flatten()
            .map(|part| part.to_string_lossy().into_owned())
            .collect();
        Self::locate(path, &parts, None)
    }

    pub fn harness(&self) -> &'static str {
        match self.kind {
            Kind::Claude => "claude",
            Kind::Codex => "codex",
        }
    }

    /// Reads off the harness, project, session and agent of `path`. `parts` are its path
    /// components below the root it was found under (for a Claude Code root: the project
    /// directory, then the file, or the session directory and the files inside it). `hint` is
    /// the harness the root belongs to, or `None` for a root that can hold either, where the
    /// file's first line says which.
    pub fn locate(path: PathBuf, parts: &[String], hint: Option<Kind>) -> Self {
        let meta = match hint {
            Some(Kind::Claude) => None,
            _ => codex::meta(&first_record(&path)),
        };
        if let Some(meta) = meta {
            return Self {
                project: codex::project(&meta.cwd),
                session: meta.session,
                agent: meta.agent,
                scripted: meta.scripted,
                kind: Kind::Codex,
                path,
            };
        }
        if hint == Some(Kind::Codex) {
            return Self {
                project: String::new(),
                session: tail(&stem(&path), 8).to_owned(),
                agent: "main".into(),
                scripted: false,
                kind: Kind::Codex,
                path,
            };
        }
        let nested = parts.len() > 2;
        let agent = if parts.iter().any(|part| part == "subagents") {
            fs::read_to_string(path.with_extension("meta.json"))
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .and_then(|meta| get(&meta, "agentType").as_str().map(str::to_owned))
                .filter(|agent| !agent.is_empty())
                .unwrap_or_else(|| "?".into())
        } else if nested {
            "other".into()
        } else {
            "main".into()
        };
        let session = if nested {
            parts[1].clone()
        } else {
            stem(&path)
        };
        Self {
            project: parts.first().cloned().unwrap_or_default(),
            session: head(&session, 8).to_owned(),
            agent,
            scripted: false,
            kind: Kind::Claude,
            path,
        }
    }
}

/// The full ids, newest rollout first, of the Codex sessions started in `cwd` whose rollout
/// changed at or after `since` (Unix seconds); a subagent's rollout is not one.
pub fn codex_sessions(home: &Path, cwd: &Path, since: i64) -> Vec<String> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_owned());
    let mut found = Vec::new();
    for step in Walker::new(vec![codex_roots(home)[0].clone()]) {
        let Step::File(path) = step else {
            continue;
        };
        let Some(modified) = fs::metadata(&path)
            .ok()
            .and_then(|metadata| clock::mtime_secs(&metadata))
            .filter(|secs| *secs >= since)
        else {
            continue;
        };
        let first = first_record(&path);
        let payload = get(&first, "payload");
        let recorded = Path::new(text(payload, "cwd"));
        if text(&first, "type") != "session_meta"
            || get(get(payload, "source"), "subagent").is_object()
            || recorded
                .canonicalize()
                .unwrap_or_else(|_| recorded.to_owned())
                != cwd
        {
            continue;
        }
        found.push((modified, text(payload, "id").to_owned()));
    }
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found.into_iter().map(|(_, session)| session).collect()
}

/// The transcripts of one session, by its full id: the main file and its subagents' files, in
/// Claude Code profiles and Codex alike.
pub fn session_files(home: &Path, session: &str) -> Vec<Transcript> {
    let mut found = Vec::new();
    if session.is_empty() || session.contains('/') {
        return found;
    }
    for projects in claude_projects(home) {
        // Claude Code names a session's file and subagent directory after its id, so the
        // projects are not searched file by file.
        let Ok(entries) = fs::read_dir(&projects) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for project in entries {
            let name = project.file_name().to_string_lossy().into_owned();
            let main = project.path().join(format!("{session}.jsonl"));
            let nested = project.path().join(session);
            let files = main.is_file().then_some(main).into_iter().chain(
                Walker::new(vec![nested]).filter_map(|step| match step {
                    Step::File(file) => Some(file),
                    Step::Unreadable(..) => None,
                }),
            );
            for file in files {
                let mut parts = vec![name.clone()];
                parts.extend(
                    file.strip_prefix(project.path())
                        .iter()
                        .flat_map(|rest| rest.components())
                        .map(|part| part.as_os_str().to_string_lossy().into_owned()),
                );
                found.push(Transcript::locate(file, &parts, Some(Kind::Claude)));
            }
        }
    }
    for step in Walker::new(all_roots(home, false)) {
        if let Step::File(file) = step
            && belongs_to(&file, session)
        {
            found.push(Transcript::of_file(file));
        }
    }
    found
}
