//! The transcripts a report reads: the roots or files its scope names, narrowed to its dates,
//! project and harness.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::core::harness::Harness;
use crate::core::transcripts::{
    Step, Transcript, Walker, claude_projects, codex_roots, is_scratch,
};
use crate::sessions::Scope;

fn modified(path: &Path) -> Option<f64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(match modified.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_secs_f64(),
        Err(before) => -before.duration().as_secs_f64(),
    })
}

fn wanted(scope: &Scope, harness: Harness) -> bool {
    scope.harness.includes(harness)
}

/// `~` and `~/...` as the home directory.
fn expand(text: &str, home: &Path) -> PathBuf {
    match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            PathBuf::from(format!("{}{rest}", home.display()))
        }
        _ => PathBuf::from(text),
    }
}

/// The transcripts in scope. Prints how many scratch transcripts it skipped.
pub fn transcripts(scope: &Scope) -> Vec<Transcript> {
    if !scope.files.is_empty() {
        return scope
            .files
            .iter()
            .map(|file| Transcript::of_file(PathBuf::from(file)))
            .filter(|transcript| wanted(scope, transcript.harness))
            .collect();
    }
    let home = &scope.home;
    // A root named on the command line can hold either harness; each file says which.
    let roots: Vec<(PathBuf, Option<Harness>)> = if scope.roots.is_empty() {
        claude_projects(home)
            .into_iter()
            .map(|root| (root, Some(Harness::Claude)))
            .chain(
                codex_roots(home)
                    .into_iter()
                    .map(|root| (root, Some(Harness::Codex))),
            )
            .collect()
    } else {
        scope
            .roots
            .iter()
            .map(|root| (expand(root, home), None))
            .collect()
    };
    let mut found = Vec::new();
    let mut skipped = 0;
    for (root, hint) in roots {
        if hint.is_some_and(|harness| !wanted(scope, harness)) {
            continue;
        }
        for step in Walker::new(vec![root.clone()]) {
            let Step::File(path) = step else {
                continue;
            };
            if modified(&path).is_none_or(|time| time < scope.cutoff) {
                continue;
            }
            let parts: Vec<String> = path
                .strip_prefix(&root)
                .iter()
                .flat_map(|relative| relative.components())
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            let transcript = Transcript::locate(path, &parts, hint);
            let outside = scope
                .project
                .as_ref()
                .is_some_and(|text| !transcript.project.contains(text.as_str()));
            if !wanted(scope, transcript.harness) || outside {
                continue;
            }
            if !scope.scratch && is_scratch(&transcript.project) {
                skipped += 1;
                continue;
            }
            found.push(transcript);
        }
    }
    if skipped > 0 {
        eprintln!("(skipped {skipped} scratch transcripts; --scratch includes them)");
    }
    found
}

#[derive(Clone, Copy, Default)]
pub struct Needs {
    pub calls: bool,
    pub tools: bool,
    pub gaps: bool,
    pub failures: bool,
    pub friction: bool,
}

pub struct Loaded {
    transcript: Transcript,
    records: Vec<serde_json::Value>,
}

impl std::ops::Deref for Loaded {
    type Target = Transcript;

    fn deref(&self) -> &Transcript {
        &self.transcript
    }
}

impl Loaded {
    pub fn records(&self, each: impl FnMut(&serde_json::Value)) {
        self.records.iter().for_each(each);
    }

    pub fn api_calls(
        &self,
        seen: &mut std::collections::HashSet<String>,
    ) -> Vec<crate::core::transcripts::Call> {
        Transcript::calls_from_records(&self.records, seen)
    }
}

fn compact(record: &serde_json::Value, needs: Needs) -> Option<serde_json::Value> {
    use crate::core::transcripts::json::{get, text};
    use serde_json::{Value, json};

    let kind = text(record, "type");
    let message = get(record, "message");
    let mut blocks = Vec::new();
    for block in get(message, "content").as_array().into_iter().flatten() {
        match text(block, "type") {
            "tool_use" if needs.calls || needs.tools || needs.gaps || needs.failures => {
                let mut summary = json!({"type": "tool_use", "id": get(block, "id")});
                if needs.tools {
                    summary["tool_key"] =
                        super::tools::tool_key(text(block, "name"), get(block, "input")).into();
                    summary["input_tokens"] =
                        (get(block, "input").to_string().chars().count() as f64 / 4.0).into();
                }
                if needs.gaps {
                    summary["gap_tool"] = super::tools::gap_tool(block).into();
                }
                if needs.failures {
                    summary["name"] = get(block, "name").clone();
                    summary["input_summary"] = super::events::input_summary(block).into();
                }
                blocks.push(summary);
            }
            "tool_result" if needs.tools || needs.failures => {
                let mut summary =
                    json!({"type": "tool_result", "tool_use_id": get(block, "tool_use_id")});
                if needs.tools {
                    summary["result_tokens"] =
                        super::tools::est_tokens(get(block, "content")).into();
                }
                if needs.failures {
                    super::events::compact_failure(block, &mut summary);
                }
                blocks.push(summary);
            }
            _ => {}
        }
    }
    let typed = needs
        .friction
        .then(|| super::events::user_text(record))
        .flatten();
    if blocks.is_empty()
        && !(kind == "assistant" && (needs.calls || needs.tools || needs.gaps))
        && !(kind == "system" && needs.tools && text(record, "subtype") == "compact_boundary")
        && typed.is_none()
    {
        return None;
    }
    let mut message_summary = serde_json::Map::new();
    for field in ["id", "model", "usage"] {
        if let Some(value) = message.get(field) {
            message_summary.insert(field.into(), value.clone());
        }
    }
    message_summary.insert("content".into(), Value::Array(blocks));
    Some(json!({
        "type": kind,
        "subtype": get(record, "subtype"),
        "timestamp": get(record, "timestamp"),
        "typed": typed,
        "message": message_summary,
    }))
}

pub fn load(scope: &Scope, needs: Needs) -> impl Iterator<Item = Loaded> + '_ {
    transcripts(scope).into_iter().map(move |transcript| {
        let mut records = Vec::new();
        transcript.records(&scope.since_iso, |record| {
            if let Some(summary) = compact(record, needs) {
                records.push(summary);
            }
        });
        Loaded {
            transcript,
            records,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_reuse_records_after_the_source_disappears() {
        let dir = tempfile::tempdir().expect("temporary transcript root");
        let path = dir.path().join("session.jsonl");
        fs::write(&path, concat!(
            r#"{"type":"assistant","timestamp":"2026-03-01","message":{"id":"response","model":"test","usage":{"input_tokens":10},"content":[]}}"#,
            "\n",
        )).expect("write transcript");
        let scope = Scope {
            since: "2026-01-01".into(),
            since_iso: "2026-01-01".into(),
            cutoff: 0.0,
            project: None,
            scratch: false,
            roots: Vec::new(),
            files: vec![path.to_string_lossy().into_owned()],
            top: 25,
            harness: crate::sessions::Which::All,
            home: dir.path().to_owned(),
        };
        let found: Vec<_> = load(
            &scope,
            Needs {
                calls: true,
                ..Needs::default()
            },
        )
        .collect();
        fs::remove_file(path).expect("remove original transcript");
        for _ in 0..2 {
            let mut count = 0;
            found[0].records(|_| count += 1);
            assert_eq!(count, 1);
            let mut seen = std::collections::HashSet::new();
            let calls = found[0].api_calls(&mut seen);
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].usage["input_tokens"], 10);
            assert!(found[0].api_calls(&mut seen).is_empty());
        }
    }
    #[test]
    fn summaries_discard_output_after_token_estimation_and_full_denial_classification() {
        let output = format!(
            "{} denied by a built-in Claude Code safety check",
            "x".repeat(1_000_000)
        );
        let record = serde_json::json!({
            "type": "user",
            "message": {"content": [{
                "type": "tool_result", "tool_use_id": "tool", "is_error": true,
                "content": output,
            }]},
        });
        assert!(
            compact(
                &record,
                Needs {
                    calls: true,
                    ..Needs::default()
                }
            )
            .is_none()
        );
        assert!(
            compact(
                &record,
                Needs {
                    friction: true,
                    ..Needs::default()
                }
            )
            .is_none()
        );
        let summary = compact(
            &record,
            Needs {
                tools: true,
                failures: true,
                friction: true,
                ..Needs::default()
            },
        )
        .expect("tool result summary");
        let block = &summary["message"]["content"][0];
        assert_eq!(block["denial"], "safety check");
        assert_eq!(block["result_tokens"], output.chars().count() as f64 / 4.0);
        assert!(summary["typed"].is_null());
        assert!(summary.to_string().len() < 500);
    }

    #[test]
    fn summaries_preserve_response_updates_and_distinct_tools() {
        let records = [
            serde_json::json!({"type": "assistant", "message": {
                "id": "response", "usage": {"input_tokens": 10},
                "content": [{"type": "tool_use", "id": "one", "input": {"command": "true"}}],
            }}),
            serde_json::json!({"type": "assistant", "message": {
                "id": "response", "usage": {"input_tokens": 20},
                "content": [{"type": "tool_use", "id": "two"}, {"type": "tool_use", "id": "one"}],
            }}),
        ];
        let summaries: Vec<_> = records
            .iter()
            .filter_map(|record| {
                compact(
                    record,
                    Needs {
                        calls: true,
                        ..Needs::default()
                    },
                )
            })
            .collect();
        let mut seen = std::collections::HashSet::new();
        let calls = Transcript::calls_from_records(&summaries, &mut seen);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].usage["input_tokens"], 20);
        assert_eq!(calls[0].tools, 2);
        assert_eq!(calls[0].model, "?");
        assert!(Transcript::calls_from_records(&summaries, &mut seen).is_empty());
    }
}
