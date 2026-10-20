//! `agt ps`: the Claude Code sessions and Codex processes running now.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use clap::Args;
use serde::Serialize;
use serde_json::{Value, json};

use super::fmt::{grouped, round, table, tok};
use super::spend::session_cost;
use crate::core::agents::{self, CodexProc};
use crate::core::clock;
use crate::core::error::{Result, ctx};
use crate::core::harness::Harness;
use crate::core::local;
use crate::core::output::Output;
use crate::core::transcripts::json::{SHORT_ID, head};
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
/// has been `waiting` for 60 seconds; once per waiting stretch), `cleared` (a blocked session
/// left `waiting`, `reason` `answered`, or a new waiting stretch replaced it, `reason`
/// `new_prompt`) and `ended` (a session's process is gone).
#[derive(Args)]
pub struct Cli {
    /// Print rows as JSON.
    #[arg(long)]
    json: bool,
    /// Print an event line when a session is blocked, unblocked or ended, until interrupted.
    #[arg(long, conflicts_with = "json")]
    watch: bool,
    /// Seconds between polls.
    #[arg(long, default_value_t = 5, requires = "watch")]
    interval: u64,
    /// Stop after this many polls.
    #[arg(long, hide = true, requires = "watch")]
    max_polls: Option<u64>,
}

/// One running agent.
#[derive(Clone, Serialize)]
struct Row {
    harness: &'static str,
    pid: i32,
    name: Option<String>,
    kind: Option<String>,
    status: Option<String>,
    status_secs: Option<i64>,
    waiting_for: Option<String>,
    cwd: Option<PathBuf>,
    session: Option<String>,
    note: Option<String>,
    /// Present when cost and place were asked for.
    #[serde(flatten)]
    spend: Option<Spend>,
    #[serde(flatten)]
    place: Option<Place>,
}

/// What a session's transcripts say it cost; all `null` when they cannot be priced.
#[derive(Clone, Default, Serialize)]
struct Spend {
    model: Option<String>,
    context_tokens: Option<i64>,
    usd: Option<f64>,
    unpriced_calls: Option<i64>,
}

/// Where a session works: its repository's main checkout, the branch, and the linked worktree.
#[derive(Clone, Default, Serialize)]
struct Place {
    repository: Option<PathBuf>,
    branch: Option<String>,
    worktree: Option<PathBuf>,
}

pub fn run(cli: Cli) -> Result<Output> {
    if cli.watch {
        watch(&cli)?;
        return Ok(Output::Exit(0));
    }
    let now = clock::now_secs()?;
    let home = local::home()?;
    let mut warned = false;
    let mut rows = claude_rows(now, Some(&home), &mut warned);
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
            &home,
        );
        row.spend = Some(spend(row.session.as_deref(), &home, &mut warned));
        row.place = proc.cwd.as_deref().map(place);
        rows.push(row);
    }
    rows.sort_by_key(|row| {
        (
            row.status.as_deref() != Some("busy"),
            std::cmp::Reverse(row.status_secs),
        )
    });
    if cli.json {
        return Ok(Output::Json(json!({ "agents": rows })));
    }
    Ok(Output::Text(render(&rows, &home)))
}

/// One row per live Claude Code session; with the `home` directory, cost and place are added.
fn claude_rows(now: i64, detail: Option<&Path>, warned: &mut bool) -> Vec<Row> {
    let mut rows = Vec::new();
    for live in agents::live() {
        let record = &live.record;
        let text = |key: &str| record.get(key).and_then(Value::as_str).map(str::to_owned);
        let since = record.get("statusUpdatedAt").and_then(Value::as_i64);
        let session = text("sessionId");
        rows.push(Row {
            harness: Harness::Claude.id(),
            pid: live.pid,
            name: text("name"),
            kind: text("kind"),
            status: text("status"),
            status_secs: since.map(|millis| (now - millis / 1000).max(0)),
            waiting_for: text("waitingFor"),
            cwd: Some(live.cwd.clone()),
            spend: detail.map(|home| spend(session.as_deref(), home, warned)),
            place: detail.map(|_| place(&live.cwd)),
            session,
            note: None,
        });
    }
    rows
}

fn codex_row(proc: &CodexProc, same_cwd: Option<&usize>, now: i64, home: &Path) -> Row {
    let mut row = Row {
        harness: Harness::Codex.id(),
        pid: proc.proc.pid,
        name: None,
        kind: None,
        status: None,
        status_secs: None,
        waiting_for: None,
        cwd: proc.cwd.clone(),
        session: None,
        note: None,
        spend: None,
        place: None,
    };
    row.note = match &proc.cwd {
        None => Some("the process's working directory cannot be read on this platform".to_owned()),
        Some(_) if same_cwd.is_some_and(|count| *count > 1) => {
            Some("several codex processes share this directory".to_owned())
        }
        Some(cwd) => {
            match codex_sessions(home, cwd, proc.proc.start)
                .into_iter()
                .next()
            {
                Some(session) => {
                    if let Some(file) = session_files(home, &session).first() {
                        let (status, since) = codex_status(&file.path);
                        row.status = Some(status.to_owned());
                        row.status_secs = since.map(|at| (now - at).max(0));
                    }
                    row.session = Some(session);
                    None
                }
                None => Some(
                    "no rollout for this directory changed since the process started".to_owned(),
                ),
            }
        }
    };
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

/// Seconds a session must have been waiting before it is blocked.
const BLOCKED_AFTER: i64 = 60;

/// What `--watch` remembers of one Claude Code session between polls.
struct Seen {
    row: Row,
    blocked: bool,
    /// Unix seconds the current status began, when the record says.
    since: Option<i64>,
}

fn watch(cli: &Cli) -> Result<()> {
    let mut seen: HashMap<i32, Seen> = HashMap::new();
    let mut polls = 0;
    loop {
        let now = clock::now_secs()?;
        let rows = claude_rows(now, None, &mut false);
        for event in poll(&mut seen, rows, now, agents::exists) {
            emit(&event)?;
        }
        polls += 1;
        if cli.max_polls.is_some_and(|max| polls >= max) {
            return Ok(());
        }
        clock::sleep(cli.interval);
    }
}

/// The events one poll produces from the rows now running and what the last polls saw.
/// `exists` says whether a pid still runs.
fn poll(
    seen: &mut HashMap<i32, Seen>,
    mut rows: Vec<Row>,
    now: i64,
    exists: impl Fn(i32) -> bool,
) -> Vec<Event> {
    let mut events = Vec::new();
    // A session missing from one poll while its process runs keeps its previous state.
    let gone = |pid: &i32| !exists(*pid);
    for (pid, old) in &*seen {
        if !rows.iter().any(|row| row.pid == *pid) && gone(pid) {
            events.push(Event::of("ended", &old.row, now, None));
        }
    }
    seen.retain(|pid, _| rows.iter().any(|row| row.pid == *pid) || !gone(pid));
    for row in rows.drain(..) {
        let pid = row.pid;
        let waiting = row.status.as_deref() == Some("waiting");
        let since = row.status_secs.map(|secs| now - secs);
        let mut blocked = seen.get(&pid).is_some_and(|old| old.blocked);
        // A new waiting stretch behind a blocked one ends the old stretch and starts afresh.
        if blocked
            && waiting
            && let Some(old) = seen.get(&pid)
            && old.since.is_some()
            && since.is_some()
            && old.since != since
        {
            events.push(Event::of("cleared", &old.row, now, Some("new_prompt")));
            blocked = false;
        }
        let now_blocked =
            waiting && (blocked || row.status_secs.is_some_and(|secs| secs >= BLOCKED_AFTER));
        if now_blocked && !blocked {
            events.push(Event::of("blocked", &row, now, None));
        } else if blocked && !waiting {
            events.push(Event::of("cleared", &row, now, Some("answered")));
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
    events
}

/// A row and what happened to it, as a `--watch` line.
#[derive(Serialize)]
struct Event {
    #[serde(flatten)]
    row: Row,
    event: &'static str,
    at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

impl Event {
    fn of(event: &'static str, row: &Row, now: i64, reason: Option<&'static str>) -> Self {
        Self {
            row: row.clone(),
            event,
            at: clock::format(now),
            reason,
        }
    }
}

fn emit(event: &Event) -> Result<()> {
    let line = serde_json::to_string(event).map_err(ctx("encode event"))?;
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")
        .and_then(|()| out.flush())
        .map_err(ctx("write stdout"))
}

/// The model, context tokens and cost from the session's transcripts. When they or the price
/// file cannot be read the cost stays `null` and the first such failure is warned about.
fn spend(session: Option<&str>, home: &Path, warned: &mut bool) -> Spend {
    try_spend(session, home).unwrap_or_else(|error| {
        if !*warned {
            eprintln!("warning: cost left unpriced: {error}");
            *warned = true;
        }
        Spend::default()
    })
}

fn try_spend(session: Option<&str>, home: &Path) -> Result<Spend> {
    let mut spend = Spend::default();
    let Some(session) = session else {
        return Ok(spend);
    };
    if session_files(home, session).is_empty() {
        return Ok(spend);
    }
    let cost = session_cost(session)?;
    if let Some((model, context)) = cost.last {
        spend.model = Some(model);
        spend.context_tokens = Some(context);
    }
    spend.unpriced_calls = Some(cost.unpriced_calls);
    if cost.unpriced_calls < cost.calls {
        spend.usd = Some(round(cost.usd, 4));
    }
    Ok(spend)
}

/// The repository's main checkout, the branch, and the linked worktree when `cwd` is in one.
fn place(cwd: &Path) -> Place {
    let mut place = Place::default();
    if !cwd.is_dir() {
        return place;
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
        return place;
    };
    let mut lines = paths.lines();
    let (Some(top), Some(common)) = (lines.next(), lines.next()) else {
        return place;
    };
    let common = Path::new(common);
    let main = match common.file_name() {
        Some(name) if name == ".git" => common.parent().unwrap_or(common),
        _ => Path::new(top),
    };
    place.repository = Some(main.to_owned());
    if main != Path::new(top) {
        place.worktree = Some(PathBuf::from(top));
    }
    place.branch = repo.current_branch().ok().flatten();
    place
}

/// A duration as its largest whole unit: `45s`, `12m`, `3h`, `2d`.
fn age(secs: i64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / clock::SECS_PER_DAY),
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

fn render(rows: &[Row], home: &Path) -> String {
    if rows.is_empty() {
        return "No agents are running.\n".into();
    }
    let cell = |value: Option<&str>| value.unwrap_or("-").to_owned();
    let table_rows: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            let spend = row.spend.clone().unwrap_or_default();
            let place = row
                .place
                .as_ref()
                .and_then(|place| place.worktree.as_deref())
                .or(row.cwd.as_deref())
                .and_then(Path::to_str)
                .unwrap_or("-");
            let usd = match (spend.usd, spend.unpriced_calls) {
                (Some(usd), Some(0)) => grouped(usd, 2),
                (Some(usd), _) => format!("{}+", grouped(usd, 2)),
                (None, Some(_)) => "unpriced".into(),
                (None, None) => "-".into(),
            };
            vec![
                row.harness.to_owned(),
                row.pid.to_string(),
                cell(row.name.as_deref()),
                cell(row.status.as_deref()),
                row.status_secs.map_or("-".into(), age),
                cell(row.place.as_ref().and_then(|place| place.branch.as_deref())),
                tilde(place, home),
                cell(spend.model.as_deref()),
                spend
                    .context_tokens
                    .map_or("-".into(), |tokens| tok(tokens as f64)),
                usd,
                head(&cell(row.session.as_deref()), SHORT_ID).to_owned(),
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
        if let Some(note) = &row.note {
            out.push_str(&format!("pid {}: {note}\n", row.pid));
        }
        if let Some(waiting) = &row.waiting_for {
            out.push_str(&format!("pid {} waits for: {waiting}\n", row.pid));
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

    fn row() -> Row {
        Row {
            harness: Harness::Claude.id(),
            pid: 7,
            name: None,
            kind: None,
            status: Some("waiting".into()),
            status_secs: Some(3),
            waiting_for: None,
            cwd: Some(PathBuf::from("/w")),
            session: Some("abc".into()),
            note: None,
            spend: None,
            place: None,
        }
    }

    #[test]
    fn rows_and_events_keep_their_json_field_order() {
        let mut full = row();
        full.spend = Some(Spend::default());
        full.place = Some(Place::default());
        let keys =
            |value: &Value| -> Vec<String> { value.as_object().unwrap().keys().cloned().collect() };
        assert_eq!(
            keys(&json!(full)).join(","),
            "harness,pid,name,kind,status,status_secs,waiting_for,cwd,session,note,\
             model,context_tokens,usd,unpriced_calls,repository,branch,worktree"
        );
        let cleared = Event::of("cleared", &row(), 0, Some("answered"));
        assert_eq!(
            keys(&json!(cleared)).join(","),
            "harness,pid,name,kind,status,status_secs,waiting_for,cwd,session,note,event,at,reason"
        );
        let blocked = Event::of("blocked", &row(), 0, None);
        assert!(json!(blocked).get("reason").is_none());
    }

    fn waiting(secs: i64) -> Row {
        let mut waiting = row();
        waiting.status_secs = Some(secs);
        waiting
    }

    fn kinds(events: &[Event]) -> Vec<&'static str> {
        events.iter().map(|event| event.event).collect()
    }

    #[test]
    fn a_session_is_blocked_once_per_waiting_stretch_and_cleared_when_answered() {
        let mut seen = HashMap::new();
        assert!(poll(&mut seen, vec![waiting(10)], 1000, |_| true).is_empty());
        let events = poll(&mut seen, vec![waiting(70)], 1060, |_| true);
        assert_eq!(kinds(&events), ["blocked"]);
        assert!(poll(&mut seen, vec![waiting(100)], 1090, |_| true).is_empty());
        let mut idle = row();
        idle.status = Some("idle".into());
        let events = poll(&mut seen, vec![idle], 1120, |_| true);
        assert_eq!(kinds(&events), ["cleared"]);
        assert_eq!(events[0].reason, Some("answered"));
    }

    #[test]
    fn a_new_waiting_stretch_behind_a_blocked_one_clears_it_and_blocks_afresh() {
        let mut seen = HashMap::new();
        poll(&mut seen, vec![waiting(100)], 1000, |_| true);
        // Waiting 20s at +30 started after the old stretch began: a new prompt.
        let events = poll(&mut seen, vec![waiting(20)], 1030, |_| true);
        assert_eq!(kinds(&events), ["cleared"]);
        assert_eq!(events[0].reason, Some("new_prompt"));
        let events = poll(&mut seen, vec![waiting(80)], 1090, |_| true);
        assert_eq!(kinds(&events), ["blocked"]);
    }

    #[test]
    fn a_session_ends_only_when_its_process_is_gone() {
        let mut seen = HashMap::new();
        poll(&mut seen, vec![waiting(100)], 1000, |_| true);
        // Its record is missing from one poll but the process runs: no event, state kept.
        assert!(poll(&mut seen, Vec::new(), 1030, |_| true).is_empty());
        assert!(seen.contains_key(&7));
        let events = poll(&mut seen, Vec::new(), 1060, |_| false);
        assert_eq!(kinds(&events), ["ended"]);
        assert!(seen.is_empty());
    }
}
