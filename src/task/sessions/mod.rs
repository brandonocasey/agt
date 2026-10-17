//! The private session index (`agents/tasks/session-index/index.json`, version 4) and the
//! `index`, `resolve` and `search` commands built on it.

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::core::clock;
use crate::core::error::{Result, ctx, fail};
use crate::core::output::Output;
use crate::task::args::Options;
use crate::task::files::{self, Lock, error_code};
use crate::task::records::pretty;
use crate::task::text::{js_number, limited, safe_integer, utf16_offset, utf16_slice};

mod discover;
mod scan;

use self::discover::{Coverage, Unreadable, discover, unique};
use self::scan::{FileState, Stat, Turn, scan_file, visible_turns};

const INDEX_VERSION: u64 = 4;
const CONTEXT_TURNS: usize = 2;
const LOCK_WAIT: Duration = Duration::from_secs(5);
const UNLIMITED: u64 = u64::MAX;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Index {
    version: u64,
    files: IndexMap<String, FileState>,
    #[serde(default)]
    coverage: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
}

impl Index {
    fn empty() -> Self {
        Self {
            version: INDEX_VERSION,
            files: IndexMap::new(),
            coverage: json!({}),
            updated_at: None,
        }
    }
}

/// A turn after duplicates across files are merged, with every file that holds it.
struct Merged {
    turn: Turn,
    sources: Vec<String>,
}

struct Indexed {
    updated_at: String,
    coverage: Coverage,
    turns: Vec<Merged>,
}

fn index_file() -> Result<PathBuf> {
    Ok(files::index_root()?.join("index.json"))
}

fn read_index() -> Result<Index> {
    let file = index_file()?;
    let raw = match fs::symlink_metadata(&file) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return fail("session index must not be a symbolic link");
        }
        Ok(_) => match fs::read_to_string(&file) {
            Ok(raw) => raw,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Index::empty()),
            Err(error) => return fail(format!("read {}: {error}", file.display())),
        },
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Index::empty()),
        Err(error) => return fail(format!("read {}: {error}", file.display())),
    };
    // An index that is not valid, or is from another version, is rebuilt from the sources.
    Ok(serde_json::from_str::<Index>(&raw)
        .ok()
        .filter(|index| index.version == INDEX_VERSION)
        .unwrap_or_else(Index::empty))
}

fn ensure_index_root() -> Result<PathBuf> {
    files::ensure_root()?;
    let directory = files::index_root()?;
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return fail("session index directory must not be a symbolic link");
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .map_err(ctx(format!("create {}", directory.display())))?;
        }
        Err(error) => return fail(format!("stat {}: {error}", directory.display())),
    }
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
        .map_err(ctx(format!("chmod {}", directory.display())))?;
    Ok(directory)
}

fn write_index(index: &Index) -> Result<()> {
    let directory = ensure_index_root()?;
    let mut data = serde_json::to_string(index).map_err(ctx("serialize session index"))?;
    data.push('\n');
    let temporary = directory.join(format!(".index.{}.json", std::process::id()));
    files::write_private(
        &directory.join("index.json"),
        &temporary,
        data.as_bytes(),
        false,
    )
}

/// `path.resolve`: absolute, with `.` and `..` folded away and no symbolic links followed.
fn resolve_path(path: &str) -> String {
    let joined = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut resolved = PathBuf::from("/");
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(part) => resolved.push(part),
            _ => {}
        }
    }
    resolved.to_string_lossy().into_owned()
}

fn limit_option(options: &Options, key: &str, variable: &str) -> Result<u64> {
    let value = options.get(key).map(str::to_owned).or_else(|| {
        std::env::var(variable)
            .ok()
            .filter(|value| !value.is_empty())
    });
    let Some(value) = value else {
        return Ok(UNLIMITED);
    };
    match safe_integer(js_number(&value)) {
        Some(number) if number >= 1 => Ok(number as u64),
        _ => fail(format!("--{key} must be a positive integer")),
    }
}

fn timestamp_key(timestamp: &Value) -> String {
    match timestamp {
        Value::Number(number) => {
            let value = number.as_f64().unwrap_or_default();
            let seconds = if value < 1e12 { value } else { value / 1_000.0 };
            format!("{}", seconds.floor())
        }
        Value::String(text) => {
            clock::parse(text).map_or_else(|_| text.clone(), |seconds| seconds.to_string())
        }
        Value::Bool(true) => "true".into(),
        Value::Array(_) | Value::Object(_) => "[object Object]".into(),
        Value::Null | Value::Bool(false) => String::new(),
    }
}

fn event_key(turn: &Turn) -> Option<String> {
    let id = match &turn.event_id {
        Value::String(text) if !text.is_empty() => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(true) => "true".into(),
        _ => return None,
    };
    Some(format!(
        "{}:{id}",
        turn.session_id.as_deref().unwrap_or_default()
    ))
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Merges turns seen in several files: by event id, else by session, role, second and text.
fn deduplicate(turns: Vec<Turn>) -> Vec<Merged> {
    let mut by_event: HashMap<String, usize> = HashMap::new();
    let mut by_content: HashMap<String, usize> = HashMap::new();
    let mut unique: Vec<Merged> = Vec::new();
    for turn in turns {
        let event = event_key(&turn);
        let content = format!(
            "{}:{}:{}:{}",
            turn.session_id.as_deref().unwrap_or_default(),
            turn.role,
            timestamp_key(&turn.timestamp),
            turn.text
        );
        let existing = event
            .as_ref()
            .and_then(|key| by_event.get(key))
            .or_else(|| by_content.get(&content))
            .copied();
        let Some(position) = existing else {
            let position = unique.len();
            if let Some(key) = event {
                by_event.insert(key, position);
            }
            by_content.insert(content, position);
            unique.push(Merged {
                sources: vec![turn.source.clone()],
                turn,
            });
            continue;
        };
        let merged = &mut unique[position];
        if !merged.sources.contains(&turn.source) {
            merged.sources.push(turn.source.clone());
        }
        let inherited = merged.turn.inherited && turn.inherited;
        if base_name(&merged.turn.source) == "history.jsonl"
            && base_name(&turn.source) != "history.jsonl"
        {
            merged.turn = turn;
        }
        merged.turn.inherited = inherited;
    }
    unique
}

fn index_sessions(options: &Options, session: Option<&str>) -> Result<Indexed> {
    let directory = ensure_index_root()?;
    let _lock = Lock::acquire(
        directory.join(".lock"),
        Some(LOCK_WAIT),
        "session index is being updated; retry",
    )?;
    let mut index = if options.rebuild {
        Index::empty()
    } else {
        read_index()?
    };
    let max_files = limit_option(options, "max-files", "AGENT_TASK_MAX_FILES")?;
    let byte_limit = limit_option(options, "byte-limit", "AGENT_TASK_BYTE_LIMIT")?;
    let discovered = discover(max_files, session)?;
    let mut coverage = discovered.coverage;
    coverage.byte_limit = (byte_limit != UNLIMITED).then_some(byte_limit);
    let extra = options.get("file").map(resolve_path);
    let paths = unique(extra.into_iter().chain(discovered.files));
    let present: std::collections::HashSet<&String> = paths.iter().collect();
    let gone: Vec<String> = index
        .files
        .keys()
        .filter(|file| !present.contains(file) && !Path::new(file).exists())
        .cloned()
        .collect();
    for file in gone {
        index.files.shift_remove(&file);
        coverage.removed_files += 1;
    }
    for file in &paths {
        let metadata = match fs::symlink_metadata(file) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                index.files.shift_remove(file);
                coverage.unreadable.push(Unreadable {
                    source: file.clone(),
                    error: "not a regular non-symlink file".into(),
                });
                continue;
            }
            Ok(metadata) => metadata,
            Err(error) => {
                if error.kind() != ErrorKind::NotFound {
                    coverage.unreadable.push(Unreadable {
                        source: file.clone(),
                        error: error_code(&error),
                    });
                }
                continue;
            }
        };
        if coverage.scanned_bytes >= byte_limit {
            coverage.skipped_files += 1;
            coverage.byte_limit_reached = true;
            continue;
        }
        let stat = Stat::of(&metadata);
        let previous = index.files.get(file);
        if previous.is_some_and(|previous| {
            previous.dev == stat.dev
                && previous.ino == stat.ino
                && stat.size == previous.offset
                && stat.size == previous.size
                && stat.modified == previous.modified
                && stat.changed == previous.changed
        }) {
            coverage.indexed += 1;
            continue;
        }
        match scan_file(
            file,
            &stat,
            previous,
            byte_limit.saturating_sub(coverage.scanned_bytes),
        ) {
            Ok(scanned) => {
                coverage.scanned_bytes += scanned.bytes;
                if scanned.state.offset < stat.size {
                    coverage.byte_limit_reached = true;
                }
                index.files.insert(file.clone(), scanned.state);
            }
            Err(error) => {
                coverage.unreadable.push(Unreadable {
                    source: file.clone(),
                    error: error.0,
                });
                continue;
            }
        }
        coverage.indexed += 1;
    }
    coverage.malformed_lines = index.files.values().map(|item| item.malformed).sum();
    coverage.oversized_lines = index.files.values().map(|item| item.oversized).sum();
    let turns: Vec<Turn> = index
        .files
        .iter()
        .flat_map(|(file, item)| visible_turns(file, item))
        .collect();
    let turns = deduplicate(turns);
    coverage.indexed_turns = Some(turns.len());
    coverage.human_turns = Some(
        turns
            .iter()
            .filter(|merged| merged.turn.role == "user" && !merged.turn.inherited)
            .count(),
    );
    coverage.inherited_turns = Some(turns.iter().filter(|merged| merged.turn.inherited).count());
    let updated_at = clock::now_iso_millis()?;
    index.coverage = serde_json::to_value(&coverage).map_err(ctx("serialize coverage"))?;
    index.updated_at = Some(updated_at.clone());
    write_index(&index)?;
    Ok(Indexed {
        updated_at,
        coverage,
        turns,
    })
}

fn coverage_value(coverage: &Coverage) -> Result<Value> {
    serde_json::to_value(coverage).map_err(ctx("serialize coverage"))
}

pub fn index_command(options: &Options) -> Result<Output> {
    let indexed = index_sessions(options, options.get("session"))?;
    let mut report = serde_json::Map::new();
    report.insert("updatedAt".into(), json!(indexed.updated_at));
    if let Value::Object(coverage) = coverage_value(&indexed.coverage)? {
        report.extend(coverage);
    }
    pretty(&Value::Object(report))
}

fn matching<'a>(turns: &'a [Merged], session: &str) -> Vec<&'a Merged> {
    turns
        .iter()
        .filter(|merged| merged.turn.session_id.as_deref() == Some(session))
        .collect()
}

pub fn resolve(options: &Options) -> Result<Output> {
    let session = limited(
        options.get("session").or_else(|| options.id_argument()),
        "session",
    )?;
    let indexed = index_sessions(options, Some(&session))?;
    let turns = matching(&indexed.turns, &session);
    let Some(first) = turns.first() else {
        eprintln!(
            "{}",
            json!({ "coverage": coverage_value(&indexed.coverage)? })
        );
        return fail(format!("session {session} was not found"));
    };
    let sources = unique(turns.iter().flat_map(|merged| merged.sources.clone()));
    pretty(&json!({
        "sessionId": session,
        "host": first.turn.host,
        "sources": sources,
        "sourceType": first.turn.source_type,
        "turns": turns.len(),
        "coverage": coverage_value(&indexed.coverage)?,
    }))
}

pub fn search(options: &Options) -> Result<Output> {
    let query = limited(options.get("query"), "query")?.to_lowercase();
    let limit = options.get("limit").map_or(20.0, js_number);
    let limit = match safe_integer(limit) {
        Some(limit) if (1..=100).contains(&limit) => limit as usize,
        _ => return fail("--limit must be between 1 and 100"),
    };
    let indexed = index_sessions(options, options.get("session"))?;
    let turns: Vec<&Merged> = match options.get("session") {
        Some(session) => matching(&indexed.turns, session),
        None => indexed
            .turns
            .iter()
            .filter(|merged| !merged.turn.inherited)
            .collect(),
    };
    let mut results = Vec::new();
    for position in (0..turns.len()).rev() {
        if results.len() >= limit {
            break;
        }
        let merged = turns[position];
        let turn = &merged.turn;
        let lowered = turn.text.to_lowercase();
        if turn.role != "user" {
            continue;
        }
        let Some(found) = lowered.find(&query) else {
            continue;
        };
        let mut neighbours: Vec<usize> = (0..turns.len())
            .filter(|index| {
                turns[*index].turn.session_id == turn.session_id
                    && turns[*index].turn.source == turn.source
            })
            .collect();
        neighbours.sort_by_key(|index| turns[*index].turn.offset);
        let at = neighbours
            .iter()
            .position(|index| *index == position)
            .unwrap_or_default();
        let context: Vec<Value> = neighbours
            [at.saturating_sub(CONTEXT_TURNS)..(at + CONTEXT_TURNS + 1).min(neighbours.len())]
            .iter()
            .map(|index| {
                let item = &turns[*index].turn;
                json!({
                    "role": item.role,
                    "text": utf16_slice(&item.text, 0, 500),
                    "timestamp": item.timestamp,
                    "offset": item.offset,
                })
            })
            .collect();
        let start = utf16_offset(&lowered, found).saturating_sub(160);
        results.push(json!({
            "source": turn.source,
            "sources": merged.sources,
            "sourceType": turn.source_type,
            "host": turn.host,
            "sessionId": turn.session_id,
            "timestamp": turn.timestamp,
            "role": turn.role,
            "text": utf16_slice(&turn.text, start, start + 500),
            "offset": turn.offset,
            "inherited": turn.inherited,
            "context": context,
        }));
    }
    eprintln!(
        "{}",
        json!({ "coverage": coverage_value(&indexed.coverage)? })
    );
    pretty(&Value::Array(results))
}
