//! Processes that agent sessions started and left behind. A tool that a Claude Code session
//! runs inherits `CLAUDE_PID`; once that session is gone and the process is a day old, it is
//! an orphan. Only the user's own processes are read. On macOS the environment of Apple's own
//! binaries (such as `/bin/sleep`) is hidden, so those are never found.

use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::core::agents::{self, Proc, exists, pids, read, record_matches, session_dirs};
use crate::core::error::{Error, Result};

pub const MIN_AGE_SECS: i64 = 86_400;
const GRACE: Duration = Duration::from_secs(10);
const PID_OVERRIDE: &str = "AGT_CLEANUP_PID";

pub struct Orphan {
    pub proc: Proc,
    pub age: i64,
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
    matches!(comm, "claude" | "codex" | "node-claude")
        || (comm.split('.').count() == 3
            && comm
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())))
}

/// Orphans at least `MIN_AGE_SECS` old. The test-only `AGT_CLEANUP_PID` limits the sweep to
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
    let mut found = Vec::new();
    for pid in pids {
        if skip.contains(&pid) {
            continue;
        }
        let Some(proc) = read(pid) else { continue };
        let Some(claude) = proc.claude_pid.filter(|claude| *claude != pid) else {
            continue;
        };
        if is_harness(&proc.comm) || session(claude) != Session::Gone {
            continue;
        }
        let age = now - proc.start;
        if age >= MIN_AGE_SECS {
            found.push(Orphan { proc, age });
        }
    }
    found.sort_by_key(|orphan| orphan.proc.pid);
    Ok(found)
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
    format!(
        "Claude session {} is gone; running {} day(s): {}",
        orphan.proc.claude_pid.unwrap_or_default(),
        orphan.age / 86_400,
        orphan.proc.cmd
    )
}
