//! `agt ps`: the Claude Code sessions and Codex processes running now.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::{Value, json};

use super::fmt::{grouped, round, table, tok};
use super::source::home;
use super::spend::session_cost;
use crate::core::agents::{self, CodexProc};
use crate::core::clock;
use crate::core::error::Result;
use crate::core::output::Output;
use crate::core::transcripts::{codex_sessions, session_files};
use crate::git::Repo;

/// One row per running Claude Code session (a record in `~/.claude*/sessions` whose process
/// still runs) and per running `codex` process. Sorted busy first, then longest in status.
/// Status is as Claude Code wrote it (`busy`, `idle`, `shell` or `waiting`). A Codex process
/// gets a session only when a rollout in `~/.codex/sessions` names its directory and
/// changed after it started; otherwise `note` says why not. Cost is USD so far, `null` when
/// the price file lists no model of the session.
#[derive(Args)]
pub struct Cli {
    /// Print rows as JSON.
    #[arg(long)]
    json: bool,
}

pub fn run(cli: Cli) -> Result<Output> {
    let now = clock::now_secs()?;
    let mut rows: Vec<Value> = Vec::new();
    let mut warned = false;
    for live in agents::live() {
        let record = &live.record;
        let text = |key: &str| record.get(key).and_then(Value::as_str).map(str::to_owned);
        let since = record.get("statusUpdatedAt").and_then(Value::as_i64);
        let mut row = json!({
            "harness": "claude",
            "pid": live.pid,
            "name": text("name"),
            "kind": text("kind"),
            "status": text("status"),
            "status_secs": since.map(|millis| (now - millis / 1000).max(0)),
            "waiting_for": text("waitingFor"),
            "cwd": live.cwd,
            "session": text("sessionId"),
            "note": null,
        });
        spend(&mut row, &mut warned);
        place(&mut row, &live.cwd);
        rows.push(row);
    }
    let codex = agents::codex_procs();
    let mut shared: HashMap<PathBuf, usize> = HashMap::new();
    for proc in &codex {
        if let Some(cwd) = &proc.cwd {
            *shared.entry(cwd.clone()).or_default() += 1;
        }
    }
    for proc in codex {
        let mut row = codex_row(
            &proc,
            shared.get(proc.cwd.as_deref().unwrap_or(Path::new(""))),
        );
        spend(&mut row, &mut warned);
        if let Some(cwd) = &proc.cwd {
            place(&mut row, cwd);
        }
        rows.push(row);
    }
    rows.sort_by_key(|row| {
        (
            row["status"] != "busy",
            std::cmp::Reverse(row["status_secs"].as_i64()),
        )
    });
    if cli.json {
        return Ok(Output::Json(json!({ "agents": rows })));
    }
    Ok(Output::Text(render(&rows)))
}

fn codex_row(proc: &CodexProc, same_cwd: Option<&usize>) -> Value {
    let mut row = json!({
        "harness": "codex",
        "pid": proc.proc.pid,
        "name": null,
        "kind": null,
        "status": null,
        "status_secs": null,
        "waiting_for": null,
        "cwd": proc.cwd,
        "session": null,
    });
    let note = match &proc.cwd {
        None => Some("the process's working directory cannot be read on this platform".to_owned()),
        Some(_) if same_cwd.is_some_and(|count| *count > 1) => {
            Some("several codex processes share this directory".to_owned())
        }
        Some(cwd) => {
            match codex_sessions(&home(), cwd, proc.proc.start)
                .into_iter()
                .next()
            {
                Some(session) => {
                    row["session"] = json!(session);
                    None
                }
                None => Some(
                    "no rollout for this directory changed since the process started".to_owned(),
                ),
            }
        }
    };
    row["note"] = json!(note);
    row
}

/// Adds model, context tokens and cost from the session's transcripts. When they or the price
/// file cannot be read the cost stays `null` and the first such failure is warned about.
fn spend(row: &mut Value, warned: &mut bool) {
    if let Err(error) = try_spend(row)
        && !*warned
    {
        eprintln!("warning: cost left unpriced: {error}");
        *warned = true;
    }
}

fn try_spend(row: &mut Value) -> Result<()> {
    row["model"] = Value::Null;
    row["context_tokens"] = Value::Null;
    row["usd"] = Value::Null;
    row["unpriced_calls"] = Value::Null;
    let Some(session) = row["session"].as_str().map(str::to_owned) else {
        return Ok(());
    };
    if session_files(&home(), &session).is_empty() {
        return Ok(());
    }
    let cost = session_cost(&session)?;
    if let Some((model, context)) = cost.last {
        row["model"] = json!(model);
        row["context_tokens"] = json!(context);
    }
    row["unpriced_calls"] = json!(cost.unpriced_calls);
    if cost.unpriced_calls < cost.calls {
        row["usd"] = json!(round(cost.usd, 4));
    }
    Ok(())
}

/// Adds the repository's main checkout, the branch, and the linked worktree when `cwd` is in one.
fn place(row: &mut Value, cwd: &Path) {
    row["repository"] = Value::Null;
    row["branch"] = Value::Null;
    row["worktree"] = Value::Null;
    if !cwd.is_dir() {
        return;
    }
    let repo = Repo::at(cwd);
    let Some(paths) = repo
        .git_opt(&[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ])
        .ok()
        .flatten()
    else {
        return;
    };
    let mut lines = paths.lines();
    let (Some(top), Some(common)) = (lines.next(), lines.next()) else {
        return;
    };
    let common = Path::new(common);
    let main = match common.file_name() {
        Some(name) if name == ".git" => common.parent().unwrap_or(common),
        _ => Path::new(top),
    };
    row["repository"] = json!(main);
    if main != Path::new(top) {
        row["worktree"] = json!(top);
    }
    row["branch"] = json!(
        repo.git_opt(&["symbolic-ref", "--quiet", "--short", "HEAD"])
            .ok()
            .flatten()
    );
}

/// A duration as its largest whole unit: `45s`, `12m`, `3h`, `2d`.
fn age(secs: i64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

fn render(rows: &[Value]) -> String {
    if rows.is_empty() {
        return "No agents are running.\n".into();
    }
    let home = home().to_string_lossy().into_owned();
    let cell = |row: &Value, key: &str| row[key].as_str().unwrap_or("-").to_owned();
    let table_rows: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            let place = row["worktree"]
                .as_str()
                .or_else(|| row["cwd"].as_str())
                .unwrap_or("-");
            let usd = match (row["usd"].as_f64(), row["unpriced_calls"].as_i64()) {
                (Some(usd), Some(0)) => grouped(usd, 2),
                (Some(usd), _) => format!("{}+", grouped(usd, 2)),
                (None, Some(_)) => "unpriced".into(),
                (None, None) => "-".into(),
            };
            vec![
                cell(row, "harness"),
                row["pid"].to_string(),
                cell(row, "name"),
                cell(row, "status"),
                row["status_secs"].as_i64().map_or("-".into(), age),
                cell(row, "branch"),
                match place.strip_prefix(&home) {
                    Some(rest) if home.len() > 1 => format!("~{rest}"),
                    _ => place.to_owned(),
                },
                cell(row, "model"),
                row["context_tokens"]
                    .as_i64()
                    .map_or("-".into(), |tokens| tok(tokens as f64)),
                usd,
                cell(row, "session").chars().take(8).collect(),
            ]
        })
        .collect();
    let mut out = String::new();
    table(
        &mut out,
        &[
            "agent", "pid", "name", "status", "for", "branch", "dir", "model", "ctx", "USD",
            "session",
        ],
        &table_rows,
    );
    for row in rows {
        if let Some(note) = row["note"].as_str() {
            out.push_str(&format!("pid {}: {note}\n", row["pid"]));
        }
        if let Some(waiting) = row["waiting_for"].as_str() {
            out.push_str(&format!("pid {} waits for: {waiting}\n", row["pid"]));
        }
    }
    out
}
