//! `hooks`: the context that hooks add to the model, by event and command.
//!
//! Claude Code writes hook output as `attachment` records, and a blocking `PreToolUse` hook or a
//! `Stop` hook's feedback as user records (see `blocked`). Only some kinds reach the model:
//! `hook_success` output of the events that add stdout to the prompt, `hook_additional_context`
//! and `hook_blocking_error`. The rest (`hook_cancelled`, `hook_non_blocking_error`,
//! `hook_system_message`, `hook_stopped_continuation`, and the `stdout` of tool hooks) only show
//! to the person. Codex rollouts record no hook output.

use std::collections::HashSet;
use std::path::Path;

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::core::harness::Harness;
use crate::core::transcripts::json::{get, head, text};
use crate::sessions::fmt::{self, tok};
use crate::sessions::source::Loaded;
use crate::sessions::tools::est_text_tokens;
use crate::sessions::{Scope, Which};

/// Events whose `hook_success` stdout Claude Code adds to the model's context.
const CONTEXT_EVENTS: [&str; 4] = ["SessionStart", "UserPromptSubmit", "Setup", "SubagentStart"];

/// Events that fire once per tool call.
const TOOL_EVENTS: [&str; 4] = [
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
];

/// Estimated tokens of the text a hook attachment carries to the model, or `None` when the
/// attachment kind does not reach the model.
fn model_tokens(kind: &str, event: &str, attachment: &Value) -> Option<f64> {
    let body = match kind {
        "hook_success" if CONTEXT_EVENTS.contains(&event) => {
            let content = text(attachment, "content");
            if content.is_empty() {
                return None;
            }
            content.to_owned()
        }
        "hook_additional_context" => match get(attachment, "content") {
            Value::Array(parts) => parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
            Value::String(part) => part.clone(),
            _ => return None,
        },
        "hook_blocking_error" => {
            let error = get(get(attachment, "blockingError"), "blockingError");
            error
                .as_str()
                .unwrap_or_else(|| text(attachment, "stderr"))
                .to_owned()
        }
        _ => return None,
    };
    Some(est_text_tokens(&body))
}

fn home_relative(command: &str, home: &Path) -> String {
    let home = home.to_string_lossy();
    if home.len() > 1 {
        command.replace(home.as_ref(), "~")
    } else {
        command.to_owned()
    }
}

/// A compact record for a hook attachment: what ran and how many tokens it gave the model.
/// `None` for any other attachment. `uuid` is the attachment record's, which resumed sessions copy.
pub fn summary(attachment: &Value, uuid: &Value, home: &Path) -> Option<Value> {
    let kind = text(attachment, "type");
    if !kind.starts_with("hook_") {
        return None;
    }
    let event = text(attachment, "hookEvent");
    let command = [text(attachment, "command"), text(attachment, "hookName")]
        .into_iter()
        .find(|name| !name.is_empty())
        .unwrap_or("?");
    let tokens = model_tokens(kind, event, attachment);
    Some(json!({
        "type": "hook",
        "hook": {
            "kind": kind,
            "event": event,
            "command": home_relative(command, home),
            "tokens": tokens,
            "uuid": uuid,
        },
    }))
}

/// The hook output a user record carries to the model, as the same compact records `summary`
/// makes. Claude Code records a blocking `PreToolUse` hook as `tool_result` text starting
/// `PreToolUse:<Tool> hook error: [<command>]:`, and a `Stop` hook's feedback as an `isMeta` user
/// text starting `Stop hook feedback:` (reported under that name, since a prompt hook names itself with its prompt); neither is an attachment.
pub fn blocked(record: &Value, home: &Path) -> Vec<Value> {
    if text(record, "type") != "user" {
        return Vec::new();
    }
    let uuid = text(record, "uuid");
    let content = get(get(record, "message"), "content");
    let mut found = Vec::new();
    if get(record, "isMeta") == true
        && content
            .as_str()
            .is_some_and(|body| body.starts_with("Stop hook feedback:"))
    {
        let whole = content.as_str().unwrap_or_default();
        // A prompt-based Stop hook names itself with its own prompt text, which is not printed.
        let mut hook = blocked_hook("hook_stop_feedback", "Stop", whole, "", uuid, home);
        hook["hook"]["command"] = "Stop hook feedback".into();
        found.push(hook);
    }
    for block in content.as_array().into_iter().flatten() {
        if text(block, "type") != "tool_result" {
            continue;
        }
        let result = match get(block, "content") {
            Value::String(result) => result.as_str(),
            Value::Array(parts) => parts.first().map_or("", |part| text(part, "text")),
            _ => "",
        };
        let Some((tool, body)) = result
            .strip_prefix("PreToolUse:")
            .and_then(|rest| rest.split_once(" hook error:"))
        else {
            continue;
        };
        let id = format!("{uuid}:{}", text(block, "tool_use_id"));
        let mut hook = blocked_hook("hook_blocking_error", "PreToolUse", result, body, &id, home);
        if hook["hook"]["command"] == "?" {
            hook["hook"]["command"] = format!("PreToolUse:{tool}").into();
        }
        found.push(hook);
    }
    found
}

/// A hook record for `whole` text whose part after the hook's prefix is ` [<command>]: <message>`.
fn blocked_hook(
    kind: &str,
    event: &str,
    whole: &str,
    body: &str,
    uuid: &str,
    home: &Path,
) -> Value {
    let command = body
        .trim_start()
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("]:"))
        .map_or("?", |(command, _)| command);
    json!({
        "type": "hook",
        "hook": {
            "kind": kind,
            "event": event,
            "command": home_relative(command, home),
            "tokens": est_text_tokens(whole),
            "uuid": uuid,
        },
    })
}

#[derive(Default)]
struct Row {
    fires: i64,
    to_model: i64,
    total: f64,
    max: f64,
}

pub struct Hooks {
    rows: IndexMap<(String, String), Row>,
    seen: HashSet<String>,
}

impl Hooks {
    pub fn new() -> Self {
        Self {
            rows: IndexMap::new(),
            seen: HashSet::new(),
        }
    }

    pub fn feed(&mut self, transcript: &Loaded) {
        if transcript.harness != Harness::Claude {
            return;
        }
        transcript.records(|record| {
            if text(record, "type") == "hook" {
                self.count(get(record, "hook"));
            }
            for hook in get(record, "hooks").as_array().into_iter().flatten() {
                self.count(get(hook, "hook"));
            }
        });
    }

    fn count(&mut self, hook: &Value) {
        // Resumed and forked sessions copy earlier records, uuid included.
        let uuid = text(hook, "uuid");
        if !uuid.is_empty() && !self.seen.insert(uuid.to_owned()) {
            return;
        }
        let row = self
            .rows
            .entry((
                text(hook, "event").to_owned(),
                text(hook, "command").to_owned(),
            ))
            .or_default();
        row.fires += 1;
        if let Some(tokens) = get(hook, "tokens").as_f64() {
            row.to_model += 1;
            row.total += tokens;
            row.max = row.max.max(tokens);
        }
    }

    pub fn render(self, scope: &Scope) -> String {
        let mut out = String::new();
        if scope.harness != Which::Codex {
            out.push_str(&self.render_claude(scope));
        }
        if scope.harness != Which::Claude {
            out.push_str("codex: rollouts record no hook output, so no hook context is reported\n");
        }
        out
    }

    fn render_claude(self, scope: &Scope) -> String {
        let mut rows: Vec<_> = self.rows.into_iter().collect();
        rows.sort_by(|a, b| {
            b.1.total
                .total_cmp(&a.1.total)
                .then(b.1.fires.cmp(&a.1.fires))
        });
        let mut out = format!("claude: hook outputs since {}\n", scope.since);
        if rows.is_empty() {
            out.push_str("no hook outputs\n");
            return out;
        }
        let model_total: f64 = rows.iter().map(|(_, row)| row.total).sum();
        out.push_str(&format!(
            "{} est tokens reached the model from {} hook commands; tokens count only the kinds that reach it\n",
            tok(model_total),
            rows.iter().filter(|(_, row)| row.to_model > 0).count(),
        ));
        let table: Vec<Vec<String>> = rows
            .iter()
            .take(scope.limit)
            .map(|((event, command), row)| {
                let avg = if row.to_model == 0 {
                    0.0
                } else {
                    row.total / row.to_model as f64
                };
                vec![
                    event.clone(),
                    cadence(event).to_owned(),
                    head(command, 100).to_owned(),
                    row.fires.to_string(),
                    row.to_model.to_string(),
                    tok(avg),
                    tok(row.max),
                    tok(row.total),
                ]
            })
            .collect();
        fmt::table(
            &mut out,
            &[
                "event",
                "cadence",
                "command",
                "outputs",
                "to model",
                "avg tok",
                "max tok",
                "total tok",
            ],
            &table,
        );
        out
    }
}

/// How often an event fires.
fn cadence(event: &str) -> &'static str {
    match event {
        "UserPromptSubmit" => "per prompt",
        "Stop" => "per turn",
        "SubagentStart" | "SubagentStop" => "per sub-agent",
        "SessionStart" | "Setup" => "per start",
        "PreCompact" => "per compact",
        _ if TOOL_EVENTS.contains(&event) => "per tool call",
        _ => "per event",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::sessions::source::{Needs, load, scoped};

    fn attachment(kind: &str, event: &str, extra: Value) -> String {
        let mut body = json!({"type": kind, "hookEvent": event, "hookName": format!("{event}:x"),
            "command": "sh /home/al/hooks/run.sh"});
        body.as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("extra object").clone());
        json!({"type": "attachment", "attachment": body}).to_string()
    }

    #[test]
    fn only_hook_output_that_reaches_the_model_counts_as_tokens() {
        let dir = tempfile::tempdir().expect("temporary root");
        let long = "c".repeat(400);
        let lines = [
            attachment(
                "hook_success",
                "UserPromptSubmit",
                json!({"content": long, "stdout": long}),
            ),
            attachment(
                "hook_success",
                "UserPromptSubmit",
                json!({"content": "c".repeat(200)}),
            ),
            attachment(
                "hook_success",
                "PostToolUse",
                json!({"content": "", "stdout": long}),
            ),
            attachment(
                "hook_additional_context",
                "PostToolUse",
                json!({"content": [long]}),
            ),
            attachment("hook_cancelled", "SessionStart", json!({})),
            attachment(
                "hook_system_message",
                "SessionStart",
                json!({"content": long}),
            ),
            attachment("hook_non_blocking_error", "Stop", json!({"stderr": long})),
            json!({"type": "attachment", "attachment": {"type": "skill_listing", "content": long}})
                .to_string(),
        ];
        let path = dir.path().join("-repo/33333333-aaaa.jsonl");
        fs::create_dir_all(path.parent().expect("parent")).expect("create directory");
        fs::write(&path, lines.join("\n") + "\n").expect("write transcript");
        let scope = scoped(dir.path());
        let mut report = Hooks::new();
        for transcript in load(
            &scope,
            Needs {
                hooks: true,
                ..Needs::default()
            },
        ) {
            report.feed(&transcript);
        }
        let out = report.render(&scope);
        let row = |event: &str| {
            out.lines()
                .find(|line| line.starts_with(event))
                .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_default()
        };
        // Two prompts: 100 and 50 tokens, 150 together.
        assert_eq!(
            row("UserPromptSubmit"),
            "UserPromptSubmit per prompt sh ~/hooks/run.sh 2 2 75 100 150"
        );
        // The success with empty content gave the model nothing; the extra context gave 100.
        assert_eq!(
            row("PostToolUse"),
            "PostToolUse per tool call sh ~/hooks/run.sh 2 1 100 100 100"
        );
        assert_eq!(
            row("SessionStart"),
            "SessionStart per start sh ~/hooks/run.sh 2 0 0 0 0"
        );
        assert_eq!(row("Stop"), "Stop per turn sh ~/hooks/run.sh 1 0 0 0 0");
        assert!(!out.contains("skill_listing"), "{out}");
        assert!(
            out.contains("codex: rollouts record no hook output"),
            "{out}"
        );
        assert!(!out.contains(&long), "{out}");
    }

    fn report(files: &[(&str, Vec<String>)]) -> String {
        let dir = tempfile::tempdir().expect("temporary root");
        for (name, lines) in files {
            let path = dir.path().join(format!("-repo/{name}.jsonl"));
            fs::create_dir_all(path.parent().expect("parent")).expect("create directory");
            fs::write(&path, lines.join("\n") + "\n").expect("write transcript");
        }
        let scope = scoped(dir.path());
        let mut report = Hooks::new();
        for transcript in load(
            &scope,
            Needs {
                hooks: true,
                ..Needs::default()
            },
        ) {
            report.feed(&transcript);
        }
        report
            .render(&scope)
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn blocking_pre_tool_use_output_and_stop_feedback_count_as_hook_events() {
        let blocked = format!(
            "PreToolUse:Bash hook error: [python3 /home/al/hooks/guard.py]: {}",
            "n".repeat(80)
        );
        let feedback = format!(
            "Stop hook feedback:\n[sh /home/al/hooks/stop.sh]: {}",
            "f".repeat(60)
        );
        let lines = vec![
            json!({"type": "user", "uuid": "u1", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "t1", "is_error": true, "content": blocked}]}})
            .to_string(),
            json!({"type": "user", "uuid": "u2", "isMeta": true, "message": {"content": feedback}})
                .to_string(),
            json!({"type": "user", "uuid": "u3", "message": {"content": "Stop hook feedback: typed"}})
                .to_string(),
        ];
        let out = report(&[("55555555-aaaa", lines)]);
        let row = |event: &str| {
            out.lines()
                .find(|line| line.starts_with(event))
                .unwrap_or("")
        };
        assert!(
            row("PreToolUse").contains("per tool call python3 ~/hooks/guard.py 1 1"),
            "{out}"
        );
        assert!(
            row("Stop").contains("per turn Stop hook feedback 1 1"),
            "{out}"
        );
        // The message a person typed is not feedback.
        assert!(!out.contains("typed"), "{out}");
    }

    #[test]
    fn a_hook_attachment_copied_into_a_resumed_session_counts_once() {
        let line = json!({"type": "attachment", "uuid": "h1", "attachment": {
            "type": "hook_success", "hookEvent": "UserPromptSubmit", "command": "run",
            "content": "c".repeat(40)}})
        .to_string();
        let out = report(&[
            ("66666666-aaaa", vec![line.clone()]),
            ("66666666-bbbb", vec![line]),
        ]);
        assert!(
            out.contains("UserPromptSubmit per prompt run 1 1 10 10 10"),
            "{out}"
        );
    }

    #[test]
    fn events_are_labelled_by_how_often_they_fire() {
        for (event, label) in [
            ("Stop", "per turn"),
            ("SubagentStop", "per sub-agent"),
            ("SessionStart", "per start"),
            ("PreCompact", "per compact"),
            ("Notification", "per event"),
        ] {
            assert_eq!(cadence(event), label, "{event}");
        }
    }

    #[test]
    fn a_blocking_error_reaches_the_model() {
        let attachment = json!({"type": "hook_blocking_error", "hookEvent": "PreToolUse",
            "blockingError": {"blockingError": "n".repeat(80), "command": "check"}});
        let summary =
            summary(&attachment, &Value::Null, Path::new("/home/al")).expect("hook summary");
        assert_eq!(summary["hook"]["tokens"], 20.0);
        assert_eq!(summary["hook"]["command"], "?");
    }
}
