//! `agt ci wait`: polls required GitHub checks or the GitLab head pipelines of PRs, MRs and
//! pipelines, and prints one JSON event per line when a target's state first appears or
//! changes, then a final event.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::classify::{State, blocked, failed_checks, is_query_error, is_waiting, status};
use super::poll::snapshot;
use super::target::Resolved;
use crate::core::clock;
use crate::core::output::{FAILED, INTERRUPTED, TERMINATED, TIMED_OUT};

pub struct Options {
    pub targets: Vec<Resolved>,
    pub timeout: Duration,
    pub interval: Duration,
    pub max_interval: Duration,
}

/// Exit status when a target needs action.
const BLOCKED: u8 = 1;

static SIGNAL: AtomicI32 = AtomicI32::new(0);
static CANCEL: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(signal: i32) {
    SIGNAL.store(signal, Ordering::SeqCst);
    CANCEL.store(true, Ordering::SeqCst);
}

fn install_handlers() {
    let handler = on_signal as extern "C" fn(i32) as libc::sighandler_t;
    // SAFETY: the handler only stores to atomics, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

fn signal_exit() -> u8 {
    if SIGNAL.load(Ordering::SeqCst) == libc::SIGINT {
        INTERRUPTED
    } else {
        TERMINATED
    }
}

fn cancelled() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

fn write_event(event: Value) {
    let mut fields = Map::new();
    fields.insert(
        "at".to_owned(),
        clock::now_iso_millis().unwrap_or_default().into(),
    );
    if let Value::Object(event) = event {
        fields.extend(event);
    }
    let mut line = Value::Object(fields).to_string();
    line.push('\n');
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

/// The wait before poll number `attempt` (from 0): the interval, doubling up to the maximum.
fn backoff(attempt: u32, interval: Duration, max: Duration) -> Duration {
    interval
        .saturating_mul(1_u32.checked_shl(attempt).unwrap_or(u32::MAX))
        .min(max)
}

fn sleep(duration: Duration) {
    let until = Instant::now() + duration;
    while !cancelled() {
        let now = Instant::now();
        if now >= until {
            return;
        }
        thread::sleep((until - now).min(Duration::from_millis(20)));
    }
}

struct Tracked {
    target: Resolved,
    /// The head the target is pinned to: its `@SHA`, else the head the first poll read.
    head: Option<String>,
    /// The pinned head came from `--sha` and the host may not show it yet.
    awaiting: bool,
    /// The last state written.
    prior: Option<State>,
    done: bool,
    /// Failed polls in a row that repeating may fix.
    transient_failures: u32,
    /// Unix seconds when the wait began.
    started: i64,
    /// Seconds into the wait at which the rollup went idle with a required context missing, while
    /// it has stayed so.
    idle_since: Option<i64>,
}

/// Transient failures in a row after which a target is reported as a query error.
const TRANSIENT_LIMIT: u32 = 5;

enum Polled {
    Read(State),
    /// A transient failure to try again, with nothing to report.
    Retry,
    /// The deadline passed or the wait was cancelled before the read finished.
    Interrupted,
}

/// Reads one target on its own thread, so a hung `gh` or `glab` cannot hold the wait past its
/// deadline or a signal.
fn poll(tracked: &mut Tracked, deadline: Instant) -> Polled {
    let (target, head, awaiting, idle_since) = (
        tracked.target.clone(),
        tracked.head.clone(),
        tracked.awaiting,
        tracked.idle_since,
    );
    let waited = clock::now_secs().map_or(0, |now| (now - tracked.started).max(0));
    let (send, receive) = channel();
    thread::spawn(move || {
        let _ = send.send(snapshot(&target, head, awaiting, waited, idle_since));
    });
    loop {
        let now = Instant::now();
        if cancelled() || now >= deadline {
            return Polled::Interrupted;
        }
        match receive.recv_timeout((deadline - now).min(Duration::from_millis(20))) {
            Ok((state, head, transient, idle_since)) => {
                tracked.head = head;
                tracked.idle_since = idle_since;
                tracked.transient_failures = if transient {
                    tracked.transient_failures + 1
                } else {
                    0
                };
                if transient && tracked.transient_failures < TRANSIENT_LIMIT {
                    return Polled::Retry;
                }
                if tracked.awaiting && !is_waiting(&state) && !is_query_error(&state) {
                    tracked.awaiting = false;
                }
                return Polled::Read(state);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let mut state = blocked("query_error");
                state["detail"] = json!("the poll stopped unexpectedly");
                return Polled::Read(state);
            }
        }
    }
}

/// Writes the final event, with the failed checks of each target, and returns `code`.
fn finish(tracked: &[Tracked], reason: &str, code: u8) -> u8 {
    let results: Vec<Value> = tracked
        .iter()
        .map(|tracked| {
            let state = tracked.prior.clone().unwrap_or_else(|| blocked(reason));
            let mut fields = Map::new();
            fields.insert("target".to_owned(), tracked.target.key.clone().into());
            fields.extend(state.as_object().cloned().unwrap_or_default());
            fields.insert("failed".to_owned(), failed_checks(&state).into());
            Value::Object(fields)
        })
        .collect();
    write_event(json!({"event": "final", "reason": reason, "results": results}));
    code
}

fn emit(tracked: &mut Tracked, state: &State) {
    let text = state.to_string();
    let previous = tracked.prior.as_ref().map(Value::to_string);
    if previous.as_deref() == Some(text.as_str()) {
        return;
    }
    let mut event = Map::new();
    event.insert(
        "event".to_owned(),
        if previous.is_some() {
            "change"
        } else {
            "initial"
        }
        .into(),
    );
    event.insert("target".to_owned(), tracked.target.key.clone().into());
    event.extend(state.as_object().cloned().unwrap_or_default());
    write_event(Value::Object(event));
    tracked.prior = Some(state.clone());
}

/// Polls until every target is complete, one needs action, the deadline passes or a signal
/// arrives; returns the exit status.
pub fn run(options: &Options) -> u8 {
    let deadline = Instant::now() + options.timeout;
    let started = clock::now_secs().unwrap_or(0);
    let mut tracked: Vec<Tracked> = options
        .targets
        .iter()
        .map(|target| Tracked {
            target: target.clone(),
            head: target.target.sha.clone(),
            awaiting: target.target.awaiting,
            prior: None,
            done: false,
            transient_failures: 0,
            started,
            idle_since: None,
        })
        .collect();
    install_handlers();
    for attempt in 0.. {
        if cancelled() {
            return finish(&tracked, "cancelled", signal_exit());
        }
        if Instant::now() >= deadline {
            return finish(&tracked, "deadline_exceeded", TIMED_OUT);
        }
        let mut blocker = false;
        let mut query_error = false;
        for entry in tracked.iter_mut().filter(|entry| !entry.done) {
            let state = match poll(entry, deadline) {
                Polled::Read(state) => state,
                Polled::Retry => continue,
                Polled::Interrupted => break,
            };
            emit(entry, &state);
            match status(&state) {
                "complete" => entry.done = true,
                "blocked" => {
                    blocker = true;
                    query_error |= is_query_error(&state);
                }
                _ => {}
            }
        }
        if cancelled() {
            return finish(&tracked, "cancelled", signal_exit());
        }
        if Instant::now() >= deadline {
            return finish(&tracked, "deadline_exceeded", TIMED_OUT);
        }
        if blocker {
            return finish(
                &tracked,
                "action_required",
                if query_error { FAILED } else { BLOCKED },
            );
        }
        if tracked.iter().all(|entry| entry.done) {
            return finish(&tracked, "checks_complete", 0);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        sleep(backoff(attempt, options.interval, options.max_interval).min(remaining));
    }
    unreachable!("the loop only returns")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_the_wait_up_to_the_maximum() {
        let wait = |attempt| backoff(attempt, Duration::from_secs(10), Duration::from_secs(35));
        let delays: Vec<u64> = [0, 1, 2, 3, 40]
            .iter()
            .map(|n| wait(*n).as_secs())
            .collect();
        assert_eq!(delays, [10, 20, 35, 35, 35]);
    }
}
