//! Finding the transcript files of Codex and Claude profiles under the home directory.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::core::error::Result;
use crate::task::files::{error_code, home};
use crate::task::sessions::scan::{MAX_LINE_BYTES, READ_CHUNK};

#[derive(Clone, Serialize)]
pub struct Unreadable {
    pub source: String,
    pub error: String,
}

/// What a run looked at and what it could not, printed so a caller can judge a missing result.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Coverage {
    pub roots: Vec<String>,
    pub discovered: usize,
    pub indexed: usize,
    pub malformed_lines: u64,
    pub oversized_lines: u64,
    pub unreadable: Vec<Unreadable>,
    pub removed_files: usize,
    pub skipped_files: usize,
    pub scan_limit: Option<u64>,
    pub scan_limit_reached: bool,
    pub unscanned_directories: usize,
    pub byte_limit: Option<u64>,
    pub byte_limit_reached: bool,
    pub scanned_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_turns: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub human_turns: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inherited_turns: Option<usize>,
}

pub struct Discovered {
    pub files: Vec<String>,
    pub coverage: Coverage,
}

/// Whether the first line of `file` names `session` as its own id.
fn first_line_session(file: &Path, session: &str) -> bool {
    let Ok(mut handle) = File::open(file) else {
        return false;
    };
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; READ_CHUNK];
    while pending.len() <= MAX_LINE_BYTES {
        let Ok(count) = handle.read(&mut chunk) else {
            return false;
        };
        if count == 0 {
            break;
        }
        let newline = chunk[..count].iter().position(|byte| *byte == b'\n');
        pending.extend_from_slice(&chunk[..newline.unwrap_or(count)]);
        if newline.is_some() {
            break;
        }
    }
    let text = String::from_utf8_lossy(&pending);
    let encoded = serde_json::to_string(session).unwrap_or_default();
    if ["id", "sessionId", "session_id"]
        .iter()
        .any(|key| text.contains(&format!("\"{key}\":{encoded}")))
    {
        return true;
    }
    let Ok(entry) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    let own = |value: Option<&Value>| value.and_then(Value::as_str) == Some(session);
    own(entry.get("payload").and_then(|payload| payload.get("id")))
        || own(entry.get("sessionId"))
        || own(entry.get("session_id"))
}

fn claude_profiles(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(home) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    entries
        .into_iter()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name == ".claude" || name.starts_with(".claude-")
        })
        .map(|entry| entry.path())
        .collect()
}

/// The items in first-seen order without repeats.
pub fn unique(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect()
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Lists transcript files, stopping after `maximum` unless `session` asks for an exact match,
/// which is found even beyond the limit.
pub fn discover(maximum: u64, session: Option<&str>) -> Result<Discovered> {
    let home = home()?;
    let mut roots = vec![
        home.join(".codex").join("sessions"),
        home.join(".codex").join("archived_sessions"),
    ];
    let profiles = claude_profiles(&home);
    for profile in &profiles {
        roots.extend(["projects", "archive", "archives"].map(|name| profile.join(name)));
    }
    let mut files: Vec<String> = Vec::new();
    let mut preferred: Vec<String> = Vec::new();
    let histories = std::iter::once(home.join(".codex").join("history.jsonl"))
        .chain(profiles.iter().map(|profile| profile.join("history.jsonl")));
    files.extend(
        histories
            .filter(|file| file.exists())
            .map(|file| lossy(&file)),
    );
    let mut coverage = Coverage {
        roots: roots.iter().map(|root| lossy(root)).collect(),
        discovered: 0,
        indexed: 0,
        malformed_lines: 0,
        oversized_lines: 0,
        unreadable: Vec::new(),
        removed_files: 0,
        skipped_files: 0,
        scan_limit: (maximum != u64::MAX).then_some(maximum),
        scan_limit_reached: false,
        unscanned_directories: 0,
        byte_limit: None,
        byte_limit_reached: false,
        scanned_bytes: 0,
        indexed_turns: None,
        human_turns: None,
        inherited_turns: None,
    };
    let count = |files: &[String], preferred: &[String]| (files.len() + preferred.len()) as u64;
    let mut pending = roots;
    while !pending.is_empty() && (session.is_some() || count(&files, &preferred) < maximum) {
        let directory = pending.pop().unwrap_or_default();
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                if error.kind() != ErrorKind::NotFound {
                    coverage.unreadable.push(Unreadable {
                        source: lossy(&directory),
                        error: error_code(&error),
                    });
                }
                continue;
            }
        };
        // Node's `readdirSync` returns names sorted, and the order decides which of two
        // duplicate transcripts is kept, so the same order is used here.
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = entry.file_type();
            if kind.as_ref().is_ok_and(|kind| kind.is_dir()) {
                pending.push(path);
            } else if kind.is_ok_and(|kind| kind.is_file()) && name.ends_with(".jsonl") {
                if session.is_some_and(|id| name.contains(id) || first_line_session(&path, id)) {
                    preferred.push(lossy(&path));
                } else if count(&files, &preferred) < maximum {
                    files.push(lossy(&path));
                }
            }
            if session.is_none() && count(&files, &preferred) >= maximum {
                break;
            }
        }
    }
    coverage.discovered = files.len() + preferred.len();
    coverage.scan_limit_reached = maximum != u64::MAX && count(&files, &preferred) >= maximum;
    coverage.unscanned_directories = pending.len();
    Ok(Discovered {
        files: unique(preferred.into_iter().chain(files)),
        coverage,
    })
}
