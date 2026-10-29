//! Processes that agent sessions started and left behind. A tool that a Claude Code session
//! runs inherits `CLAUDE_PID`; once that session is gone and the process is a day old, it is
//! an orphan. A browser or browser-MCP server whose parent is gone (adopted by init) is one after
//! `BROWSER_MIN_AGE_SECS`, whatever harness started it, such as Codex, which sets no session
//! marker; with a running parent it waits a day like any tool. Only the user's own processes are read. On macOS the environment of Apple's own
//! binaries (such as `/bin/sleep`) is hidden, so those are never found.

use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::core::agents::{self, Proc, exists, pids, read, record_matches, session_dirs};
use crate::core::clock;
use crate::core::error::{Error, Result};
use crate::core::harness::Harness;

pub const MIN_AGE_SECS: i64 = clock::SECS_PER_DAY;
/// How long a browser or browser-MCP server adopted by init runs before it is an orphan: it holds
/// memory the whole time, so it is not left for a day.
pub const BROWSER_MIN_AGE_SECS: i64 = 300;
const GRACE: Duration = Duration::from_secs(10);
const PID_OVERRIDE: &str = "AGT_CLEANUP_PID";

pub struct Orphan {
    pub proc: Proc,
    pub age: i64,
    /// How the process was found to be left behind.
    pub cause: Cause,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cause {
    /// The Claude Code session that started it is gone.
    Session,
    /// Its parent exited and init adopted it.
    Parent,
    /// A child of a browser orphan, which would otherwise outlive its root.
    Child,
}

/// A browser-MCP server, or a browser driven over a pipe, which only its starter can reach.
/// The `chrome-devtools` daemon is detached on purpose and outlives its starter.
pub fn is_browser(cmd: &str) -> bool {
    !cmd.contains("daemon")
        && [
            "chrome-devtools-mcp",
            "firefox-devtools-mcp",
            "--remote-debugging-pipe",
            "-juggler-pipe",
            "--inspector-pipe",
        ]
        .iter()
        .any(|marker| cmd.contains(marker))
}

#[derive(PartialEq, Eq)]
enum Session {
    Alive,
    Gone,
    /// The pid still runs but no record says whose it is, or a record agt cannot read:
    /// leave its tools alone.
    Unknown,
}

/// Whether Claude process `pid` is still the session that started the tool. Its record
/// (`<pid>.json` in any config directory) goes away with the session, and its `procStart`
/// tells a reused pid apart. A session is gone only when its pid no longer runs, or every
/// record clearly names another process.
fn session(pid: i32) -> Session {
    let records: Vec<Value> = session_dirs()
        .into_iter()
        .map(|dir| dir.join(format!("{pid}.json")))
        .filter(|path| path.is_file())
        .map(|path| {
            std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or(Value::Null)
        })
        .collect();
    let Some(proc) = read(pid) else {
        // Not ours to read (another user's) or not running at all.
        return if exists(pid) {
            Session::Unknown
        } else {
            Session::Gone
        };
    };
    if records.is_empty() {
        return Session::Unknown;
    }
    let mut unsure = false;
    for record in &records {
        match record_matches(record, &proc) {
            Some(true) => return Session::Alive,
            Some(false) => {}
            None => unsure = true,
        }
    }
    if unsure {
        Session::Unknown
    } else {
        Session::Gone
    }
}

/// Harness processes themselves, which carry `CLAUDE_PID` too; versioned Claude installs run
/// under their version number, such as `2.1.302`.
fn is_harness(comm: &str) -> bool {
    Harness::ALL.iter().any(|harness| harness.command() == comm)
        || comm == "node-claude"
        || (comm.split('.').count() == 3
            && comm
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())))
}

/// Orphans: tools of a gone session at least `MIN_AGE_SECS` old, and browsers adopted by init
/// at least `BROWSER_MIN_AGE_SECS` old. The test-only `AGT_CLEANUP_PID` limits the sweep to
/// one pid, so tests never see the machine's own processes; `0` looks at none.
pub fn orphans(now: i64) -> Result<Vec<Orphan>> {
    let skip = agents::lineage();
    let pids = match std::env::var(PID_OVERRIDE) {
        Ok(value) => {
            let pid: i32 = value
                .parse()
                .map_err(|_| Error(format!("{PID_OVERRIDE} must be a pid, got {value}")))?;
            if pid > 0 { vec![pid] } else { Vec::new() }
        }
        Err(_) => pids(),
    };
    let procs = pids
        .into_iter()
        .filter(|pid| !skip.contains(pid))
        .filter_map(read)
        .filter(|proc| !is_harness(&proc.comm))
        .collect();
    Ok(classify(procs, now, |claude| {
        session(claude) == Session::Gone
    }))
}

/// How long `proc` must run before it counts as left behind. A browser whose parent is gone
/// (adopted by init) holds memory and waits minutes. One with a live parent waits a day like
/// any other tool: the `chrome-devtools` daemon's Chrome inherits the `CLAUDE_PID` of whichever
/// session started the daemon, yet other sessions may still drive it.
fn min_age(proc: &Proc) -> i64 {
    if proc.ppid == 1 && is_browser(&proc.args) {
        BROWSER_MIN_AGE_SECS
    } else {
        MIN_AGE_SECS
    }
}

/// The orphans among `procs`. `gone` says whether a Claude Code session pid is gone.
fn classify(procs: Vec<Proc>, now: i64, gone: impl Fn(i32) -> bool) -> Vec<Orphan> {
    let mut found: Vec<Orphan> = Vec::new();
    let mut all = Vec::new();
    for proc in procs {
        let age = now - proc.start;
        let cause = match proc.claude_pid.filter(|claude| *claude != proc.pid) {
            Some(claude) if gone(claude) => Some(Cause::Session),
            // A live or unknown session keeps its tools, whatever their parent.
            Some(_) => None,
            None if proc.ppid == 1 && is_browser(&proc.args) => Some(Cause::Parent),
            None => None,
        };
        if let Some(cause) = cause
            && age >= min_age(&proc)
        {
            found.push(Orphan { proc, age, cause });
        } else {
            all.push(proc);
        }
    }
    // What a browser orphan started goes with it: its renderers and helpers carry no marker.
    let mut roots: Vec<i32> = found
        .iter()
        .filter(|orphan| is_browser(&orphan.proc.args))
        .map(|orphan| orphan.proc.pid)
        .collect();
    while let Some(parent) = roots.pop() {
        for proc in all.iter().filter(|proc| proc.ppid == parent) {
            if found.iter().all(|orphan| orphan.proc.pid != proc.pid) {
                roots.push(proc.pid);
                found.push(Orphan {
                    age: now - proc.start,
                    proc: proc.clone(),
                    cause: Cause::Child,
                });
            }
        }
    }
    found.sort_by_key(|orphan| orphan.proc.pid);
    found
}

fn same(proc: &Proc) -> bool {
    read(proc.pid).is_some_and(|now| now.start_key == proc.start_key)
}

/// Sends SIGTERM to each process that is still the one planned, waits up to ten seconds, then
/// sends SIGKILL to those still running. Returns, per process, whether it was signalled.
pub fn stop(procs: &[Proc]) -> Vec<(Proc, std::result::Result<&'static str, String>)> {
    let mut results = Vec::new();
    let mut waiting = Vec::new();
    for proc in procs {
        if !same(proc) {
            results.push((
                proc.clone(),
                Err("already gone or its pid was reused".into()),
            ));
        } else if unsafe { libc::kill(proc.pid, libc::SIGTERM) } == 0 {
            waiting.push(proc.clone());
        } else {
            let error = std::io::Error::last_os_error();
            results.push((proc.clone(), Err(format!("SIGTERM: {error}"))));
        }
    }
    let deadline = Instant::now() + GRACE;
    while waiting.iter().any(same) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    for proc in waiting {
        let signal = if same(&proc) && unsafe { libc::kill(proc.pid, libc::SIGKILL) } == 0 {
            "SIGKILL"
        } else {
            "SIGTERM"
        };
        results.push((proc, Ok(signal)));
    }
    results
}

pub fn reason(orphan: &Orphan) -> String {
    let running = if orphan.age >= clock::SECS_PER_DAY {
        format!("{} day(s)", orphan.age / clock::SECS_PER_DAY)
    } else {
        format!("{} minute(s)", orphan.age / 60)
    };
    match orphan.cause {
        Cause::Session => format!(
            "Claude session {} is gone; running {running}: {}",
            orphan.proc.claude_pid.unwrap_or_default(),
            orphan.proc.cmd
        ),
        Cause::Parent => format!(
            "its parent process is gone; running {running}: {}",
            orphan.proc.cmd
        ),
        Cause::Child => format!(
            "started by an orphan browser process; running {running}: {}",
            orphan.proc.cmd
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_servers_and_pipe_browsers_are_recognised() {
        assert!(is_browser("node /x/chrome-devtools-mcp --headless"));
        assert!(is_browser(
            "/opt/chrome --headless=new --remote-debugging-pipe"
        ));
        assert!(is_browser("firefox -juggler-pipe"));
        assert!(is_browser("MiniBrowser --inspector-pipe"));
        assert!(!is_browser("chrome-devtools-mcp daemon start"));
        assert!(!is_browser("node scripts/dev-server.mjs"));
        // Real Chrome puts the marker after many flags, far past the displayed 80 characters.
        let long = format!(
            "/opt/google/chrome/chrome {} --remote-debugging-pipe",
            "--flag ".repeat(30)
        );
        assert!(is_browser(&long));
    }

    fn proc(pid: i32, ppid: i32, start: i64, args: &str, claude_pid: Option<i32>) -> Proc {
        Proc {
            pid,
            ppid,
            start,
            start_key: 0,
            comm: String::new(),
            cmd: args.chars().take(80).collect(),
            args: args.to_owned(),
            claude_pid,
        }
    }

    fn found(procs: Vec<Proc>, now: i64, gone: &[i32]) -> Vec<(i32, Cause)> {
        classify(procs, now, |claude| gone.contains(&claude))
            .into_iter()
            .map(|orphan| (orphan.proc.pid, orphan.cause))
            .collect()
    }

    const MCP: &str = "node /x/chrome-devtools-mcp --headless";

    #[test]
    fn a_browser_with_no_parent_waits_minutes_and_no_session_marker_is_needed() {
        let now = 10_000;
        assert_eq!(
            found(vec![proc(5, 1, 0, MCP, None)], now, &[]),
            [(5, Cause::Parent)]
        );
        // Too young.
        assert_eq!(found(vec![proc(5, 1, now - 100, MCP, None)], now, &[]), []);
        // A live parent keeps a browser with no marker.
        assert_eq!(found(vec![proc(5, 77, 0, MCP, None)], now, &[]), []);
        // Another tool without a marker is never found by its parent.
        assert_eq!(
            found(vec![proc(5, 1, 0, "node dev-server.mjs", None)], now, &[]),
            []
        );
    }

    #[test]
    fn the_daemons_browser_waits_a_day_while_its_parent_runs() {
        let chrome = "/opt/chrome --remote-debugging-pipe";
        let gone = [9];
        // Its parent (the daemon) runs, though the session that started it is gone.
        assert_eq!(
            found(vec![proc(5, 77, 0, chrome, Some(9))], 10_000, &gone),
            []
        );
        assert_eq!(
            found(
                vec![proc(5, 77, 0, chrome, Some(9))],
                MIN_AGE_SECS + 1,
                &gone
            ),
            [(5, Cause::Session)]
        );
        // With the parent gone too, minutes are enough.
        assert_eq!(
            found(vec![proc(5, 1, 0, chrome, Some(9))], 10_000, &gone),
            [(5, Cause::Session)]
        );
        // A live session keeps it whatever the age.
        assert_eq!(
            found(vec![proc(5, 1, 0, chrome, Some(9))], MIN_AGE_SECS * 3, &[]),
            []
        );
    }

    #[test]
    fn what_an_orphan_browser_started_goes_with_it() {
        let procs = vec![
            proc(5, 1, 0, MCP, None),
            proc(6, 5, 0, "renderer", None),
            proc(7, 6, 0, "helper", None),
            proc(8, 77, 0, "unrelated", None),
        ];
        assert_eq!(
            found(procs, 10_000, &[]),
            [(5, Cause::Parent), (6, Cause::Child), (7, Cause::Child)]
        );
        // Without an orphan root, its children stay.
        assert_eq!(
            found(vec![proc(6, 5, 0, "renderer", None)], 10_000, &[]),
            []
        );
    }
}
