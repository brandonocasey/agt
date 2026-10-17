//! `tools` and `cache-gaps`: what tool results cost, and what idle gaps cost.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Value, json};

use crate::core::error::Result;
use crate::sessions::Scope;
use crate::sessions::prices::Prices;
use crate::sessions::py::{self, count, fixed, get, grouped, key, object, text, tok, truthy};
use crate::sessions::source::{Kind, transcripts};
use crate::sessions::spend::short_project;

// Leading `cd DIR &&`, `cd DIR;`, and `VAR=value;` steps hide the command that did the work.
static PREAMBLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:(?:cd\s+\S+|[A-Za-z_]\w*=\S*)\s*(?:&&|;)\s*)+").expect("valid pattern")
});

fn tool_key(name: &str, input: &Value) -> String {
    if name == "Bash" {
        let command = text(input, "command");
        let command = PREAMBLE.replace(py::strip(command), "");
        let Some(first) = py::split(&command).next() else {
            return "Bash".into();
        };
        let base = first.rsplit('/').next().unwrap_or(first);
        return format!("Bash:{}", py::head(base, 20));
    }
    if name == "Read" {
        let path = text(input, "file_path");
        let kind = if path.contains("/skills/") || path.contains(".claude") {
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

/// About four characters per token; an image counts 1,600.
fn est_tokens(content: &Value) -> f64 {
    match content {
        Value::String(text) => py::chars(text) as f64 / 4.0,
        Value::Array(blocks) => {
            blocks
                .iter()
                .filter(|block| block.is_object())
                .fold(0.0, |sum, block| {
                    sum + match text(block, "type") {
                        "image" => 1600.0,
                        "text" => py::chars(text(block, "text")) as f64 / 4.0,
                        _ => py::chars(&py::dumps(block)) as f64 / 4.0,
                    }
                })
        }
        other if truthy(other) => py::chars(&py::dumps(other)) as f64 / 4.0,
        _ => 0.0,
    }
}

pub fn tools(scope: &Scope) -> String {
    // Relative cost units in multiples of the input price: a tool result is written to the cache
    // once and re-read on every later call until the next compaction.
    let (write, read, output) = (2.0, 0.1, 5.0);
    let mut totals: IndexMap<String, (i64, f64, f64)> = IndexMap::new();
    let mut grand = 0.0;
    for transcript in transcripts(scope) {
        let mut seen = HashSet::new();
        let mut pending: HashMap<String, (String, f64)> = HashMap::new();
        let mut results: Vec<(i64, f64, f64, String)> = Vec::new();
        let mut bounds: Vec<i64> = Vec::new();
        let mut calls = 0i64;
        transcript.records(&scope.since_iso, |record| {
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
                    let usage = get(message, "usage");
                    grand += count(usage, "input_tokens") as f64
                        + count(usage, "cache_creation_input_tokens") as f64 * write
                        + count(usage, "cache_read_input_tokens") as f64 * read
                        + count(usage, "output_tokens") as f64 * output;
                }
                for block in get(message, "content").as_array().into_iter().flatten() {
                    if text(block, "type") == "tool_use" {
                        let input = get(block, "input");
                        let tokens = py::chars(&py::dumps(get(block, "input"))) as f64 / 4.0;
                        pending.insert(
                            key(get(block, "id")),
                            (tool_key(text(block, "name"), input), tokens),
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
                        results.push((calls, est_tokens(get(block, "content")), input, name));
                    }
                }
            }
        });
        bounds.push(calls);
        for (at, result, input, name) in results {
            let later = (bounds.iter().find(|end| **end >= at).copied().unwrap_or(at) - at) as f64;
            let entry = totals.entry(name).or_insert((0, 0.0, 0.0));
            entry.0 += 1;
            entry.1 += result;
            entry.2 += result * (write + read * later) + input * (output + read * later);
        }
    }
    let mut rows: Vec<(String, (i64, f64, f64))> = totals.into_iter().collect();
    rows.sort_by(|a, b| b.1.2.total_cmp(&a.1.2));
    let attributed = rows.iter().fold(0.0, |sum, (_, row)| sum + row.2);
    let base = if grand == 0.0 { 1.0 } else { grand };
    let mut out = format!(
        "Tool inputs and results carry {}% of estimated cost since {}\n",
        fixed(100.0 * attributed / base, 0),
        scope.since
    );
    let table: Vec<Vec<String>> = rows
        .iter()
        .take(scope.top)
        .map(|(name, (calls, result, cost))| {
            vec![
                name.clone(),
                calls.to_string(),
                py::int(result / *calls as f64),
                fixed(100.0 * cost / base, 1),
            ]
        })
        .collect();
    py::table(
        &mut out,
        &["tool", "calls", "avg result tok", "share %"],
        &table,
    );
    out
}

/// Microseconds since the epoch for an ISO 8601 time, `Z` or an offset.
fn parse_ts(stamp: &str) -> Option<i128> {
    let (date, time) = stamp.split_once(['T', ' '])?;
    let mut date = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let split = time.find(['Z', 'z', '+', '-']).unwrap_or(time.len());
    let (clock, zone) = time.split_at(split);
    let (whole, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let mut parts = whole.split(':').map(|part| part.parse::<i64>().ok());
    let (hour, minute) = (parts.next()??, parts.next()??);
    let second = parts.next().flatten().unwrap_or(0);
    let micros: i64 = format!("{:0<6}", py::head(fraction, 6)).parse().ok()?;
    let offset = match zone {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let digits: String = zone[1..].chars().filter(char::is_ascii_digit).collect();
            let hours: i64 = digits.get(..2)?.parse().ok()?;
            let minutes: i64 = digits.get(2..4).unwrap_or("0").parse().ok()?;
            sign * (hours * 3600 + minutes * 60)
        }
    };
    let days = crate::core::clock::days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    Some(i128::from(secs) * 1_000_000 + i128::from(micros))
}

struct Previous {
    at: i128,
    context: i64,
    tool: Option<String>,
}

pub fn cache_gaps(scope: &Scope, gap: Option<f64>) -> Result<String> {
    let prices = Prices::load()?;
    let minutes = gap.unwrap_or(5.0);
    let (mut gaps, mut expired, mut rewrite_tokens, mut rewrite_usd) = (0i64, 0i64, 0i64, 0.0);
    let mut per: IndexMap<(String, String), i64> = IndexMap::new();
    let mut cause: IndexMap<String, i64> = IndexMap::new();
    // Gap minutes, rewrite tokens, time, project, session, agent.
    let mut worst: Vec<(f64, i64, String, String, String, String)> = Vec::new();
    for transcript in transcripts(scope) {
        let mut previous: Option<Previous> = None;
        let mut last_tool: Option<String> = None;
        let mut current = Value::Null;
        let mut seen: HashSet<String> = HashSet::new();
        transcript.records(&scope.since_iso, |record| {
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
                    let input = get(block, "input");
                    let detail = [get(input, "command"), get(input, "description")]
                        .into_iter()
                        .find(|value| truthy(value))
                        .map(|value| match value {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        })
                        .unwrap_or_default();
                    last_tool = Some(
                        format!("{}: {}", text(block, "name"), py::head(&detail, 60))
                            .replace('\n', " "),
                    );
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
            let Some(at) = parse_ts(stamp) else {
                return;
            };
            seen.insert(id_key);
            // OpenAI bills a cache miss as plain input, with no write, so for Codex the
            // uncached input is what a gap rewrites.
            let codex = transcript.kind == Kind::Codex;
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
                    gaps += 1;
                    if (cached as f64) < 0.5 * previous.context as f64 {
                        expired += 1;
                        rewrite_tokens += written;
                        rewrite_usd += if codex {
                            prices
                                .usd(model, &json!({ "input_tokens": written }))
                                .map_or(0.0, |parts| parts.input)
                        } else {
                            prices
                                .usd(model, &json!({ "cache_creation_input_tokens": written }))
                                .map_or(0.0, |parts| parts.write)
                        };
                        *per.entry((transcript.agent.clone(), transcript.project.clone()))
                            .or_default() += written;
                        *cause
                            .entry(
                                previous
                                    .tool
                                    .clone()
                                    .filter(|tool| !tool.is_empty())
                                    .unwrap_or_else(|| "(no tool: waiting on the user)".into()),
                            )
                            .or_default() += 1;
                        worst.push((
                            elapsed / 60.0,
                            written,
                            py::head(stamp, 16).to_owned(),
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
    let shown = gap.map_or_else(|| "5".to_owned(), py::float_repr);
    let mut out = format!(
        "Gaps over {shown} min: {gaps}; cache expired in {expired}; rewritten {} tokens, about ${} (as 5m writes)\n",
        tok(rewrite_tokens as f64),
        grouped(rewrite_usd, 2)
    );
    out.push_str("\nRewrite tokens by agent and project:\n");
    let rows: Vec<Vec<String>> = most_common(per, scope.top)
        .into_iter()
        .map(|((agent, project), tokens)| vec![agent, short_project(&project), tok(tokens as f64)])
        .collect();
    py::table(&mut out, &["agent", "project", "rewrite tok"], &rows);
    out.push_str("\nTool running before the expired gap:\n");
    let rows: Vec<Vec<String>> = most_common(cause, scope.top)
        .into_iter()
        .map(|(tool, times)| vec![times.to_string(), tool])
        .collect();
    py::table(&mut out, &["count", "tool"], &rows);
    out.push_str("\nLargest rewrites:\n");
    worst.sort_by_key(|item| std::cmp::Reverse(item.1));
    let rows: Vec<Vec<String>> = worst
        .into_iter()
        .take(10)
        .map(|(minutes, tokens, time, project, session, agent)| {
            vec![
                time,
                fixed(minutes, 0),
                tok(tokens as f64),
                agent,
                short_project(&project),
                session,
            ]
        })
        .collect();
    py::table(
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
    Ok(out)
}

/// `Counter.most_common(n)`: highest first, ties in insertion order.
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
        let key = |command: &str| tool_key("Bash", &json!({ "command": command }));
        assert_eq!(key("cd /x && FOO=1; /usr/bin/git status"), "Bash:git");
        assert_eq!(key("  "), "Bash");
        assert_eq!(tool_key("mcp__slack__post", &json!({})), "mcp:slack");
        assert_eq!(
            tool_key("Read", &json!({"file_path": "/a/skills/x.md"})),
            "Read:skills/rules"
        );
    }

    #[test]
    fn timestamps_parse_to_microseconds() {
        assert_eq!(parse_ts("1970-01-01T00:00:01.5Z"), Some(1_500_000));
        assert_eq!(parse_ts("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_ts("nope"), None);
    }
}
