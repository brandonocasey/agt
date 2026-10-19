//! `agt ps`: the Claude Code sessions and Codex processes running now.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::Args;
use serde_json::{Value, json};

use super::fmt::{grouped, round, table, tok};
use super::source::home;
use super::spend::session_cost;
use crate::core::agents::{self, CodexProc};
use crate::core::clock;
use crate::core::error::{Error, Result, ctx};
use crate::core::output::Output;
use crate::core::transcripts::{codex_sessions, session_files};
use crate::git::Repo;

/// One row per running Claude Code session (a record in `~/.claude*/sessions` whose process
/// still runs) and per running `codex` process. Sorted busy first, then longest in status.
/// Status is as Claude Code wrote it (`busy`, `idle`, `shell` or `waiting`). A Codex process
/// gets a session only when a rollout in `~/.codex/sessions` names its directory and
/// changed after it started; otherwise `note` says why not. Cost is USD so far, `null` when
/// the price file lists no model of the session. A matched Codex session is `busy` while its
/// rollout's last `task_started` has no later `task_complete` or `turn_aborted`, else `idle`.
///
/// `--watch` polls forever and prints one JSON line per event: `blocked` (a Claude Code session
/// has been `waiting` for `--blocked-after` seconds; once per waiting stretch), `cleared` (a
/// blocked session left `waiting`, `reason` `answered`, or a new waiting stretch replaced it,
/// `reason` `new_prompt`) and `ended` (a session's process is gone). `--notify` posts a
/// desktop notification for each `blocked` event. `--exec PROGRAM [-- ARGS...]` runs the
/// program (no shell) per event with the event JSON on stdin and `AGT_PS_EVENT`, `AGT_PS_PID`,
/// `AGT_PS_SESSION`, `AGT_PS_NAME`, `AGT_PS_CWD`, `AGT_PS_WAITING_FOR`, `AGT_PS_STATUS_SECS`
/// and `AGT_PS_REASON` (`cleared` only) in its environment, its stdout going to stderr. Handlers run one at a time and block polling
/// until they exit; one that exits non-zero only prints a warning.
#[derive(Args)]
pub struct Cli {
    /// Print rows as JSON.
    #[arg(long)]
    json: bool,
    /// Print an event line when a session is blocked, unblocked or ended, until interrupted.
    #[arg(long, conflicts_with = "json")]
    watch: bool,
    /// Seconds a session must have been waiting before it is blocked.
    #[arg(long, default_value_t = 60, requires = "watch")]
    blocked_after: i64,
    /// Seconds between polls.
    #[arg(long, default_value_t = 5, requires = "watch")]
    interval: u64,
    /// Post a desktop notification for each blocked session.
    #[arg(long, requires = "watch")]
    notify: bool,
    /// Run this program for each event.
    #[arg(long, requires = "watch")]
    exec: Option<String>,
    /// Arguments for the `--exec` program.
    #[arg(last = true, requires = "exec")]
    exec_args: Vec<String>,
    /// Stop after this many polls.
    #[arg(long, hide = true, requires = "watch")]
    max_polls: Option<u64>,
}

pub fn run(cli: Cli) -> Result<Output> {
    if cli.watch {
        watch(&cli)?;
        return Ok(Output::Exit(0));
    }
    let now = clock::now_secs()?;
    let mut warned = false;
    let mut rows = claude_rows(now, true, &mut warned);
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
            now,
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

/// One row per live Claude Code session; `detail` adds cost and place.
fn claude_rows(now: i64, detail: bool, warned: &mut bool) -> Vec<Value> {
    let mut rows = Vec::new();
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
        if detail {
            spend(&mut row, warned);
            place(&mut row, &live.cwd);
        }
        rows.push(row);
    }
    rows
}

fn codex_row(proc: &CodexProc, same_cwd: Option<&usize>, now: i64) -> Value {
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
                    if let Some(file) = session_files(&home(), &session).first() {
                        let (status, since) = codex_status(&file.path);
                        row["status"] = json!(status);
                        row["status_secs"] = json!(since.map(|at| (now - at).max(0)));
                    }
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

/// How much of a rollout's end is searched for its last turn event.
const ROLLOUT_TAIL: u64 = 256 * 1024;

/// `busy` or `idle` from the end of a Codex rollout, and the Unix seconds of the event that
/// decided it. A tail without a turn event is `idle` since an unknown time.
fn codex_status(path: &Path) -> (&'static str, Option<i64>) {
    let mut tail = Vec::new();
    if let Ok(mut file) = File::open(path)
        && let Ok(len) = file.metadata().map(|meta| meta.len())
        && file
            .seek(SeekFrom::Start(len.saturating_sub(ROLLOUT_TAIL)))
            .is_ok()
    {
        let _ = file.take(ROLLOUT_TAIL).read_to_end(&mut tail);
    }
    for line in tail.split(|byte| *byte == b'\n').rev() {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if record["type"] != "event_msg" {
            continue;
        }
        let busy = match record["payload"]["type"].as_str() {
            Some("task_started") => true,
            Some("task_complete" | "turn_aborted") => false,
            _ => continue,
        };
        let at = record["timestamp"]
            .as_str()
            .and_then(|stamp| clock::parse(stamp).ok());
        return (if busy { "busy" } else { "idle" }, at);
    }
    ("idle", None)
}

/// What `--watch` remembers of one Claude Code session between polls.
struct Seen {
    row: Value,
    blocked: bool,
    /// Unix seconds the current status began, when the record says.
    since: Option<i64>,
}

fn watch(cli: &Cli) -> Result<()> {
    let mut seen: HashMap<i64, Seen> = HashMap::new();
    let mut polls = 0;
    loop {
        let now = clock::now_secs()?;
        let mut rows = claude_rows(now, false, &mut false);
        let mut events = Vec::new();
        // A session missing from one poll while its process runs keeps its previous state.
        let gone = |pid: &i64| !agents::exists(i32::try_from(*pid).unwrap_or_default());
        for (pid, old) in &seen {
            if !rows.iter().any(|row| row["pid"] == *pid) && gone(pid) {
                events.push(event("ended", &old.row, now));
            }
        }
        seen.retain(|pid, _| rows.iter().any(|row| row["pid"] == *pid) || !gone(pid));
        for row in rows.drain(..) {
            let pid = row["pid"].as_i64().unwrap_or_default();
            let waiting = row["status"] == "waiting";
            let since = row["status_secs"].as_i64().map(|secs| now - secs);
            let mut blocked = seen.get(&pid).is_some_and(|old| old.blocked);
            // A new waiting stretch behind a blocked one ends the old stretch and starts afresh.
            if blocked
                && waiting
                && let Some(old) = seen.get(&pid)
                && old.since.is_some()
                && since.is_some()
                && old.since != since
            {
                let mut cleared = event("cleared", &old.row, now);
                cleared["reason"] = json!("new_prompt");
                events.push(cleared);
                blocked = false;
            }
            let now_blocked = if waiting {
                blocked
                    || row["status_secs"]
                        .as_i64()
                        .is_some_and(|secs| secs >= cli.blocked_after)
            } else {
                false
            };
            if now_blocked && !blocked {
                events.push(event("blocked", &row, now));
            } else if blocked && !waiting {
                let mut cleared = event("cleared", &row, now);
                cleared["reason"] = json!("answered");
                events.push(cleared);
            }
            seen.insert(
                pid,
                Seen {
                    row,
                    blocked: now_blocked,
                    since,
                },
            );
        }
        for event in &events {
            emit(cli, event)?;
        }
        polls += 1;
        if cli.max_polls.is_some_and(|max| polls >= max) {
            return Ok(());
        }
        clock::sleep(cli.interval);
    }
}

fn event(kind: &str, row: &Value, now: i64) -> Value {
    let mut event = row.clone();
    event["event"] = json!(kind);
    event["at"] = json!(clock::format(now));
    event
}

fn emit(cli: &Cli, event: &Value) -> Result<()> {
    let line = event.to_string();
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")
        .and_then(|()| out.flush())
        .map_err(ctx("write stdout"))?;
    drop(out);
    if cli.notify && event["event"] == "blocked" {
        notify(event);
    }
    if let Some(program) = &cli.exec
        && let Err(error) = exec(program, &cli.exec_args, event, &line)
    {
        eprintln!("warning: {error}");
    }
    Ok(())
}

/// Runs `program` with the event on stdin and in `AGT_PS_*` variables, and waits for it.
fn exec(program: &str, args: &[String], event: &Value, line: &str) -> Result<()> {
    let text = |key: &str| match &event[key] {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let mut child = Command::new(program)
        .args(args)
        .env("AGT_PS_EVENT", text("event"))
        .env("AGT_PS_PID", text("pid"))
        .env("AGT_PS_SESSION", text("session"))
        .env("AGT_PS_NAME", text("name"))
        .env("AGT_PS_CWD", text("cwd"))
        .env("AGT_PS_WAITING_FOR", text("waiting_for"))
        .env("AGT_PS_STATUS_SECS", text("status_secs"))
        .env("AGT_PS_REASON", text("reason"))
        .stdin(Stdio::piped())
        // The event stream on stdout stays one JSON line per event.
        .stdout(std::io::stderr())
        .spawn()
        .map_err(|error| Error(format!("run {program}: {error}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A program that ignores its input may exit before reading it.
        let _ = writeln!(stdin, "{line}");
    }
    let status = child
        .wait()
        .map_err(|error| Error(format!("wait for {program}: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error(format!("{program} exited with {status}")))
    }
}

/// Posts a desktop notification. The text travels as arguments, never through a shell or as
/// script source.
fn notify(event: &Value) {
    let who = event["name"]
        .as_str()
        .or_else(|| event["cwd"].as_str())
        .unwrap_or("a session");
    let title = format!("agt: {who} is blocked");
    let body = event["waiting_for"].as_str().unwrap_or("waiting for input");
    let mut command = if cfg!(target_os = "macos") {
        let mut command = Command::new("osascript");
        command
            .args([
                "-e",
                "on run argv",
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv)",
                "-e",
                "end run",
            ])
            .args([&title, body]);
        command
    } else {
        let mut command = Command::new("notify-send");
        command.args(["--", &title, body]);
        command
    };
    if let Err(error) = command.stdin(Stdio::null()).status() {
        eprintln!("warning: notification failed: {error}");
    }
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

/// `place` with a leading `home` directory written `~`. Only whole path components match, so
/// `/home/alice/x` is not under `/home/al`.
fn tilde(place: &str, home: &Path) -> String {
    match Path::new(place).strip_prefix(home) {
        Ok(rest) if home.components().count() > 1 && rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) if home.components().count() > 1 => format!("~/{}", rest.display()),
        _ => place.to_owned(),
    }
}

fn render(rows: &[Value]) -> String {
    if rows.is_empty() {
        return "No agents are running.\n".into();
    }
    let home = home();
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
                tilde(place, &home),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_is_abbreviated_by_whole_components_only() {
        let home = Path::new("/home/al");
        assert_eq!(tilde("/home/al/x/y", home), "~/x/y");
        assert_eq!(tilde("/home/al", home), "~");
        assert_eq!(tilde("/home/alice/x", home), "/home/alice/x");
        assert_eq!(tilde("/srv/x", home), "/srv/x");
        assert_eq!(tilde("-", home), "-");
        assert_eq!(tilde("/x", Path::new("/")), "/x");
    }
}
