//! `cost` and `agents`: USD and context per group of API responses.

use std::collections::HashSet;

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::core::error::{Result, ctx, fail};
use crate::core::transcripts::json::{count, tail, text};
use crate::core::transcripts::session_files;
use crate::sessions::Scope;
use crate::sessions::fmt::{self, grouped, median, round, tok};
use crate::sessions::prices::Prices;
use crate::sessions::source::transcripts;

const USAGE_KEYS: [&str; 4] = [
    "input_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
    "output_tokens",
];
const BY_KEYS: [&str; 6] = ["agent", "model", "project", "session", "file", "harness"];

#[derive(Default)]
struct Group {
    calls: i64,
    tools: i64,
    tokens: [i64; 4],
    /// USD for input, cache writes, cache reads and output.
    usd: [f64; 4],
}

struct Row {
    key: Vec<String>,
    group: Group,
    usd: f64,
}

pub fn cost(scope: &Scope, by: &str, as_json: bool) -> Result<String> {
    let prices = Prices::load()?;
    let keys: Vec<&str> = by.split(',').collect();
    if let Some(bad) = keys.iter().find(|key| !BY_KEYS.contains(key)) {
        return fail(format!("--by takes {}; got {bad}", BY_KEYS.join(", ")));
    }
    let mut groups: IndexMap<Vec<String>, Group> = IndexMap::new();
    let mut unpriced: IndexMap<String, i64> = IndexMap::new();
    let mut fast = 0i64;
    let mut seen = HashSet::new();
    for transcript in transcripts(scope) {
        for call in transcript.api_calls(&scope.since_iso, &mut seen) {
            let fields = |name: &str| -> String {
                match name {
                    "agent" => transcript.agent.clone(),
                    "model" => call.model.clone(),
                    "project" => short_project(&transcript.project),
                    "session" => transcript.session.clone(),
                    "file" => transcript.path.to_string_lossy().into_owned(),
                    _ => transcript.harness().to_owned(),
                }
            };
            let group = groups
                .entry(keys.iter().map(|key| fields(key)).collect())
                .or_default();
            group.calls += 1;
            group.tools += call.tools as i64;
            for (total, key) in group.tokens.iter_mut().zip(USAGE_KEYS) {
                *total += count(&call.usage, key);
            }
            fast += i64::from(text(&call.usage, "speed") == "fast");
            match prices.usd(&call.model, &call.usage) {
                None => *unpriced.entry(call.model.clone()).or_default() += 1,
                Some(parts) => {
                    for (sum, value) in group.usd.iter_mut().zip(parts.values()) {
                        *sum += value;
                    }
                }
            }
        }
    }
    let mut rows: Vec<Row> = groups
        .into_iter()
        .map(|(key, mut group)| {
            let usd = round(group.usd.iter().sum(), 4);
            group.usd = group.usd.map(|part| round(part, 4));
            Row { key, group, usd }
        })
        .collect();
    rows.sort_by(|a, b| b.usd.total_cmp(&a.usd));
    if as_json {
        let rows: Vec<Value> = rows
            .iter()
            .map(|row| {
                let mut object = Map::new();
                for (name, value) in keys.iter().zip(&row.key) {
                    object.insert((*name).to_owned(), json!(value));
                }
                object.insert("calls".into(), json!(row.group.calls));
                object.insert("tool_calls".into(), json!(row.group.tools));
                for (name, total) in USAGE_KEYS.iter().zip(row.group.tokens) {
                    object.insert((*name).to_owned(), json!(total));
                }
                object.insert("usd".into(), json!(row.usd));
                for (name, part) in ["usd_input", "usd_write", "usd_read", "usd_output"]
                    .iter()
                    .zip(row.group.usd)
                {
                    object.insert((*name).to_owned(), json!(part));
                }
                Value::Object(object)
            })
            .collect();
        let report = json!({"rows": rows, "unpriced": unpriced, "fast_calls": fast});
        let mut text = serde_json::to_string_pretty(&report).map_err(ctx("serialize report"))?;
        text.push('\n');
        return Ok(text);
    }
    let total: f64 = rows.iter().map(|row| row.usd).sum();
    let grand = if total == 0.0 { 1.0 } else { total };
    let mut out = format!("Total ${} since {}", grouped(total, 2), scope.since);
    if let Some(project) = &scope.project {
        out.push_str(&format!(" (project ~ {project})"));
    }
    out.push('\n');
    let mut headers: Vec<&str> = keys.clone();
    headers.extend([
        "calls", "in", "write", "read", "out", "USD", "%", "write%", "read%", "out%",
    ]);
    let table: Vec<Vec<String>> = rows
        .iter()
        .take(scope.top)
        .map(|row| {
            let base = if row.usd == 0.0 { 1.0 } else { row.usd };
            let share = |part: f64| format!("{:.0}", 100.0 * part / base);
            let mut cells: Vec<String> = row
                .key
                .iter()
                .map(|value| tail(value, 60).to_owned())
                .collect();
            cells.push(row.group.calls.to_string());
            cells.extend(row.group.tokens.map(|tokens| tok(tokens as f64)));
            cells.push(grouped(row.usd, 2));
            cells.push(format!("{:.1}", 100.0 * row.usd / grand));
            cells.extend([row.group.usd[1], row.group.usd[2], row.group.usd[3]].map(share));
            cells
        })
        .collect();
    fmt::table(&mut out, &headers, &table);
    if rows.len() > scope.top {
        out.push_str(&format!(
            "({} more groups; --top raises the limit)\n",
            rows.len() - scope.top
        ));
    }
    if !unpriced.is_empty() {
        let unpriced: Vec<String> = unpriced
            .into_iter()
            .map(|(model, calls)| format!("{model}: {calls}"))
            .collect();
        out.push_str(&format!(
            "Unpriced models (add them to the price file): {}\n",
            unpriced.join(", ")
        ));
    }
    if fast > 0 {
        out.push_str(&format!("Fast-mode calls: {fast}\n"));
    }
    Ok(out)
}

/// What one session cost so far.
#[derive(Default)]
pub struct SessionCost {
    pub calls: i64,
    /// Input, cache write, cache read and output tokens.
    pub tokens: [i64; 4],
    pub usd: f64,
    /// Calls to a model the price file does not list.
    pub unpriced_calls: i64,
    /// The model and context tokens (input, cache writes and reads) of the main agent's last call.
    pub last: Option<(String, i64)>,
}

/// The cost of the session with this full id, across its transcripts and its subagents'.
pub fn session_cost(session: &str) -> Result<SessionCost> {
    let prices = Prices::load()?;
    let mut seen = HashSet::new();
    let mut cost = SessionCost::default();
    let found = session_files(&super::source::home(), session);
    if found.is_empty() {
        return fail(format!("session {session} has no transcripts"));
    }
    for transcript in found {
        let calls = transcript.api_calls("", &mut seen);
        if transcript.agent == "main"
            && let Some(call) = calls.last()
        {
            let context = USAGE_KEYS[..3]
                .iter()
                .map(|key| count(&call.usage, key))
                .sum();
            cost.last = Some((call.model.clone(), context));
        }
        for call in calls {
            cost.calls += 1;
            for (total, key) in cost.tokens.iter_mut().zip(USAGE_KEYS) {
                *total += count(&call.usage, key);
            }
            match prices.usd(&call.model, &call.usage) {
                Some(parts) => cost.usd += parts.total(),
                None => cost.unpriced_calls += 1,
            }
        }
    }
    Ok(cost)
}

/// `cost --session`: one session's cost, as a line or as JSON.
pub fn session(id: &str, as_json: bool) -> Result<String> {
    let cost = session_cost(id)?;
    if as_json {
        let mut object = Map::new();
        object.insert("session".into(), json!(id));
        object.insert("calls".into(), json!(cost.calls));
        for (name, total) in USAGE_KEYS.iter().zip(cost.tokens) {
            object.insert((*name).to_owned(), json!(total));
        }
        object.insert("usd".into(), json!(round(cost.usd, 4)));
        object.insert("unpriced_calls".into(), json!(cost.unpriced_calls));
        let mut text = serde_json::to_string_pretty(&Value::Object(object))
            .map_err(ctx("serialize report"))?;
        text.push('\n');
        return Ok(text);
    }
    let [input, write, read, output] = cost.tokens.map(|tokens| tok(tokens as f64));
    let mut line = format!(
        "Session {id}: ${} over {} calls (in {input}, write {write}, read {read}, out {output})\n",
        grouped(cost.usd, 2),
        cost.calls
    );
    if cost.unpriced_calls > 0 {
        line.push_str(&format!(
            "{} calls to models the price file does not list\n",
            cost.unpriced_calls
        ));
    }
    Ok(line)
}

pub fn agents(scope: &Scope) -> Result<String> {
    let prices = Prices::load()?;
    // Per run: start context, peak context, calls, USD.
    let mut by: IndexMap<String, Vec<[f64; 4]>> = IndexMap::new();
    let mut seen = HashSet::new();
    for transcript in transcripts(scope) {
        let calls = transcript.api_calls(&scope.since_iso, &mut seen);
        if calls.is_empty() {
            continue;
        }
        let context: Vec<i64> = calls
            .iter()
            .map(|call| {
                USAGE_KEYS[..3]
                    .iter()
                    .map(|key| count(&call.usage, key))
                    .sum()
            })
            .collect();
        let usd = calls.iter().fold(0.0, |sum, call| {
            sum + prices
                .usd(&call.model, &call.usage)
                .map_or(0.0, |parts| parts.total())
        });
        by.entry(transcript.agent.clone()).or_default().push([
            context[0] as f64,
            context.iter().copied().max().unwrap_or_default() as f64,
            calls.len() as f64,
            usd,
        ]);
    }
    let total = |runs: &[[f64; 4]]| runs.iter().fold(0.0, |sum, run| sum + run[3]);
    let mut agents: Vec<(String, Vec<[f64; 4]>)> = by.into_iter().collect();
    agents.sort_by(|a, b| total(&b.1).total_cmp(&total(&a.1)));
    let rows: Vec<Vec<String>> = agents
        .iter()
        .map(|(agent, runs)| {
            let median: Vec<f64> = (0..4)
                .map(|index| median(runs.iter().map(|run| run[index]).collect()))
                .collect();
            vec![
                agent.clone(),
                runs.len().to_string(),
                tok(median[0]),
                tok(median[1]),
                format!("{}", median[2].trunc() as i64),
                format!("{:.2}", median[3]),
                grouped(total(runs), 2),
            ]
        })
        .collect();
    let mut out = String::new();
    fmt::table(
        &mut out,
        &[
            "agent",
            "runs",
            "med start ctx",
            "med peak ctx",
            "med calls",
            "med USD",
            "total USD",
        ],
        &rows,
    );
    Ok(out)
}

/// A Claude Code project directory name under the home directory as `~/<rest>`.
pub fn short_project(name: &str) -> String {
    let home = super::source::home()
        .to_string_lossy()
        .replace(['/', '.'], "-");
    match name.strip_prefix(home.as_str()) {
        Some(rest) => format!("~/{}", rest.chars().skip(1).collect::<String>()),
        None => name.to_owned(),
    }
}
