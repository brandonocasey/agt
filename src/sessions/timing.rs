//! The time report: wall-clock time spent waiting on the model, on tools and on you.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use indexmap::IndexMap;
use serde_json::Value;

use crate::core::clock;
use crate::core::error::Result;
use crate::core::local;
use crate::core::transcripts::json::{get, key, object, text};
use crate::sessions::Scope;
use crate::sessions::fmt::{self, duration, tok};
use crate::sessions::source::Loaded;
use crate::sessions::spend::{by_keys, group_key};

/// A wait on you longer than this is idle time and is left out.
const IDLE_SECS: f64 = 30.0 * 60.0;

const MODEL: usize = 0;
const TOOLS: usize = 1;
const YOU: usize = 2;

/// Seconds since the previous record, split by what the record ended: a model response, a
/// tool result or a message from you. Each second counts once, so parallel tools share theirs.
pub struct Time {
    keys: Vec<String>,
    groups: IndexMap<Vec<String>, [f64; 3]>,
    /// Per tool, from its call to its result.
    tools: IndexMap<String, Run>,
    idle: i64,
    subagents: bool,
    seen: HashSet<String>,
    home: PathBuf,
}

/// One tool's runs.
#[derive(Default)]
struct Run {
    calls: i64,
    /// Results marked as errors, other than permission denials.
    failed: i64,
    /// Estimated tokens of the inputs and results.
    tokens: f64,
    total: f64,
    longest: f64,
}

fn secs(from: i128, to: i128) -> Option<f64> {
    Some((to - from) as f64 / 1e6).filter(|secs| *secs >= 0.0)
}

impl Time {
    pub fn new(by: &str) -> Result<Self> {
        Ok(Self {
            keys: by_keys(by)?,
            groups: IndexMap::new(),
            tools: IndexMap::new(),
            idle: 0,
            subagents: false,
            seen: HashSet::new(),
            home: local::home()?,
        })
    }

    pub fn feed(&mut self, transcript: &Loaded) {
        let mut previous: Option<i128> = None;
        let mut model = String::from("?");
        let mut pending: HashMap<String, (String, i128, f64)> = HashMap::new();
        transcript.records(|record| {
            let kind = text(record, "type");
            if kind != "user" && kind != "assistant" {
                return;
            }
            let Some(at) = clock::parse_micros(text(record, "timestamp")) else {
                return;
            };
            let gap = previous.replace(at).and_then(|before| secs(before, at));
            // A resumed session repeats earlier records; they were counted in their own file.
            let fresh = record
                .get("uuid")
                .and_then(Value::as_str)
                .is_none_or(|uuid| self.seen.insert(uuid.to_owned()));
            if !fresh {
                return;
            }
            let message = object(record, "message");
            let blocks = get(message, "content").as_array().into_iter().flatten();
            let mut results = false;
            if kind == "assistant" {
                if let Some(name) = message.get("model").and_then(Value::as_str)
                    && name != "<synthetic>"
                {
                    model = name.to_owned();
                }
                for block in blocks.filter(|block| text(block, "type") == "tool_use") {
                    pending.insert(
                        key(get(block, "id")),
                        (
                            text(block, "tool_key").to_owned(),
                            at,
                            get(block, "input_tokens").as_f64().unwrap_or_default(),
                        ),
                    );
                }
            } else {
                for block in blocks.filter(|block| text(block, "type") == "tool_result") {
                    results = true;
                    let Some((name, started, input)) =
                        pending.remove(&key(get(block, "tool_use_id")))
                    else {
                        continue;
                    };
                    let Some(took) = secs(started, at) else {
                        continue;
                    };
                    let run = self.tools.entry(name).or_default();
                    run.calls += 1;
                    run.failed += i64::from(
                        get(block, "is_error").as_bool() == Some(true)
                            && get(block, "denial").is_null(),
                    );
                    run.tokens += input + get(block, "result_tokens").as_f64().unwrap_or_default();
                    run.total += took;
                    run.longest = run.longest.max(took);
                }
            }
            let slot = match (kind, results) {
                ("assistant", _) => MODEL,
                (_, true) => TOOLS,
                _ => YOU,
            };
            let Some(gap) = gap else {
                return;
            };
            if slot == YOU && gap > IDLE_SECS {
                self.idle += 1;
                return;
            }
            self.subagents |= transcript.agent != "main";
            self.groups
                .entry(group_key(&self.keys, transcript, &model, &self.home))
                .or_default()[slot] += gap;
        });
    }

    pub fn render(self, scope: &Scope) -> String {
        let active = |spent: &[f64; 3]| spent[MODEL] + spent[TOOLS];
        let mut rows: Vec<(Vec<String>, [f64; 3])> = self.groups.into_iter().collect();
        rows.sort_by(|a, b| active(&b.1).total_cmp(&active(&a.1)));
        let mut totals = [0.0; 3];
        for (_, spent) in &rows {
            for (total, part) in totals.iter_mut().zip(spent) {
                *total += part;
            }
        }
        let busy = active(&totals);
        let share = |part: f64, of: f64| {
            let of = if of == 0.0 { 1.0 } else { of };
            format!("{:.0}", 100.0 * part / of)
        };
        let mut out = format!(
            "Active {} since {}: model {} ({}%), tools {} ({}%); waiting on you {}",
            duration(busy),
            scope.since,
            duration(totals[MODEL]),
            share(totals[MODEL], busy),
            duration(totals[TOOLS]),
            share(totals[TOOLS], busy),
            duration(totals[YOU]),
        );
        if let Some(project) = &scope.project {
            out.push_str(&format!(" (project ~ {project})"));
        }
        out.push('\n');
        let mut headers: Vec<&str> = self.keys.iter().map(String::as_str).collect();
        headers.extend(["model", "tools", "you", "active", "%"]);
        let table: Vec<Vec<String>> = rows
            .iter()
            .take(scope.limit)
            .map(|(key, spent)| {
                let mut cells: Vec<String> = key.clone();
                cells.extend(spent.map(duration));
                cells.push(duration(active(spent)));
                cells.push(share(active(spent), busy));
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
        if !self.tools.is_empty() {
            let mut tools: Vec<(String, Run)> = self.tools.into_iter().collect();
            tools.sort_by(|a, b| b.1.total.total_cmp(&a.1.total));
            let table: Vec<Vec<String>> = tools
                .iter()
                .take(scope.limit)
                .map(|(name, run)| {
                    vec![
                        name.clone(),
                        run.calls.to_string(),
                        run.failed.to_string(),
                        tok(run.tokens),
                        duration(run.total),
                        duration(run.total / run.calls as f64),
                        duration(run.longest),
                    ]
                })
                .collect();
            out.push_str("\nSlowest tools:\n");
            fmt::table(
                &mut out,
                &["tool", "calls", "failed", "est tok", "total", "avg", "max"],
                &table,
            );
        }
        if self.idle > 0 {
            out.push_str(&format!(
                "Waits on you over 30 min, left out as idle: {}\n",
                self.idle
            ));
        }
        if self.subagents {
            out.push_str("Subagent time also counts in the main agent's Agent tool time.\n");
        }
        out
    }
}
