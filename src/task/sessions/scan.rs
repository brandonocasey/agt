//! Incremental scanning of one transcript file into the turns the index stores.

use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::core::error::{Result, ctx};
use crate::task::files;
use crate::task::text::{MAX_TEXT, safe_integer, utf16_slice};

pub const READ_CHUNK: usize = 64 * 1024;
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
const PREFIX_BYTES: u64 = 4_096;
const ASSISTANT_TEXT: usize = 500;

/// One human or assistant message found in a transcript.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Turn {
    pub source: String,
    pub source_type: String,
    pub host: String,
    pub session_id: Option<String>,
    pub timestamp: Value,
    pub role: String,
    pub text: String,
    pub offset: u64,
    pub line_bytes: u64,
    pub event_id: Value,
    pub inherited: bool,
}

/// What the index remembers about one transcript file, enough to continue after the bytes it
/// has already read.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FileState {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub offset: u64,
    pub session_id: Option<String>,
    pub forked_from: Option<String>,
    pub history_start: Option<i64>,
    pub malformed: u64,
    pub oversized: u64,
    pub skip_line: bool,
    pub partial: Option<String>,
    pub prefix: String,
    pub tail_hash: String,
    pub modified: f64,
    pub changed: f64,
    pub turns: Vec<Turn>,
}

#[derive(Clone, Default)]
pub struct ScanState {
    pub turns: Vec<Turn>,
    pub session_id: Option<String>,
    pub forked_from: Option<String>,
    pub history_start: Option<i64>,
    pub malformed: u64,
    pub oversized: u64,
}

/// A file's identity and times as `agent-task` records them (milliseconds as a float).
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub modified: f64,
    pub changed: f64,
}

impl Stat {
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        let millis =
            |seconds: i64, nanos: i64| seconds as f64 * 1000.0 + nanos as f64 / 1_000_000.0;
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.len(),
            modified: millis(metadata.mtime(), metadata.mtime_nsec()),
            changed: millis(metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

fn text_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn non_empty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    text_field(value, key).filter(|text| !text.is_empty())
}

fn text_parts(content: Option<&Value>) -> Vec<String> {
    match content {
        Some(Value::String(text)) => vec![text.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| {
                matches!(
                    text_field(item, "type"),
                    Some("text" | "input_text" | "output_text")
                )
            })
            .filter_map(|item| text_field(item, "text").map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

struct Message {
    role: String,
    text: String,
    session_id: Option<String>,
}

fn chat_role(message: Option<&Value>) -> Option<&str> {
    message
        .and_then(|message| text_field(message, "role"))
        .filter(|role| matches!(*role, "user" | "assistant"))
}

fn transcript_message(entry: &Value, session_id: Option<&str>) -> Option<Message> {
    let fallback = |own: Option<&str>| own.or(session_id).map(str::to_owned);
    if let (Some(text), Some(own)) = (text_field(entry, "text"), non_empty(entry, "session_id")) {
        return Some(Message {
            role: "user".into(),
            text: text.to_owned(),
            session_id: Some(own.to_owned()),
        });
    }
    if let (Some(text), Some(own)) = (text_field(entry, "display"), non_empty(entry, "sessionId")) {
        return Some(Message {
            role: "user".into(),
            text: text.to_owned(),
            session_id: Some(own.to_owned()),
        });
    }
    let kind = text_field(entry, "type");
    if matches!(kind, Some("user" | "assistant")) && !truthy(entry.get("isMeta")) {
        let message = entry.get("message");
        let role = chat_role(message)?;
        let text = text_parts(message.and_then(|message| message.get("content"))).join("\n");
        return Some(Message {
            role: role.to_owned(),
            text,
            session_id: fallback(non_empty(entry, "sessionId")),
        });
    }
    if kind == Some("response_item") {
        let payload = entry.get("payload")?;
        if text_field(payload, "type") != Some("message") {
            return None;
        }
        let role = chat_role(Some(payload))?;
        return Some(Message {
            role: role.to_owned(),
            text: text_parts(payload.get("content")).join("\n"),
            session_id: fallback(non_empty(entry, "session_id")),
        });
    }
    None
}

fn is_injected(message: &Message, entry: &Value) -> bool {
    if truthy(entry.get("isMeta"))
        || truthy(entry.get("isCompactSummary"))
        || text_field(entry, "type") == Some("summary")
    {
        return true;
    }
    if message.role != "user" {
        return false;
    }
    let text = message.text.trim_start();
    text.starts_with("# AGENTS.md instructions")
        || text.starts_with("<environment_context>")
        || text.starts_with("<skills_instructions>")
        || text.starts_with("<system")
        || text.starts_with("<developer")
}

fn source_kind(file: &str) -> &'static str {
    if file.contains("/.codex/") {
        "codex"
    } else {
        "claude"
    }
}

fn first_truthy(entry: &Value, keys: &[&str]) -> Value {
    keys.iter()
        .find_map(|key| entry.get(key).filter(|value| truthy(Some(value))))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Reads one JSON line into `state`, counting it as malformed when it is not JSON.
pub fn parse_line(line: &[u8], state: &mut ScanState, file: &str, offset: u64) {
    let parsed = serde_json::from_str::<Value>(&String::from_utf8_lossy(line));
    match parsed {
        Ok(Value::Null) | Err(_) => state.malformed += 1,
        Ok(entry) => read_entry(&entry, line, state, file, offset),
    }
}

fn read_entry(entry: &Value, line: &[u8], state: &mut ScanState, file: &str, offset: u64) {
    if text_field(entry, "type") == Some("session_meta") {
        let payload = entry.get("payload");
        let payload_field = |key: &str| payload.and_then(|payload| non_empty(payload, key));
        if let Some(id) = payload.and_then(|payload| text_field(payload, "id")) {
            state.session_id = Some(id.to_owned());
        }
        state.forked_from = payload_field("forked_from_id")
            .or_else(|| payload_field("parent_thread_id"))
            .map(str::to_owned);
        state.history_start = payload
            .and_then(|payload| payload.get("subagent_history_start_ordinal"))
            .and_then(Value::as_f64)
            .and_then(safe_integer);
    }
    for key in ["sessionId", "session_id"] {
        if let Some(id) = text_field(entry, key) {
            state.session_id = Some(id.to_owned());
        }
    }
    let Some(message) = transcript_message(entry, state.session_id.as_deref()) else {
        return;
    };
    if message.text.is_empty() || is_injected(&message, entry) {
        return;
    }
    let ordinal = entry
        .get("ordinal")
        .and_then(Value::as_f64)
        .and_then(safe_integer);
    let inherited = state.forked_from.is_some()
        && state
            .history_start
            .is_some_and(|start| ordinal.is_some_and(|ordinal| ordinal < start));
    let maximum = if message.role == "user" {
        MAX_TEXT
    } else {
        ASSISTANT_TEXT
    };
    state.turns.push(Turn {
        source: file.to_owned(),
        source_type: source_kind(file).to_owned(),
        host: files::hostname(),
        session_id: message.session_id.or_else(|| state.session_id.clone()),
        timestamp: first_truthy(entry, &["ts", "timestamp"]),
        role: message.role,
        text: utf16_slice(&message.text, 0, maximum),
        offset,
        line_bytes: line.len() as u64 + 1,
        event_id: first_truthy(entry, &["uuid", "id"]),
        inherited,
    });
}

/// Reads up to `length` bytes at `position`, fewer only at the end of the file.
fn read_at(file: &File, position: u64, length: usize) -> Result<Vec<u8>> {
    let mut buffer = vec![0; length];
    let mut filled = 0;
    while filled < length {
        let count = file
            .read_at(&mut buffer[filled..], position + filled as u64)
            .map_err(ctx("read transcript"))?;
        if count == 0 {
            break;
        }
        filled += count;
    }
    buffer.truncate(filled);
    Ok(buffer)
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn file_prefix(file: &File, size: u64) -> Result<String> {
    let length = usize::try_from(size.min(PREFIX_BYTES)).unwrap_or_default();
    Ok(digest(&read_at(file, 0, length)?))
}

fn tail_hash(file: &File, end: u64) -> Result<String> {
    let length = end.min(READ_CHUNK as u64);
    Ok(digest(&read_at(
        file,
        end - length,
        usize::try_from(length).unwrap_or_default(),
    )?))
}

/// Whether `previous` describes the start of `stat`'s file, so scanning can continue from its
/// offset instead of starting over.
fn can_append(file: &File, stat: &Stat, previous: Option<&FileState>) -> Result<bool> {
    let Some(previous) = previous else {
        return Ok(false);
    };
    Ok(previous.dev == stat.dev
        && previous.ino == stat.ino
        && stat.size >= previous.size
        && previous.prefix == file_prefix(file, previous.size)?
        && previous.tail_hash == tail_hash(file, previous.offset)?
        && (stat.size > previous.size
            || (stat.modified == previous.modified && stat.changed == previous.changed)))
}

pub struct Scanned {
    pub state: FileState,
    pub bytes: u64,
}

/// Scans `path` from where `previous` stopped, reading at most `budget` bytes.
pub fn scan_file(
    path: &str,
    stat: &Stat,
    previous: Option<&FileState>,
    budget: u64,
) -> Result<Scanned> {
    let file = File::open(path).map_err(ctx(format!("open {path}")))?;
    let prefix = file_prefix(&file, stat.size)?;
    let append = can_append(&file, stat, previous)?;
    let (mut state, start_offset, mut pending, mut skipping) = match previous.filter(|_| append) {
        Some(previous) => (
            ScanState {
                turns: previous.turns.clone(),
                session_id: previous.session_id.clone(),
                forked_from: previous.forked_from.clone(),
                history_start: previous.history_start,
                malformed: previous.malformed,
                oversized: previous.oversized,
            },
            previous.offset,
            previous
                .partial
                .as_deref()
                .and_then(|partial| STANDARD.decode(partial).ok())
                .unwrap_or_default(),
            previous.skip_line,
        ),
        None => (ScanState::default(), 0, Vec::new(), false),
    };
    let mut read_bytes: u64 = 0;
    while start_offset + read_bytes < stat.size && read_bytes < budget {
        let length = (READ_CHUNK as u64)
            .min(stat.size - start_offset - read_bytes)
            .min(budget - read_bytes);
        let chunk = read_at(
            &file,
            start_offset + read_bytes,
            usize::try_from(length).unwrap_or_default(),
        )?;
        if chunk.is_empty() {
            break;
        }
        read_bytes += chunk.len() as u64;
        let mut start = 0;
        for position in 0..chunk.len() {
            if chunk[position] != b'\n' {
                continue;
            }
            let fragment = &chunk[start..position];
            let line_offset = start_offset + read_bytes - chunk.len() as u64 + start as u64
                - pending.len() as u64;
            if skipping {
                skipping = false;
            } else if pending.is_empty() {
                parse_line(fragment, &mut state, path, line_offset);
            } else {
                pending.extend_from_slice(fragment);
                parse_line(&pending, &mut state, path, line_offset);
            }
            pending.clear();
            start = position + 1;
        }
        if start < chunk.len() && !skipping {
            let fragment = &chunk[start..];
            if pending.len() + fragment.len() > MAX_LINE_BYTES {
                state.oversized += 1;
                pending.clear();
                skipping = true;
            } else {
                pending.extend_from_slice(fragment);
            }
        }
    }
    let offset = start_offset + read_bytes;
    let result = FileState {
        dev: stat.dev,
        ino: stat.ino,
        size: stat.size,
        offset,
        session_id: state.session_id,
        forked_from: state.forked_from,
        history_start: state.history_start,
        malformed: state.malformed,
        oversized: state.oversized,
        skip_line: skipping,
        partial: (!pending.is_empty()).then(|| STANDARD.encode(&pending)),
        prefix,
        tail_hash: tail_hash(&file, offset)?,
        modified: stat.modified,
        changed: stat.changed,
        turns: state.turns,
    };
    Ok(Scanned {
        state: result,
        bytes: read_bytes,
    })
}

/// The turns of `state` plus the final line when it is complete JSON without a newline.
pub fn visible_turns(file: &str, state: &FileState) -> Vec<Turn> {
    let mut turns = state.turns.clone();
    let Some(partial) = state.partial.as_deref() else {
        return turns;
    };
    if state.skip_line || state.offset != state.size {
        return turns;
    }
    let Ok(pending) = STANDARD.decode(partial) else {
        return turns;
    };
    if serde_json::from_str::<Value>(&String::from_utf8_lossy(&pending)).is_err() {
        return turns;
    }
    let mut view = ScanState {
        turns: Vec::new(),
        session_id: state.session_id.clone(),
        forked_from: state.forked_from.clone(),
        history_start: state.history_start,
        malformed: state.malformed,
        oversized: state.oversized,
    };
    parse_line(
        &pending,
        &mut view,
        file,
        state.offset - pending.len() as u64,
    );
    for mut turn in view.turns {
        turn.line_bytes = pending.len() as u64;
        turns.push(turn);
    }
    turns
}
