//! The cost report: USD per group of API responses, and the cost of one session.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::core::error::{Result, fail};
use crate::core::local;
use crate::core::output;
use crate::core::transcripts::json::{tail, text};
use crate::core::transcripts::{Call, session_files};
use crate::sessions::Scope;
use crate::sessions::fmt::{self, grouped, round, tok};
use crate::sessions::prices::{Parts, Prices, USAGE_KEYS, context, tokens};
use crate::sessions::source::Loaded;

const BY_KEYS: [&str; 6] = ["agent", "model", "project", "session", "file", "harness"];

#[derive(Default)]
struct Group {
    calls: i64,
    tools: i64,
    tokens: [i64; 4],
    /// USD for input, cache writes, cache reads and output.
    usd: [f64; 4],
}

impl Group {
    /// Counts `call` and prices it; `None` for a model the price file does not list.
    fn add(&mut self, call: &Call, prices: &Prices) -> Option<Parts> {
        self.calls += 1;
        self.tools += call.tools as i64;
        for (total, count) in self.tokens.iter_mut().zip(tokens(&call.usage)) {
            *total += count;
        }
        let parts = prices.usd(&call.model, &call.usage)?;
        for (sum, value) in self.usd.iter_mut().zip(parts.values()) {
            *sum += value;
        }
        Some(parts)
    }
}

/// Puts a group's token counts into a JSON row under their usage key names.
fn insert_tokens(object: &mut Map<String, Value>, tokens: [i64; 4]) {
    for (name, total) in USAGE_KEYS.iter().zip(tokens) {
        object.insert((*name).to_owned(), json!(total));
    }
}

struct Row {
    key: Vec<String>,
    group: Group,
    usd: f64,
}

pub struct Cost {
    prices: Prices,
    keys: Vec<String>,
    groups: IndexMap<Vec<String>, Group>,
    unpriced: IndexMap<String, i64>,
    fast: i64,
    seen: HashSet<String>,
    home: PathBuf,
}

impl Cost {
    pub fn new(by: &str) -> Result<Self> {
        let keys: Vec<String> = by.split(',').map(str::to_owned).collect();
        if let Some(bad) = keys.iter().find(|key| !BY_KEYS.contains(&key.as_str())) {
            return fail(format!("--by takes {}; got {bad}", BY_KEYS.join(", ")));
        }
        Ok(Self {
            prices: Prices::load()?,
            keys,
            groups: IndexMap::new(),
            unpriced: IndexMap::new(),
            fast: 0,
            seen: HashSet::new(),
            home: local::home()?,
        })
    }
    pub fn feed(&mut self, transcript: &Loaded) {
        for call in transcript.api_calls(&mut self.seen) {
            let fields = |name: &str| -> String {
                match name {
                    "agent" => transcript.agent.clone(),
                    "model" => call.model.clone(),
                    "project" => short_project(&transcript.project, &self.home),
                    "session" => transcript.session.clone(),
                    "file" => transcript.path.to_string_lossy().into_owned(),
                    _ => transcript.harness.id().to_owned(),
                }
            };
            let group = self
                .groups
                .entry(self.keys.iter().map(|key| fields(key)).collect())
                .or_default();
            self.fast += i64::from(text(&call.usage, "speed") == "fast");
            if group.add(&call, &self.prices).is_none() {
                *self.unpriced.entry(call.model.clone()).or_default() += 1;
            }
        }
    }

    pub fn render(self, scope: &Scope, as_json: bool) -> Result<String> {
        let Self {
            keys,
            groups,
            unpriced,
            fast,
            ..
        } = self;
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
                    insert_tokens(&mut object, row.group.tokens);
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
            return output::pretty(&report);
        }
        let total: f64 = rows.iter().map(|row| row.usd).sum();
        let grand = if total == 0.0 { 1.0 } else { total };
        let mut out = format!("Total ${} since {}", grouped(total, 2), scope.since);
        if let Some(project) = &scope.project {
            out.push_str(&format!(" (project ~ {project})"));
        }
        out.push('\n');
        let mut headers: Vec<&str> = keys.iter().map(String::as_str).collect();
        headers.extend([
            "calls", "in", "write", "read", "out", "USD", "%", "write%", "read%", "out%",
        ]);
        let table: Vec<Vec<String>> = rows
            .iter()
            .take(scope.limit)
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
        if rows.len() > scope.limit {
            out.push_str(&format!(
                "({} more groups; --limit raises the limit)\n",
                rows.len() - scope.limit
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
    let mut group = Group::default();
    let found = session_files(&local::home()?, session);
    if found.is_empty() {
        return fail(format!("session {session} has no transcripts"));
    }
    for transcript in found {
        let calls = transcript.api_calls("", &mut seen);
        if transcript.agent == "main"
            && let Some(call) = calls.last()
        {
            cost.last = Some((call.model.clone(), context(&call.usage)));
        }
        for call in calls {
            match group.add(&call, &prices) {
                Some(parts) => cost.usd += parts.total(),
                None => cost.unpriced_calls += 1,
            }
        }
    }
    cost.calls = group.calls;
    cost.tokens = group.tokens;
    Ok(cost)
}

/// A Claude Code project directory name under the home directory as `~/<rest>`.
pub fn short_project(name: &str, home: &Path) -> String {
    let home = home.to_string_lossy().replace(['/', '.'], "-");
    match name.strip_prefix(home.as_str()) {
        Some(rest) => format!("~/{}", rest.chars().skip(1).collect::<String>()),
        None => name.to_owned(),
    }
}
