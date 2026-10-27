//! `hooks`: the context that hooks add to the model, by event and command.
//!
//! Claude Code writes each hook run as an `attachment` record. Only some kinds reach the model:
//! `hook_success` output of the events that add stdout to the prompt, `hook_additional_context`
//! and `hook_blocking_error`. The rest (`hook_cancelled`, `hook_non_blocking_error`,
//! `hook_system_message`, `hook_stopped_continuation`, and the `stdout` of tool hooks) only show
//! to the person. Codex rollouts record no hook output.

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
/// `None` for any other attachment.
pub fn summary(attachment: &Value, home: &Path) -> Option<Value> {
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
        },
    }))
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
}

impl Hooks {
    pub fn new() -> Self {
        Self {
            rows: IndexMap::new(),
        }
    }

    pub fn feed(&mut self, transcript: &Loaded) {
        if transcript.harness != Harness::Claude {
            return;
        }
        transcript.records(|record| {
            if text(record, "type") != "hook" {
                return;
            }
            let hook = get(record, "hook");
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
        });
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
        let mut out = format!("claude: hook runs since {}\n", scope.since);
        if rows.is_empty() {
            out.push_str("no hook attachments\n");
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
                "runs",
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
    if event == "UserPromptSubmit" {
        "per prompt"
    } else if TOOL_EVENTS.contains(&event) {
        "per tool call"
    } else {
        "once"
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
            "SessionStart once sh ~/hooks/run.sh 2 0 0 0 0"
        );
        assert_eq!(row("Stop"), "Stop once sh ~/hooks/run.sh 1 0 0 0 0");
        assert!(!out.contains("skill_listing"), "{out}");
        assert!(
            out.contains("codex: rollouts record no hook output"),
            "{out}"
        );
        assert!(!out.contains(&long), "{out}");
    }

    #[test]
    fn a_blocking_error_reaches_the_model() {
        let attachment = json!({"type": "hook_blocking_error", "hookEvent": "PreToolUse",
            "blockingError": {"blockingError": "n".repeat(80), "command": "check"}});
        let summary = summary(&attachment, Path::new("/home/al")).expect("hook summary");
        assert_eq!(summary["hook"]["tokens"], 20.0);
        assert_eq!(summary["hook"]["command"], "?");
    }
}
