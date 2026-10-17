//! `cost` and `agents`: USD and context per group of API responses.

use std::collections::HashSet;

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::core::error::{Result, fail};
use crate::sessions::Scope;
use crate::sessions::prices::Prices;
use crate::sessions::py::{self, count, fixed, grouped, text, tok};
use crate::sessions::source::transcripts;

const USAGE_KEYS: [&str; 4] = [
    "input_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
    "output_tokens",
];
const BY_KEYS: [&str; 6] = ["agent", "model", "project", "session", "file", "harness"];

/// A sum that stays the integer 0 until a priced response adds to it, as the script's
/// `Counter` did, so JSON prints `0` rather than `0.0` for a group with no priced calls.
#[derive(Clone, Copy, Default)]
struct Usd(Option<f64>);

impl Usd {
    fn add(&mut self, value: f64) {
        self.0 = Some(self.0.unwrap_or(0.0) + value);
    }

    fn value(self) -> f64 {
        self.0.unwrap_or(0.0)
    }

    fn rounded(self) -> Self {
        Self(self.0.map(|value| py::round(value, 4)))
    }

    fn json(self) -> Value {
        self.0.map_or(json!(0), |value| json!(value))
    }
}

#[derive(Default)]
struct Group {
    calls: i64,
    tools: i64,
    tokens: [i64; 4],
    parts: Option<[f64; 4]>,
}

struct Row {
    key: Vec<String>,
    group: Group,
    usd: Usd,
    parts: [Usd; 4],
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
                    let sums = group.parts.get_or_insert([0.0; 4]);
                    for (sum, value) in sums.iter_mut().zip(parts.values()) {
                        *sum += value;
                    }
                }
            }
        }
    }
    let mut rows: Vec<Row> = groups
        .into_iter()
        .map(|(key, group)| {
            let mut usd = Usd::default();
            let mut parts = [Usd::default(); 4];
            if let Some(sums) = group.parts {
                for (part, sum) in parts.iter_mut().zip(sums) {
                    usd.add(sum);
                    part.add(sum);
                }
            }
            Row {
                key,
                group,
                usd: usd.rounded(),
                parts: parts.map(Usd::rounded),
            }
        })
        .collect();
    rows.sort_by(|a, b| b.usd.value().total_cmp(&a.usd.value()));
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
                object.insert("usd".into(), row.usd.json());
                for (name, part) in ["usd_input", "usd_write", "usd_read", "usd_output"]
                    .iter()
                    .zip(row.parts)
                {
                    object.insert((*name).to_owned(), part.json());
                }
                Value::Object(object)
            })
            .collect();
        let unpriced: Map<String, Value> = unpriced
            .into_iter()
            .map(|(model, calls)| (model, json!(calls)))
            .collect();
        let report = json!({"rows": rows, "unpriced": unpriced, "fast_calls": fast});
        return Ok(py::dumps_indented(&report) + "\n");
    }
    let total = rows.iter().fold(0.0, |sum, row| sum + row.usd.value());
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
            let usd = row.usd.value();
            let share = |part: Usd| {
                let base = if usd == 0.0 { 1.0 } else { usd };
                fixed(100.0 * part.value() / base, 0)
            };
            let mut cells: Vec<String> = row
                .key
                .iter()
                .map(|value| py::tail(value, 60).to_owned())
                .collect();
            cells.push(row.group.calls.to_string());
            cells.extend(row.group.tokens.map(|tokens| tok(tokens as f64)));
            cells.push(grouped(usd, 2));
            cells.push(fixed(100.0 * usd / grand, 1));
            cells.extend([row.parts[1], row.parts[2], row.parts[3]].map(share));
            cells
        })
        .collect();
    py::table(&mut out, &headers, &table);
    if rows.len() > scope.top {
        out.push_str(&format!(
            "({} more groups; --top raises the limit)\n",
            rows.len() - scope.top
        ));
    }
    if !unpriced.is_empty() {
        let unpriced: Vec<(String, i64)> = unpriced.into_iter().collect();
        out.push_str(&format!(
            "Unpriced models (add them to the price file): {}\n",
            py::counts_repr(&unpriced)
        ));
    }
    if fast > 0 {
        out.push_str(&format!("Fast-mode calls: {fast}\n"));
    }
    Ok(out)
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
                .map(|index| py::median(runs.iter().map(|run| run[index]).collect()))
                .collect();
            vec![
                agent.clone(),
                runs.len().to_string(),
                tok(median[0]),
                tok(median[1]),
                py::int(median[2]),
                fixed(median[3], 2),
                grouped(total(runs), 2),
            ]
        })
        .collect();
    let mut out = String::new();
    py::table(
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
