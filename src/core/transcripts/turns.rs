//! The human and assistant messages of a transcript, one JSON line at a time: Claude Code
//! records, prompt history lines and Codex rollouts, with Codex fork replays marked inherited.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::json::{get, head, text, truthy};
use crate::core::harness;
use crate::core::local::hostname;

const MAX_TEXT: usize = 16_384;
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

/// What reading a transcript up to some line has found, enough to continue with the next one.
#[derive(Clone, Default)]
pub struct ScanState {
    pub turns: Vec<Turn>,
    pub session_id: Option<String>,
    pub forked_from: Option<String>,
    pub history_start: Option<i64>,
    pub malformed: u64,
    pub oversized: u64,
}

fn non_empty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    Some(text(value, key)).filter(|text| !text.is_empty())
}

fn text_parts(content: &Value) -> Vec<String> {
    match content {
        Value::String(text) => vec![text.clone()],
        Value::Array(items) => items
            .iter()
            .filter(|item| matches!(text(item, "type"), "text" | "input_text" | "output_text"))
            .filter_map(|item| item.get("text").and_then(Value::as_str).map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

struct Message {
    role: String,
    text: String,
    session_id: Option<String>,
}

fn chat_role(message: &Value) -> Option<&str> {
    Some(text(message, "role")).filter(|role| matches!(*role, "user" | "assistant"))
}

fn transcript_message(entry: &Value, session_id: Option<&str>) -> Option<Message> {
    let fallback = |own: Option<&str>| own.or(session_id).map(str::to_owned);
    // Prompt history lines: Codex's `{session_id, text}` and Claude Code's `{sessionId, display}`.
    for (body, id) in [("text", "session_id"), ("display", "sessionId")] {
        if let (Some(text), Some(own)) = (
            entry.get(body).and_then(Value::as_str),
            non_empty(entry, id),
        ) {
            return Some(Message {
                role: "user".into(),
                text: text.to_owned(),
                session_id: Some(own.to_owned()),
            });
        }
    }
    let kind = text(entry, "type");
    if matches!(kind, "user" | "assistant") && !truthy(get(entry, "isMeta")) {
        let message = get(entry, "message");
        let role = chat_role(message)?;
        return Some(Message {
            role: role.to_owned(),
            text: text_parts(get(message, "content")).join("\n"),
            session_id: fallback(non_empty(entry, "sessionId")),
        });
    }
    if kind == "response_item" {
        let payload = entry.get("payload")?;
        if text(payload, "type") != "message" {
            return None;
        }
        let role = chat_role(payload)?;
        return Some(Message {
            role: role.to_owned(),
            text: text_parts(get(payload, "content")).join("\n"),
            session_id: fallback(non_empty(entry, "session_id")),
        });
    }
    None
}

/// Whether the harness, not the person, wrote the message.
fn is_injected(message: &Message, entry: &Value) -> bool {
    let set = |key: &str| truthy(get(entry, key));
    if set("isMeta") || set("isCompactSummary") || text(entry, "type") == "summary" {
        return true;
    }
    if message.role != "user" {
        return false;
    }
    let text = message.text.trim_start();
    text.starts_with("# AGENTS.md instructions")
        || text.starts_with("<environment_context>")
        || text.starts_with(harness::SKILLS_INSTRUCTIONS_TAG)
        || text.starts_with("<system")
        || text.starts_with("<developer")
}

fn source_kind(file: &str) -> &'static str {
    if file.contains(harness::CODEX_PATH) {
        "codex"
    } else {
        "claude"
    }
}

fn first_truthy(entry: &Value, keys: &[&str]) -> Value {
    keys.iter()
        .map(|key| get(entry, key))
        .find(|value| truthy(value))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Reads one JSON line of `file`, which starts at byte `offset`, into `state`. A line that is
/// not JSON counts as malformed.
pub fn parse_line(line: &[u8], state: &mut ScanState, file: &str, offset: u64) {
    match serde_json::from_str::<Value>(&String::from_utf8_lossy(line)) {
        Ok(Value::Null) | Err(_) => state.malformed += 1,
        Ok(entry) => read_entry(&entry, line, state, file, offset),
    }
}

fn read_entry(entry: &Value, line: &[u8], state: &mut ScanState, file: &str, offset: u64) {
    if text(entry, "type") == "session_meta" {
        let payload = get(entry, "payload");
        if let Some(id) = payload.get("id").and_then(Value::as_str) {
            state.session_id = Some(id.to_owned());
        }
        state.forked_from = non_empty(payload, "forked_from_id")
            .or_else(|| non_empty(payload, "parent_thread_id"))
            .map(str::to_owned);
        state.history_start = payload
            .get("subagent_history_start_ordinal")
            .and_then(Value::as_i64);
    }
    for key in ["sessionId", "session_id"] {
        if let Some(id) = entry.get(key).and_then(Value::as_str) {
            state.session_id = Some(id.to_owned());
        }
    }
    let Some(message) = transcript_message(entry, state.session_id.as_deref()) else {
        return;
    };
    if message.text.is_empty() || is_injected(&message, entry) {
        return;
    }
    let ordinal = entry.get("ordinal").and_then(Value::as_i64);
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
        host: hostname(),
        session_id: message.session_id.or_else(|| state.session_id.clone()),
        timestamp: first_truthy(entry, &["ts", "timestamp"]),
        role: message.role,
        text: head(&message.text, maximum).to_owned(),
        offset,
        line_bytes: line.len() as u64 + 1,
        event_id: first_truthy(entry, &["uuid", "id"]),
        inherited,
    });
}
