//! A transcript's records as Claude Code writes them, whichever harness wrote the file, and
//! the API responses in them.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use indexmap::IndexMap;
use serde_json::Value;

use super::codex::Adapter;
use super::json::{display, get, key, object, text, truthy};
use super::locate::Transcript;
use crate::core::harness::Harness;

/// One API response: its last record's usage and model, and how many tools it called.
pub struct Call {
    pub usage: Value,
    pub model: String,
    pub tools: usize,
}

fn lines(path: &Path, mut each: impl FnMut(&Value)) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if let Ok(record) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&line)) {
            each(&record);
        }
    }
}

impl Transcript {
    /// Calls `each` with every record, skipping those stamped before `since` (`YYYY-MM-DD`).
    /// Codex rollouts arrive as Claude Code records, without the history a fork replays from
    /// its parent.
    pub fn records(&self, since: &str, mut each: impl FnMut(&Value)) {
        let mut keep = |record: &Value| {
            let stamp = get(record, "timestamp");
            if since.is_empty() || stamp.as_str().is_none_or(|stamp| stamp >= since) {
                each(record);
            }
        };
        match self.harness {
            Harness::Claude => lines(&self.path, keep),
            Harness::Codex => {
                let mut adapter =
                    Adapter::new(self.path.to_string_lossy().into_owned(), self.scripted);
                lines(&self.path, |record| adapter.feed(record, &mut keep));
                adapter.finish(&mut keep);
            }
        }
    }

    /// One `Call` per API response. Transcripts write one record per content block, so tool
    /// calls are counted across records sharing the response id. Responses whose id is in `seen`
    /// were counted from an earlier file, such as those copied into a resumed session.
    pub fn api_calls(&self, since: &str, seen: &mut HashSet<String>) -> Vec<Call> {
        Self::calls_with(|each| self.records(since, each), seen)
    }

    /// API responses from already adapted records, preserving cross-file deduplication.
    pub fn calls_from_records(records: &[Value], seen: &mut HashSet<String>) -> Vec<Call> {
        Self::calls_with(|each| records.iter().for_each(each), seen)
    }

    fn calls_with(
        read: impl FnOnce(&mut dyn FnMut(&Value)),
        seen: &mut HashSet<String>,
    ) -> Vec<Call> {
        let mut calls: IndexMap<String, Call> = IndexMap::new();
        let mut tools: IndexMap<String, HashSet<String>> = IndexMap::new();
        let mut anonymous = 0usize;
        read(&mut |record| {
            let message = object(record, "message");
            if text(record, "type") != "assistant"
                || !truthy(get(message, "usage"))
                || text(message, "model") == "<synthetic>"
            {
                return;
            }
            let id = get(message, "id");
            let name = if truthy(id) {
                key(id)
            } else {
                anonymous += 1;
                format!("a{anonymous}")
            };
            let model = message.get("model").map_or_else(|| "?".into(), display);
            calls.insert(
                name.clone(),
                Call {
                    usage: get(message, "usage").clone(),
                    model,
                    tools: 0,
                },
            );
            let used = tools.entry(name).or_default();
            if let Some(blocks) = get(message, "content").as_array() {
                for block in blocks {
                    if text(block, "type") == "tool_use" {
                        used.insert(key(get(block, "id")));
                    }
                }
            }
        });
        calls
            .into_iter()
            .filter(|(name, _)| {
                // Only string ids are shared between files.
                !name.starts_with('s') || seen.insert(name.clone())
            })
            .map(|(name, mut call)| {
                call.tools = tools.get(&name).map_or(0, HashSet::len);
                call
            })
            .collect()
    }
}
