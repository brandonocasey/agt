//! Codex rollout files read as Claude Code records, so every report runs on both.
//!
//! A Codex `token_count` event follows the tool calls and outputs of the response it bills, so
//! calls and everything after them wait until it arrives: the response then comes first, as a
//! Claude Code assistant record holding its `tool_use` blocks, then the waiting records.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use crate::sessions::py::{count, get, text};

/// Who wrote a rollout file, from its first line.
pub struct Meta {
    pub cwd: String,
    pub session: String,
    pub agent: String,
}

/// The `session_meta` line, or `None` when `first` is not one.
pub fn meta(first: &Value) -> Option<Meta> {
    if text(first, "type") != "session_meta" {
        return None;
    }
    let payload = get(first, "payload");
    let subagent = get(get(payload, "source"), "subagent");
    let spawn = get(subagent, "thread_spawn");
    let (session, agent) = if spawn.is_object() {
        // A spawn without a role runs Codex's default agent.
        let role = text(spawn, "agent_role");
        (
            text(spawn, "parent_thread_id"),
            if role.is_empty() { "default" } else { role },
        )
    } else {
        // Built-in subagents, such as reviews, name their kind as a string.
        let kind = subagent
            .as_str()
            .or_else(|| get(subagent, "other").as_str());
        (text(payload, "id"), kind.unwrap_or("main"))
    };
    Some(Meta {
        cwd: text(payload, "cwd").to_owned(),
        session: session.chars().take(8).collect(),
        agent: agent.to_owned(),
    })
}

/// A working directory as Claude Code names its project directory.
pub fn project(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

static EXEC_COMMAND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\bcmd"?\s*:\s*("(?:[^"\\]|\\.)*")"#).expect("valid pattern"));

/// A shell command from a call's arguments, under any of the names Codex has used.
fn command(name: &str, raw: &str, arguments: &Value) -> Option<String> {
    match name {
        "exec_command" => get(arguments, "cmd").as_str().map(str::to_owned),
        "shell" | "local_shell" | "shell_command" => match get(arguments, "command") {
            Value::String(command) => Some(command.clone()),
            Value::Array(parts) => Some(
                parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        },
        "exec" => EXEC_COMMAND.captures(raw).map(|found| {
            let quoted = &found[1];
            serde_json::from_str(quoted).unwrap_or_else(|_| quoted[1..quoted.len() - 1].to_owned())
        }),
        _ => None,
    }
}

/// A Claude Code `tool_use` block for a Codex call: shell commands become `Bash`.
fn tool_use(payload: &Value) -> Value {
    let name = text(payload, "name");
    let raw = get(payload, "input");
    let raw = if raw.is_null() {
        get(payload, "arguments")
    } else {
        raw
    };
    let raw_text = raw.as_str().unwrap_or_default();
    let arguments = match raw {
        Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| raw.clone()),
        other => other.clone(),
    };
    let id = get(payload, "call_id").clone();
    if let Some(command) = command(name, raw_text, &arguments) {
        return json!({"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command}});
    }
    let input = if arguments.is_object() {
        arguments
    } else {
        json!({ "input": arguments })
    };
    json!({"type": "tool_use", "id": id, "name": name, "input": input})
}

/// The text of a call's output and whether the call failed.
fn output(payload: &Value) -> (Value, bool) {
    let content = match get(payload, "output") {
        Value::String(raw) => {
            // Older rollouts wrap shell output in JSON with the exit code alongside.
            if let Ok(wrapped) = serde_json::from_str::<Value>(raw)
                && wrapped.get("output").is_some_and(Value::is_string)
            {
                let code = count(get(&wrapped, "metadata"), "exit_code");
                return (get(&wrapped, "output").clone(), code != 0);
            }
            Value::String(raw.clone())
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| match text(item, "type") {
                    "input_image" => json!({"type": "image"}),
                    _ => json!({"type": "text", "text": text(item, "text")}),
                })
                .collect(),
        ),
        other => other.clone(),
    };
    let first = match &content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(|item| text(item, "text")).collect(),
        _ => String::new(),
    };
    let first = first.lines().next().unwrap_or_default();
    let failed = first.starts_with("Script failed")
        || first.starts_with("aborted by user")
        || first.contains(" failed: ");
    (content, failed)
}

/// What the person typed, without the instructions and context Codex injects.
fn user_text(payload: &Value) -> Option<String> {
    let parts: Vec<&str> = get(payload, "content")
        .as_array()?
        .iter()
        .map(|item| text(item, "text"))
        .filter(|text| !text.starts_with('<') && !text.starts_with("# AGENTS.md instructions"))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Turns Codex records into Claude Code records for one file.
pub struct Adapter {
    id: String,
    model: String,
    calls: usize,
    total: Option<Value>,
    started: Option<Value>,
    tools: Vec<Value>,
    waiting: Vec<Value>,
}

impl Adapter {
    pub fn new(id: String) -> Self {
        Self {
            id,
            model: "?".into(),
            calls: 0,
            total: None,
            started: None,
            tools: Vec::new(),
            waiting: Vec::new(),
        }
    }

    fn send(&mut self, record: Value, out: &mut impl FnMut(&Value)) {
        if self.tools.is_empty() {
            out(&record);
        } else {
            self.waiting.push(record);
        }
    }

    fn flush(&mut self, message: Value, timestamp: Value, out: &mut impl FnMut(&Value)) {
        out(&json!({"type": "assistant", "timestamp": timestamp, "message": message}));
        self.tools.clear();
        self.started = None;
        for record in std::mem::take(&mut self.waiting) {
            out(&record);
        }
    }

    pub fn feed(&mut self, record: &Value, out: &mut impl FnMut(&Value)) {
        let payload = get(record, "payload");
        let timestamp = get(record, "timestamp").clone();
        match (text(record, "type"), text(payload, "type")) {
            ("turn_context", _) => {
                if let Some(model) = payload.get("model").and_then(Value::as_str) {
                    self.model = model.to_owned();
                }
            }
            ("compacted", _) => self.send(
                json!({"type": "system", "subtype": "compact_boundary", "timestamp": timestamp}),
                out,
            ),
            ("response_item", "message") if text(payload, "role") == "user" => {
                if let Some(content) = user_text(payload) {
                    self.send(
                        json!({"type": "user", "timestamp": timestamp,
                               "message": {"role": "user", "content": content}}),
                        out,
                    );
                }
            }
            ("response_item", "function_call" | "custom_tool_call" | "local_shell_call") => {
                self.started.get_or_insert(timestamp);
                self.tools.push(tool_use(payload));
            }
            (
                "response_item",
                "function_call_output" | "custom_tool_call_output" | "local_shell_call_output",
            ) => {
                let (content, failed) = output(payload);
                self.send(
                    json!({"type": "user", "timestamp": timestamp, "message": {"role": "user",
                        "content": [{"type": "tool_result", "tool_use_id": get(payload, "call_id"),
                                     "content": content, "is_error": failed}]}}),
                    out,
                );
            }
            ("response_item", _) => {
                self.started.get_or_insert(timestamp);
            }
            ("event_msg", "token_count") => self.bill(payload, timestamp, out),
            _ => {}
        }
    }

    fn bill(&mut self, payload: &Value, timestamp: Value, out: &mut impl FnMut(&Value)) {
        let info = get(payload, "info");
        let total = get(info, "total_token_usage");
        if !info.is_object() || self.total.as_ref() == Some(total) {
            return;
        }
        self.total = Some(total.clone());
        let last = get(info, "last_token_usage");
        let cached = count(last, "cached_input_tokens");
        let written = count(last, "cache_write_input_tokens");
        let usage = json!({
            "input_tokens": (count(last, "input_tokens") - cached - written).max(0),
            "cache_creation_input_tokens": written,
            "cache_read_input_tokens": cached,
            "output_tokens": count(last, "output_tokens"),
        });
        self.calls += 1;
        let message = json!({
            "id": format!("codex:{}:{}", self.id, self.calls),
            "model": self.model,
            "usage": usage,
            "content": self.tools,
        });
        let at = self.started.take().unwrap_or(timestamp);
        self.flush(message, at, out);
    }

    /// Sends calls no `token_count` billed, then what waited on them.
    pub fn finish(&mut self, out: &mut impl FnMut(&Value)) {
        if self.tools.is_empty() {
            return;
        }
        let at = self.started.take().unwrap_or(Value::Null);
        let message = json!({"model": self.model, "content": self.tools});
        self.flush(message, at, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lines: &[Value]) -> Vec<Value> {
        let mut adapter = Adapter::new("f".into());
        let mut records = Vec::new();
        for line in lines {
            adapter.feed(line, &mut |record| records.push(record.clone()));
        }
        adapter.finish(&mut |record| records.push(record.clone()));
        records
    }

    #[test]
    fn a_billed_call_comes_before_its_output() {
        let usage = json!({"input_tokens": 100, "cached_input_tokens": 60,
                           "cache_write_input_tokens": 0, "output_tokens": 7});
        let records = run(&[
            json!({"type": "turn_context", "payload": {"model": "gpt-x"}}),
            json!({"type": "response_item", "timestamp": "t1", "payload": {"type": "custom_tool_call",
                   "call_id": "c1", "name": "exec",
                   "input": "await tools.exec_command({cmd:\"cd x && ls -la\"})"}}),
            json!({"type": "response_item", "timestamp": "t2", "payload": {
                   "type": "custom_tool_call_output", "call_id": "c1",
                   "output": [{"type": "input_text", "text": "Script failed\nboom"}]}}),
            json!({"type": "event_msg", "timestamp": "t3", "payload": {"type": "token_count",
                   "info": {"total_token_usage": usage, "last_token_usage": usage}}}),
            json!({"type": "event_msg", "timestamp": "t4", "payload": {"type": "token_count",
                   "info": {"total_token_usage": usage, "last_token_usage": usage}}}),
        ]);
        assert_eq!(records.len(), 2);
        let message = &records[0]["message"];
        assert_eq!(records[0]["timestamp"], "t1");
        assert_eq!(message["model"], "gpt-x");
        assert_eq!(message["usage"]["input_tokens"], 40);
        assert_eq!(message["usage"]["cache_read_input_tokens"], 60);
        assert_eq!(message["content"][0]["name"], "Bash");
        assert_eq!(message["content"][0]["input"]["command"], "cd x && ls -la");
        let result = &records[1]["message"]["content"][0];
        assert_eq!(result["tool_use_id"], "c1");
        assert_eq!(result["is_error"], true);
    }

    #[test]
    fn injected_context_is_not_a_user_message() {
        let records = run(&[
            json!({"type": "response_item", "payload": {"type": "message",
            "role": "user", "content": [
                {"type": "input_text", "text": "# AGENTS.md instructions\n..."},
                {"type": "input_text", "text": "<environment_context>"}]}}),
        ]);
        assert!(records.is_empty());
    }

    #[test]
    fn subagents_take_their_role_and_parent_session() {
        let first = json!({"type": "session_meta", "payload": {"id": "abcdef123456", "cwd": "/w/x.y",
            "source": {"subagent": {"thread_spawn": {"parent_thread_id": "99999999zzz",
                                                     "agent_role": "explorer"}}}}});
        let meta = meta(&first).expect("session meta");
        assert_eq!(meta.agent, "explorer");
        assert_eq!(meta.session, "99999999");
        assert_eq!(project(&meta.cwd), "-w-x-y");
    }
}
