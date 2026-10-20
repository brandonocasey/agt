//! `errors`, `denials` and `friction`: failed tool calls and what the person typed.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::Value;

use crate::core::transcripts::injected;
use crate::core::transcripts::json::{display, get, head, key, text, truthy};
use crate::sessions::fmt;
use crate::sessions::source::Loaded;
use crate::sessions::spend::short_project;
use crate::sessions::{ErrorKind, Scope};

static DENIALS: LazyLock<Vec<(Regex, &str)>> =
    LazyLock::new(|| {
        [
        (
            r"denied by the Claude Code auto mode classifier\. Reason: (\[[^\]]+\]|[^.\n]{0,60})",
            "auto mode",
        ),
        (r"The user doesn't want to proceed", "user rejected"),
        (r"denied by a built-in Claude Code safety check", "safety check"),
        (r"don't ask mode", "don't-ask mode"),
        (r"(?i)hook", "hook"),
    ]
    .into_iter()
    .map(|(pattern, label)| (Regex::new(pattern).expect("valid pattern"), label))
    .collect()
    });
static PERMISSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Permission .{0,40}denied").expect("valid pattern"));
static ERROR_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"</?tool_use_error>").expect("valid pattern"));
static DIGITS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+").expect("valid pattern"));

fn result_text(content: &Value) -> String {
    match content {
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.is_object())
            .map(|block| text(block, "text"))
            .collect::<Vec<_>>()
            .join(" "),
        Value::String(text) => text.clone(),
        _ => String::new(),
    }
}

pub(super) fn input_summary(block: &Value) -> String {
    let input = get(block, "input");
    let input = if truthy(input) {
        input.clone()
    } else {
        Value::Object(serde_json::Map::new())
    };
    let summary = ["command", "file_path", "url"]
        .into_iter()
        .map(|name| get(&input, name))
        .find(|value| truthy(value))
        .map_or_else(|| input.to_string(), display);
    head(&summary, 100).to_owned()
}

pub(super) fn compact_failure(block: &Value, summary: &mut Value) {
    if !truthy(get(block, "is_error")) {
        return;
    }
    summary["is_error"] = true.into();
    let content = result_text(get(block, "content"));
    if let Some(denial) = denial_kind(&content) {
        summary["denial"] = denial.into();
        return;
    }
    let cleaned = ERROR_TAG.replace_all(&content, "");
    let mut lines = cleaned
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or_default();
    let first = if first.starts_with("Exit code") {
        lines
            .next()
            .map_or_else(|| first.to_owned(), |next| format!("{first} | {next}"))
    } else {
        first.to_owned()
    };
    summary["error_group"] = head(&DIGITS.replace_all(&first, "#"), 80).into();
}

struct Failure {
    session: String,
    timestamp: String,
    tool: String,
    summary: String,
    denial: Option<String>,
    error: String,
}

fn tool_errors(transcript: &Loaded) -> Vec<Failure> {
    let mut failures = Vec::new();

    let mut uses: HashMap<String, (String, String)> = HashMap::new();
    transcript.records(|record| {
        let message = get(record, "message");
        let Some(blocks) = get(message, "content").as_array() else {
            return;
        };
        for block in blocks.iter().filter(|block| block.is_object()) {
            match text(block, "type") {
                "tool_use" => {
                    let summary = text(block, "input_summary");
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("?");
                    uses.insert(key(get(block, "id")), (name.to_owned(), summary.to_owned()));
                }
                "tool_result" if truthy(get(block, "is_error")) => {
                    let (tool, summary) = uses
                        .get(&key(get(block, "tool_use_id")))
                        .cloned()
                        .unwrap_or_else(|| ("?".into(), String::new()));
                    failures.push(Failure {
                        session: transcript.session.clone(),
                        timestamp: text(record, "timestamp").to_owned(),
                        tool,
                        summary,
                        denial: block
                            .get("denial")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        error: text(block, "error_group").to_owned(),
                    });
                }
                _ => {}
            }
        }
    });
    failures
}

/// The kind of permission denial `text` reports, or `None` for an ordinary error.
fn denial_kind(text: &str) -> Option<String> {
    // A command that ran and failed is an error even when its output says "Permission denied".
    if text.trim_start().starts_with("Exit code")
        || (!text.contains("denied")
            && !text.contains("rejected")
            && !text.contains("doesn't want to proceed"))
    {
        return None;
    }
    for (pattern, label) in DENIALS.iter() {
        if let Some(found) = pattern.captures(text) {
            return Some(match found.get(1) {
                Some(reason) => format!("{label}: {}", reason.as_str().trim()),
                None => (*label).to_owned(),
            });
        }
    }
    PERMISSION.is_match(text).then(|| "other".to_owned())
}

/// Rows of (group, session, timestamp, example), printed as count, sessions, last date, group
/// and the first non-empty example.
/// Count, sessions, last day and first example of one group.
type Tally = (usize, HashSet<String>, String, String);

fn grouped(out: &mut String, rows: Vec<(String, String, String, String)>, top: usize) {
    let mut groups: IndexMap<String, Tally> = IndexMap::new();
    for (group, session, timestamp, example) in rows {
        let entry = groups.entry(group).or_default();
        entry.0 += 1;
        entry.1.insert(session);
        let day = head(&timestamp, 10);
        if day > entry.2.as_str() {
            entry.2 = day.to_owned();
        }
        if entry.3.is_empty() {
            entry.3 = example;
        }
    }
    let mut items: Vec<(String, Tally)> = groups.into_iter().collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.1.0));
    let total: usize = items.iter().map(|(_, item)| item.0).sum();
    let sessions: HashSet<&String> = items.iter().flat_map(|(_, item)| &item.1).collect();
    out.push_str(&format!("{total} total in {} sessions\n", sessions.len()));
    let rows: Vec<Vec<String>> = items
        .iter()
        .take(top)
        .map(|(group, (count, sessions, last, example))| {
            vec![
                count.to_string(),
                sessions.len().to_string(),
                last.clone(),
                head(group, 90).to_owned(),
                head(example, 70).replace('\n', " "),
            ]
        })
        .collect();
    fmt::table(
        out,
        &["count", "sessions", "last", "group", "example"],
        &rows,
    );
}

pub struct Failures {
    kind: ErrorKind,
    rows: Vec<(String, String, String, String)>,
}

impl Failures {
    pub fn new(kind: ErrorKind) -> Self {
        Self {
            kind,
            rows: Vec::new(),
        }
    }

    pub fn feed(&mut self, transcript: &Loaded) {
        for failure in tool_errors(transcript) {
            if let Some(denial) = failure.denial {
                if self.kind != ErrorKind::Error {
                    self.rows.push((
                        denial,
                        failure.session,
                        failure.timestamp,
                        format!("{}: {}", failure.tool, failure.summary),
                    ));
                }
                continue;
            }
            if self.kind == ErrorKind::Denial {
                continue;
            }
            let group = format!("{}: {}", failure.tool, failure.error);
            self.rows
                .push((group, failure.session, failure.timestamp, failure.summary));
        }
    }

    pub fn render(self, scope: &Scope) -> String {
        let mut out = String::new();
        grouped(&mut out, self.rows, scope.limit);
        out
    }
}

static CORRECTIONS: LazyLock<Vec<(&str, Regex)>> = LazyLock::new(|| {
    [
        ("continue", r"^(continue|keep going|go on|proceed)\b"),
        (
            "you do it",
            r"\byou (do|run|paste|try|fix) (it|that|this)\b|^you do\b",
        ),
        ("did you", r"\bdid you \w+"),
        ("eli5", r"\beli5\b"),
        ("status?", r"^status\b"),
        ("stop", r"^stop\b|\bstop \w+ing\b"),
        ("I meant", r"\bi meant\b"),
        ("again", r"\bagain\b|\bstill (broken|failing|wrong|not)\b"),
    ]
    .into_iter()
    .map(|(label, pattern)| {
        (
            label,
            Regex::new(&format!("(?i){pattern}")).expect("valid pattern"),
        )
    })
    .collect()
});
static COMMAND_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<command-name>([^<]*)</command-name>").expect("valid pattern"));
static COMMAND_ARGS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<command-args>([^<]*)</command-args>").expect("valid pattern"));
static BASH_INPUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^<bash-input>(.*?)</bash-input>").expect("valid pattern"));
static PUNCTUATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^\w\s/!-]").expect("valid pattern"));

/// Text the person typed, or `None` for tool results and injected harness content.
pub(super) fn user_text(record: &Value) -> Option<String> {
    if text(record, "type") != "user"
        || truthy(get(record, "isMeta"))
        || truthy(get(record, "isCompactSummary"))
        || text(record, "entrypoint").starts_with("sdk")
    {
        return None;
    }
    let content = get(get(record, "message"), "content");
    let content = match content {
        Value::Array(blocks) => {
            if blocks
                .iter()
                .any(|block| text(block, "type") == "tool_result")
            {
                return None;
            }
            blocks
                .iter()
                .filter(|block| text(block, "type") == "text")
                .map(|block| text(block, "text"))
                .collect::<Vec<_>>()
                .join(" ")
        }
        Value::String(text) => text.clone(),
        _ => return None,
    };
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    if let Some(command) = COMMAND_NAME.captures(content) {
        let arguments = COMMAND_ARGS
            .captures(content)
            .map(|found| found[1].to_owned())
            .unwrap_or_default();
        return Some(format!("{} {arguments}", &command[1]).trim().to_owned());
    }
    if let Some(bash) = BASH_INPUT.captures(content) {
        return Some(format!("! {}", bash[1].trim()));
    }
    if injected(content) {
        return None;
    }
    Some(content.to_owned())
}

type Seen = Vec<(String, String)>;

fn last_day(seen: &Seen) -> String {
    seen.iter()
        .map(|(_, day)| day.clone())
        .max()
        .unwrap_or_default()
}

fn sessions(seen: &Seen) -> usize {
    seen.iter()
        .map(|(session, _)| session)
        .collect::<HashSet<_>>()
        .len()
}

pub struct Friction {
    messages: Vec<(String, String, String, String)>,
}

impl Friction {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }
    pub fn feed(&mut self, transcript: &Loaded) {
        if transcript.agent != "main" {
            return;
        }
        transcript.records(|record| {
            if let Some(typed) = record.get("typed").and_then(Value::as_str) {
                self.messages.push((
                    head(text(record, "timestamp"), 10).to_owned(),
                    transcript.project.clone(),
                    transcript.session.clone(),
                    typed.to_owned(),
                ));
            }
        });
    }

    pub fn render(self, scope: &Scope, tsv: bool, width: usize) -> String {
        let Self { messages } = self;
        let mut out = String::new();
        if tsv {
            for (day, project, session, typed) in &messages {
                let flat = typed.replace('\t', " ").replace('\n', " \\n ");
                out.push_str(&format!(
                    "{day}\t{project}\t{session}\t{}\n",
                    head(&flat, width)
                ));
            }
            return out;
        }
        let mut rows = Vec::new();
        let mut short: IndexMap<String, Seen> = IndexMap::new();
        let mut normalized: IndexMap<String, Seen> = IndexMap::new();
        for (day, project, session, typed) in &messages {
            let flat = typed.split_whitespace().collect::<Vec<_>>().join(" ");
            if flat.starts_with("[Request interrupted") {
                let label = if flat.contains("tool use") {
                    "interrupt (tool use)"
                } else {
                    "interrupt"
                };
                rows.push((
                    label.to_owned(),
                    session.clone(),
                    day.clone(),
                    short_project(project, &scope.home),
                ));
                continue;
            }
            for (label, pattern) in CORRECTIONS.iter() {
                if pattern.is_match(&flat) {
                    rows.push((
                        format!("correction: {label}"),
                        session.clone(),
                        day.clone(),
                        flat.clone(),
                    ));
                }
            }
            let lower = flat.to_lowercase();
            if flat.split_whitespace().count() <= 3 {
                short
                    .entry(lower.clone())
                    .or_default()
                    .push((session.clone(), day.clone()));
            }
            normalized
                .entry(PUNCTUATION.replace_all(&lower, "").into_owned())
                .or_default()
                .push((session.clone(), day.clone()));
        }
        out.push_str(&format!(
            "{} user messages since {}\n\n",
            messages.len(),
            scope.since
        ));
        out.push_str("Interrupts and corrections:\n");
        grouped(&mut out, rows, scope.limit);
        out.push_str("\nShort replies (3 words or fewer), top 20:\n");
        let mut replies: Vec<(String, Seen)> = short.into_iter().collect();
        replies.sort_by_key(|item| std::cmp::Reverse(item.1.len()));
        let table: Vec<Vec<String>> = replies
            .iter()
            .take(20)
            .map(|(reply, seen)| {
                vec![
                    seen.len().to_string(),
                    sessions(seen).to_string(),
                    last_day(seen),
                    head(reply, 60).to_owned(),
                ]
            })
            .collect();
        fmt::table(&mut out, &["count", "sessions", "last", "reply"], &table);
        out.push_str("\nSame prompt in 3 or more sessions:\n");
        let mut repeated: Vec<(String, Seen)> = normalized
            .into_iter()
            .filter(|(_, seen)| sessions(seen) >= 3)
            .collect();
        repeated.sort_by_key(|item| std::cmp::Reverse(item.1.len()));
        let table: Vec<Vec<String>> = repeated
            .iter()
            .take(scope.limit)
            .map(|(prompt, seen)| {
                vec![
                    seen.len().to_string(),
                    sessions(seen).to_string(),
                    last_day(seen),
                    head(prompt, 70).to_owned(),
                ]
            })
            .collect();
        fmt::table(&mut out, &["count", "sessions", "last", "prompt"], &table);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denials_are_told_from_errors() {
        assert_eq!(denial_kind("Exit code 1\nPermission denied"), None);
        assert_eq!(
            denial_kind(
                "Permission to use Bash has been denied by the Claude Code auto mode classifier. Reason: [risky] x"
            ),
            Some("auto mode: [risky]".into())
        );
        assert_eq!(
            denial_kind("The user doesn't want to proceed with this tool use."),
            Some("user rejected".into())
        );
        assert_eq!(
            denial_kind("Permission for this path was denied"),
            Some("other".into())
        );
    }

    #[test]
    fn typed_text_skips_harness_content() {
        let typed = |content: Value| {
            user_text(&serde_json::json!({"type": "user", "message": {"content": content}}))
        };
        assert_eq!(typed("  hi ".into()), Some("hi".into()));
        assert_eq!(
            typed("<command-name>/review</command-name><command-args>x</command-args>".into()),
            Some("/review x".into())
        );
        assert_eq!(
            typed("<bash-input>ls</bash-input>".into()),
            Some("! ls".into())
        );
        assert_eq!(typed("<system-reminder>".into()), None);
    }
}
