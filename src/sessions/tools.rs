//! `tools` and `cache-gaps`: what tool results cost, and what idle gaps cost.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Value, json};

use crate::core::clock;
use crate::core::error::Result;
use crate::core::harness::{self, Harness};
use crate::core::transcripts::json::{count, display, get, head, key, object, text, truthy};
use crate::sessions::Scope;
use crate::sessions::fmt::{self, grouped, tok};
use crate::sessions::prices::{Prices, tokens};
use crate::sessions::source::Loaded;
use crate::sessions::spend::short_project;

// Leading steps hide the command that did the work: `cd DIR &&` (also `cd A 2>/dev/null || cd B;`),
// `export VAR=value;`, `VAR=value`, and the head of a `for` or `while` loop up to its `do`.
static PREAMBLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"^(?s:(?:cd\s+\S+(?:\s+2>\S+)?\s*(?:&&|;|\n)|cd\s+\S+(?:\s+2>\S+)?\s*\|\|\s*cd\s+\S+\s*(?:&&|;|\n)|export\s+[^;&\n]*(?:&&|;|\n)|[A-Za-z_]\w*=(?:'[^']*'|"[^"]*"|\$\([^)]*\)|\$[^(]|[^\s;&'"$])*\s*(?:&&|;|\n)?|(?:for|while)\s.*?(?:;|\n)\s*do\s)\s*)+"#,
    )
    .expect("valid pattern")
});

// Commands whose first plain argument names what they did, as in `git status`.
const SUBCOMMANDS: [&str; 30] = [
    "agt",
    "aws",
    "brew",
    "bun",
    "cargo",
    "chezmoi",
    "claude",
    "claude-two",
    "codex",
    "deno",
    "docker",
    "gcloud",
    "gh",
    "git",
    "glab",
    "go",
    "jira",
    "just",
    "kubectl",
    "make",
    "mise",
    "npm",
    "pip",
    "pip3",
    "pnpm",
    "poetry",
    "rustup",
    "terraform",
    "uv",
    "yarn",
];

static SUBCOMMAND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9:_-]*$").expect("valid pattern"));

/// `git status` for `git -C DIR status --short`: the first argument that is not an option, an
/// option's value (`-C DIR`, `-c KEY=VALUE`) or a toolchain (`+nightly`).
fn subcommand<'a>(base: &str, mut args: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    if !SUBCOMMANDS.contains(&base) {
        return None;
    }
    while let Some(arg) = args.next() {
        if arg == "-C" || arg == "-c" {
            args.next();
        } else if !arg.starts_with(['-', '+']) {
            return Some(arg).filter(|arg| arg.len() <= 20 && SUBCOMMAND.is_match(arg));
        }
    }
    None
}

// A wrapper word, kept when no command follows it, as in `command -v rg` or `env | grep`.
static WRAPPER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?s:(?:env|time|nohup|command|timeout\s+\S+)\s+([^-|;&\s].*))$")
        .expect("valid pattern")
});

/// A shell command without the leading steps that hide the one that did the work.
pub(super) fn bare_command(command: &str) -> String {
    let mut command = command.trim().to_owned();
    loop {
        let stripped = PREAMBLE.replace(&command, "").into_owned();
        let stripped = match WRAPPER.captures(&stripped) {
            Some(rest) => rest[1].to_owned(),
            None => stripped,
        };
        if stripped == command {
            return command;
        }
        command = stripped;
    }
}

pub(super) fn tool_key(name: &str, input: &Value, home: &Path) -> String {
    if name == "Bash" {
        let command = bare_command(text(input, "command"));
        let mut words = command.split_whitespace();
        let Some(first) = words.next() else {
            return "Bash".into();
        };
        let base = head(first.rsplit('/').next().unwrap_or(first), 20);
        return match subcommand(base, words) {
            Some(sub) => format!("Bash:{base} {sub}"),
            None => format!("Bash:{base}"),
        };
    }
    if name == "Read" {
        let path = text(input, "file_path");
        let kind = if path.contains(harness::SKILLS_PATH)
            || harness::in_claude_profile(Path::new(path), home)
        {
            "skills/rules"
        } else {
            "code/other"
        };
        return format!("Read:{kind}");
    }
    if name.starts_with("mcp__") {
        return format!("mcp:{}", name.split("__").nth(1).unwrap_or_default());
    }
    name.to_owned()
}

/// What a cache write, a cache read and an output token are taken to cost relative to an
/// input token when estimating the share of cost that tool traffic carries; the price file's
/// per-model rates are not used.
const WRITE_WEIGHT: f64 = 2.0;
const READ_WEIGHT: f64 = 0.1;
const OUTPUT_WEIGHT: f64 = 5.0;

/// About four characters per token.
pub(super) fn est_text_tokens(text: &str) -> f64 {
    text.chars().count() as f64 / 4.0
}

/// About four characters per token; an image counts 1,600.
pub(super) fn est_tokens(content: &Value) -> f64 {
    match content {
        Value::String(text) => est_text_tokens(text),
        Value::Array(blocks) => {
            blocks
                .iter()
                .filter(|block| block.is_object())
                .fold(0.0, |sum, block| {
                    sum + match text(block, "type") {
                        "image" => 1600.0,
                        "text" => est_text_tokens(text(block, "text")),
                        _ => est_text_tokens(&block.to_string()),
                    }
                })
        }
        other if truthy(other) => est_text_tokens(&other.to_string()),
        _ => 0.0,
    }
}

pub struct Tools {
    totals: IndexMap<String, (i64, f64, f64)>,
    grand: f64,
}

impl Tools {
    pub fn new() -> Self {
        Self {
            totals: IndexMap::new(),
            grand: 0.0,
        }
    }
    pub fn feed(&mut self, transcript: &Loaded) {
        let mut seen = HashSet::new();
        let mut pending: HashMap<String, (String, f64)> = HashMap::new();
        let mut results: Vec<(i64, f64, f64, String)> = Vec::new();
        let mut bounds: Vec<i64> = Vec::new();
        let mut calls = 0i64;
        transcript.records(|record| {
            let kind = text(record, "type");
            if kind == "system" && text(record, "subtype") == "compact_boundary" {
                bounds.push(calls);
            }
            let Some(message) = record.get("message").filter(|message| message.is_object()) else {
                return;
            };
            if kind == "assistant" {
                let id = get(message, "id");
                if truthy(id) && truthy(get(message, "usage")) && seen.insert(key(id)) {
                    calls += 1;
                    let [input_tokens, written, cached, produced] =
                        tokens(get(message, "usage")).map(|count| count as f64);
                    self.grand += input_tokens
                        + written * WRITE_WEIGHT
                        + cached * READ_WEIGHT
                        + produced * OUTPUT_WEIGHT;
                }
                for block in get(message, "content").as_array().into_iter().flatten() {
                    if text(block, "type") == "tool_use" {
                        let tokens = get(block, "input_tokens").as_f64().unwrap_or_default();
                        pending.insert(
                            key(get(block, "id")),
                            (text(block, "tool_key").to_owned(), tokens),
                        );
                    }
                }
            } else if kind == "user"
                && let Some(blocks) = get(message, "content").as_array()
            {
                for block in blocks {
                    if text(block, "type") != "tool_result" {
                        continue;
                    }
                    if let Some((name, input)) = pending.remove(&key(get(block, "tool_use_id"))) {
                        results.push((
                            calls,
                            get(block, "result_tokens").as_f64().unwrap_or_default(),
                            input,
                            name,
                        ));
                    }
                }
            }
        });
        bounds.push(calls);
        for (at, result, input, name) in results {
            let later = (bounds.iter().find(|end| **end >= at).copied().unwrap_or(at) - at) as f64;
            let entry = self.totals.entry(name).or_insert((0, 0.0, 0.0));
            entry.0 += 1;
            entry.1 += result;
            entry.2 += result * (WRITE_WEIGHT + READ_WEIGHT * later)
                + input * (OUTPUT_WEIGHT + READ_WEIGHT * later);
        }
    }

    pub fn render(self, scope: &Scope) -> String {
        let Self { totals, grand } = self;
        let mut rows: Vec<(String, (i64, f64, f64))> = totals.into_iter().collect();
        rows.sort_by(|a, b| b.1.2.total_cmp(&a.1.2));
        let attributed = rows.iter().fold(0.0, |sum, (_, row)| sum + row.2);
        let base = if grand == 0.0 { 1.0 } else { grand };
        let mut out = format!(
            "Tool inputs and results carry {:.0}% of estimated cost since {}\n",
            100.0 * attributed / base,
            scope.since
        );
        let table: Vec<Vec<String>> = rows
            .iter()
            .take(scope.limit)
            .map(|(name, (calls, result, cost))| {
                vec![
                    name.clone(),
                    calls.to_string(),
                    format!("{}", (result / *calls as f64).trunc() as i64),
                    format!("{:.1}", 100.0 * cost / base),
                ]
            })
            .collect();
        fmt::table(
            &mut out,
            &["tool", "calls", "avg result tok", "share %"],
            &table,
        );
        out
    }
}

struct Previous {
    at: i128,
    context: i64,
    tool: Option<String>,
}

pub(super) fn gap_tool(block: &Value) -> String {
    let input = get(block, "input");
    let detail = [get(input, "command"), get(input, "description")]
        .into_iter()
        .find(|value| truthy(value))
        .map(display)
        .unwrap_or_default();
    format!("{}: {}", text(block, "name"), head(&detail, 60)).replace('\n', " ")
}

pub struct CacheGaps {
    prices: Prices,
    gap: Option<f64>,
    gaps: i64,
    expired: i64,
    rewrite_tokens: i64,
    rewrite_usd: f64,
    per: IndexMap<(String, String), i64>,
    cause: IndexMap<String, i64>,
    worst: Vec<(f64, i64, String, String, String, String)>,
}

impl CacheGaps {
    pub fn new(gap: Option<f64>) -> Result<Self> {
        Ok(Self {
            prices: Prices::load()?,
            gap,
            gaps: 0,
            expired: 0,
            rewrite_tokens: 0,
            rewrite_usd: 0.0,
            per: IndexMap::new(),
            cause: IndexMap::new(),
            worst: Vec::new(),
        })
    }
    pub fn feed(&mut self, transcript: &Loaded) {
        let minutes = self.gap.unwrap_or(5.0);
        let mut previous: Option<Previous> = None;
        let mut last_tool: Option<String> = None;
        let mut current = Value::Null;
        let mut seen: HashSet<String> = HashSet::new();
        transcript.records(|record| {
            let message = object(record, "message");
            if text(record, "type") != "assistant" {
                return;
            }
            let id = get(message, "id");
            if *id != current {
                // A response without tool_use ends the turn, so a gap after it is idle time.
                current = id.clone();
                last_tool = None;
            }
            for block in get(message, "content").as_array().into_iter().flatten() {
                if text(block, "type") == "tool_use" {
                    last_tool = Some(text(block, "gap_tool").to_owned());
                }
            }
            let usage = get(message, "usage");
            let id_key = key(id);
            if seen.contains(&id_key)
                && let Some(previous) = previous.as_mut()
            {
                // Later records of the same response carry its tool_use; it runs during the gap.
                previous.tool.clone_from(&last_tool);
            }
            let model = match message.get("model") {
                Some(Value::String(model)) => model.as_str(),
                _ => "?",
            };
            let Some(stamp) = record.get("timestamp").and_then(Value::as_str) else {
                return;
            };
            if !truthy(usage) || seen.contains(&id_key) || model == "<synthetic>" {
                return;
            }
            let Some(at) = clock::parse_micros(stamp) else {
                return;
            };
            seen.insert(id_key);
            // OpenAI bills a cache miss as plain input, with no write, so for Codex the
            // uncached input is what a gap rewrites.
            let codex = transcript.harness == Harness::Codex;
            let written = count(usage, "cache_creation_input_tokens")
                + if codex {
                    count(usage, "input_tokens")
                } else {
                    0
                };
            let cached = count(usage, "cache_read_input_tokens");
            if let Some(previous) = &previous {
                let elapsed = (at - previous.at) as f64 / 1e6;
                if elapsed > minutes * 60.0 {
                    self.gaps += 1;
                    if (cached as f64) < 0.5 * previous.context as f64 {
                        self.expired += 1;
                        self.rewrite_tokens += written;
                        self.rewrite_usd += if codex {
                            self.prices
                                .usd(model, &json!({ "input_tokens": written }))
                                .map_or(0.0, |parts| parts.input)
                        } else {
                            self.prices
                                .usd(model, &json!({ "cache_creation_input_tokens": written }))
                                .map_or(0.0, |parts| parts.write)
                        };
                        *self
                            .per
                            .entry((transcript.agent.clone(), transcript.project.clone()))
                            .or_default() += written;
                        *self
                            .cause
                            .entry(
                                previous
                                    .tool
                                    .clone()
                                    .filter(|tool| !tool.is_empty())
                                    .unwrap_or_else(|| "(no tool: waiting on the user)".into()),
                            )
                            .or_default() += 1;
                        self.worst.push((
                            elapsed / 60.0,
                            written,
                            head(stamp, 16).to_owned(),
                            transcript.project.clone(),
                            transcript.session.clone(),
                            transcript.agent.clone(),
                        ));
                    }
                }
            }
            previous = Some(Previous {
                at,
                context: cached + written,
                tool: last_tool.clone(),
            });
        });
    }

    pub fn render(self, scope: &Scope) -> String {
        let Self {
            gap,
            gaps,
            expired,
            rewrite_tokens,
            rewrite_usd,
            per,
            cause,
            mut worst,
            ..
        } = self;
        let shown = gap.map_or_else(|| "5".to_owned(), |gap| gap.to_string());
        let mut out = format!(
            "Gaps over {shown} min: {gaps}; cache expired in {expired}; rewritten {} tokens, about ${} (as 5m writes)\n",
            tok(rewrite_tokens as f64),
            grouped(rewrite_usd, 2)
        );
        out.push_str("\nRewrite tokens by agent and project:\n");
        let rows: Vec<Vec<String>> = most_common(per, scope.limit)
            .into_iter()
            .map(|((agent, project), tokens)| {
                vec![
                    agent,
                    short_project(&project, &scope.home),
                    tok(tokens as f64),
                ]
            })
            .collect();
        fmt::table(&mut out, &["agent", "project", "rewrite tok"], &rows);
        out.push_str("\nTool running before the expired gap:\n");
        let rows: Vec<Vec<String>> = most_common(cause, scope.limit)
            .into_iter()
            .map(|(tool, times)| vec![times.to_string(), tool])
            .collect();
        fmt::table(&mut out, &["count", "tool"], &rows);
        out.push_str("\nLargest rewrites:\n");
        worst.sort_by_key(|item| std::cmp::Reverse(item.1));
        let rows: Vec<Vec<String>> = worst
            .into_iter()
            .take(10)
            .map(|(minutes, tokens, time, project, session, agent)| {
                vec![
                    time,
                    format!("{minutes:.0}"),
                    tok(tokens as f64),
                    agent,
                    short_project(&project, &scope.home),
                    session,
                ]
            })
            .collect();
        fmt::table(
            &mut out,
            &[
                "time",
                "gap min",
                "rewrite tok",
                "agent",
                "project",
                "session",
            ],
            &rows,
        );
        out
    }
}

/// The `n` highest counts, ties in insertion order.
pub fn most_common<K>(counts: IndexMap<K, i64>, n: usize) -> Vec<(K, i64)> {
    let mut items: Vec<(K, i64)> = counts.into_iter().collect();
    items.sort_by_key(|item| std::cmp::Reverse(item.1));
    items.truncate(n);
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_keys_skip_cd_and_assignments() {
        let home = Path::new("/home/al");
        let key = |command: &str| tool_key("Bash", &json!({ "command": command }), home);
        assert_eq!(
            key("cd /x && FOO=1; /usr/bin/git status"),
            "Bash:git status"
        );
        assert_eq!(
            key("cd /a 2>/dev/null || cd /b; git -C /r log -1"),
            "Bash:git log"
        );
        assert_eq!(
            key("export A=1 B=2 && cargo +nightly test -q"),
            "Bash:cargo test"
        );
        assert_eq!(key("FOO=1 timeout 5 npm run build"), "Bash:npm run");
        assert_eq!(key("for f in a b; do cat $f; done"), "Bash:cat");
        assert_eq!(key("while read x\ndo rg -n x; done"), "Bash:rg");
        assert_eq!(key("git -c a=b --no-pager"), "Bash:git");
        assert_eq!(key("git README.md"), "Bash:git");
        assert_eq!(key("rg status src"), "Bash:rg");
        assert_eq!(key("cd /x"), "Bash:cd");
        assert_eq!(key("command -v rg"), "Bash:command");
        assert_eq!(key("env | grep PATH"), "Bash:env");
        assert_eq!(key("cd dir || exit 1; make"), "Bash:cd");
        assert_eq!(key("time command mise run test"), "Bash:mise run");
        assert_eq!(key("env A=1 nohup git fetch"), "Bash:git fetch");
        assert_eq!(
            key("B=$(ls -l x); D=\"a b\" E='c d' $F/bin/agt ps"),
            "Bash:agt ps"
        );
        assert_eq!(key("  "), "Bash");
        assert_eq!(tool_key("mcp__slack__post", &json!({}), home), "mcp:slack");
        assert_eq!(
            tool_key("Read", &json!({"file_path": "/a/skills/x.md"}), home),
            "Read:skills/rules"
        );
        let read = |path: &str| tool_key("Read", &json!({ "file_path": path }), home);
        assert_eq!(read("/home/al/.claude/rules/x.md"), "Read:skills/rules");
        assert_eq!(
            read("/home/al/repo/.claude/settings.json"),
            "Read:code/other"
        );
    }
}
