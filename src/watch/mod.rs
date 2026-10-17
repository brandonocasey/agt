//! `agt watch`: polls required GitHub checks or the GitLab head pipeline of pull requests and
//! prints one JSON event per line when a target's state first appears or changes, then a final
//! event. A port of agent-watch with the same arguments, events and exit statuses.

mod args;
mod classify;
mod json;
mod snapshot;

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Args;
use serde_json::Value;

use crate::core::clock;
use crate::core::error::Result;
use crate::core::output::Output;
use crate::core::proc;

use args::{Options, Platform, Target, backoff_delay, parse_args, usage};
use classify::{State, blocked, reason, status};
use json::{Fields, J, render_fields};
use snapshot::{CmdResult, QUERY_TIMEOUT, Runner, snapshot_github, snapshot_gitlab};

/// Poll GitHub or GitLab checks until they finish, fail or change.
#[derive(Args)]
#[command(disable_help_flag = true, disable_version_flag = true)]
pub struct Cli {
    /// `--target OWNER/REPO#PR@SHA` (GitHub) or `GROUP/PROJECT!IID@SHA` (GitLab), repeatable,
    /// plus `--deadline-seconds`, `--interval-ms`, `--max-interval-ms` and `--help`.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "OPTIONS"
    )]
    args: Vec<String>,
}

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

fn tool(platform: Platform) -> OsString {
    let (name, default) = match platform {
        Platform::Gitlab => ("AGT_GLAB_BIN", "glab"),
        Platform::Github => ("AGT_GH_BIN", "gh"),
    };
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.into())
}

/// Runs the tool of one target, never past the deadline and never longer than 30 seconds.
struct Tool {
    program: OsString,
    deadline: Instant,
}

/// The deadline result for a run that was still going at the deadline, unless the watch was
/// cancelled. A tool that ended before it keeps its own outcome even when polling noticed that
/// only after the deadline.
fn settle(run: &proc::Run, deadline: Instant, cancelled: bool) -> Option<CmdResult> {
    let at_deadline = run.last_running.is_some_and(|seen| seen >= deadline);
    (at_deadline && !cancelled).then(CmdResult::deadline)
}

impl Runner for Tool {
    fn run(&mut self, args: &[String]) -> CmdResult {
        let now = Instant::now();
        if now >= self.deadline {
            return CmdResult::deadline();
        }
        let remaining = self.deadline - now;
        let timeout = remaining.min(QUERY_TIMEOUT);
        let run = proc::run(
            &self.program,
            args,
            &proc::Options {
                env: &[],
                timeout: Some(timeout),
                cancel: Some(&CANCEL),
            },
        );
        if let Some(result) = settle(&run, self.deadline, CANCEL.load(Ordering::SeqCst)) {
            return result;
        }
        let program = self.program.to_string_lossy().into_owned();
        let mut invocation = vec![program];
        invocation.extend(args.iter().cloned());
        CmdResult {
            deadline_exceeded: false,
            code: match (run.spawn_error, run.code) {
                (Some(name), _) => Value::from(name),
                (None, Some(code)) => Value::from(code),
                (None, None) => Value::from(1),
            },
            signal: run.signal,
            stdout: run.stdout,
            stderr: run.stderr,
            invocation,
            timed_out: run.timed_out,
        }
    }
}

fn timestamp() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let stamp = clock::format(i64::try_from(elapsed.as_secs()).unwrap_or(0));
    format!(
        "{}.{:03}Z",
        stamp.trim_end_matches('Z'),
        elapsed.subsec_millis()
    )
}

fn write_event(event: Fields) {
    let mut fields: Fields = vec![("at", J::str(&timestamp()))];
    fields.extend(event);
    let mut line = String::new();
    render_fields(&fields, &mut line);
    line.push('\n');
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

fn serialize(state: &State) -> String {
    let mut text = String::new();
    render_fields(state, &mut text);
    text
}

fn sleep(milliseconds: u64) {
    let until = Instant::now() + Duration::from_millis(milliseconds);
    while !CANCEL.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= until {
            return;
        }
        thread::sleep((until - now).min(Duration::from_millis(20)));
    }
}

fn signal_exit() -> u8 {
    if SIGNAL.load(Ordering::SeqCst) == libc::SIGINT {
        130
    } else {
        143
    }
}

struct Watch<'a> {
    options: &'a Options,
    prior: HashMap<String, State>,
    results: HashMap<String, State>,
    active: Vec<&'a Target>,
    finalized: bool,
}

impl<'a> Watch<'a> {
    fn finish(&mut self, reason: &str, code: u8) -> u8 {
        if self.finalized {
            return code;
        }
        self.finalized = true;
        for target in &self.active {
            let state = self
                .prior
                .get(&target.key)
                .cloned()
                .unwrap_or_else(|| blocked(reason));
            self.results.insert(target.key.clone(), state);
        }
        let results = self
            .options
            .targets
            .iter()
            .map(|target| {
                let mut fields: Fields = vec![("target", J::str(&target.key))];
                fields.extend(self.results.get(&target.key).cloned().unwrap_or_default());
                J::Obj(fields)
            })
            .collect();
        write_event(vec![
            ("event", J::str("final")),
            ("reason", J::str(reason)),
            ("results", J::Arr(results)),
        ]);
        code
    }

    fn cancelled(&self) -> bool {
        SIGNAL.load(Ordering::SeqCst) != 0
    }
}

fn watch(options: &Options) -> u8 {
    let deadline = Instant::now() + Duration::from_secs(options.deadline_seconds);
    let mut state = Watch {
        options,
        prior: HashMap::new(),
        results: HashMap::new(),
        active: options.targets.iter().collect(),
        finalized: false,
    };
    let mut gitlab_host: Option<String> = None;
    let mut attempt: u32 = 0;
    install_handlers();
    while !state.active.is_empty() {
        if state.cancelled() {
            return state.finish("cancelled", signal_exit());
        }
        if Instant::now() >= deadline {
            return state.finish("deadline_exceeded", 124);
        }
        let mut found_blocker = false;
        let mut deadline_hit = false;
        for target in state.active.clone() {
            let mut runner = Tool {
                program: tool(target.platform),
                deadline,
            };
            let snapshot = match target.platform {
                Platform::Gitlab => snapshot_gitlab(target, &mut runner, &mut gitlab_host),
                Platform::Github => snapshot_github(target, &mut runner),
            };
            if state.cancelled() {
                break;
            }
            if reason(&snapshot) == "deadline_exceeded" {
                deadline_hit = true;
                break;
            }
            let text = serialize(&snapshot);
            let previous = state.prior.get(&target.key).map(serialize);
            if previous.as_deref() != Some(text.as_str()) {
                let mut event: Fields = vec![
                    (
                        "event",
                        J::str(if previous.is_some() {
                            "change"
                        } else {
                            "initial"
                        }),
                    ),
                    ("target", J::str(&target.key)),
                ];
                event.extend(snapshot.clone());
                write_event(event);
                state.prior.insert(target.key.clone(), snapshot.clone());
            }
            match status(&snapshot) {
                "complete" => {
                    state.results.insert(target.key.clone(), snapshot);
                    state.active.retain(|active| active.key != target.key);
                }
                "blocked" => {
                    state.results.insert(target.key.clone(), snapshot);
                    found_blocker = true;
                }
                _ => {}
            }
        }
        if state.cancelled() {
            return state.finish("cancelled", signal_exit());
        }
        if deadline_hit {
            return state.finish("deadline_exceeded", 124);
        }
        if found_blocker {
            return state.finish("action_required", 2);
        }
        if state.active.is_empty() {
            return state.finish("checks_complete", 0);
        }
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            // Round up, so the last nap ends past the deadline and the loop stops there
            // instead of starting a tool run with under a millisecond left.
            .as_micros()
            .div_ceil(1000);
        let delay = u128::from(backoff_delay(
            attempt,
            options.interval_ms,
            options.max_interval_ms,
        ))
        .min(remaining);
        attempt += 1;
        sleep(u64::try_from(delay).unwrap_or(u64::MAX));
    }
    state.finish("checks_complete", 0)
}

pub fn run(cli: Cli) -> Result<Output> {
    let options = match parse_args(&cli.args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}\n{}", usage());
            return Ok(Output::Exit(64));
        }
    };
    if options.help {
        println!("{}", usage());
        return Ok(Output::Exit(0));
    }
    Ok(Output::Exit(watch(&options)))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::time::{Duration, Instant};

    use super::args::{backoff_delay, parse_args, parse_target};
    use super::snapshot::Runner;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn uses_bounded_exponential_backoff() {
        let delays: Vec<u64> = [0, 1, 2, 3, 20]
            .iter()
            .map(|attempt| backoff_delay(*attempt, 10, 35))
            .collect();
        assert_eq!(delays, [10, 20, 35, 35, 35]);
        assert_eq!(backoff_delay(u32::MAX, 1 << 52, 1 << 53), 1 << 53);
    }

    #[test]
    fn parses_github_and_nested_gitlab_targets() {
        let options = parse_args(&argv(&[
            "--target",
            "owner/repo#42@abc123",
            "--target",
            "group/sub/project!7@abc123",
        ]))
        .unwrap();
        let keys: Vec<_> = options
            .targets
            .iter()
            .map(|target| (target.platform, target.key.as_str()))
            .collect();
        assert_eq!(
            keys,
            [
                (super::args::Platform::Github, "owner/repo#42"),
                (super::args::Platform::Gitlab, "group/sub/project!7")
            ]
        );
        for bad in ["project!7@abc123", "group/project!0@abc123"] {
            let error = parse_target(bad).unwrap_err();
            assert!(error.starts_with("invalid target"), "{error}");
        }
    }

    #[test]
    fn rejects_bad_options_like_agent_watch() {
        let cases: [(&[&str], &str); 8] = [
            (&[], "at least one --target is required"),
            (&["--target"], "--target needs a value"),
            (&["--bogus", "1"], "unknown option: --bogus"),
            (
                &["--target", "o/r#1@a", "--interval-ms", "x"],
                "--interval-ms must be an integer",
            ),
            (
                &["--target", "o/r#1@a", "--interval-ms", "0"],
                "--interval-ms is out of range",
            ),
            (
                &[
                    "--target",
                    "o/r#1@a",
                    "--interval-ms",
                    "9",
                    "--max-interval-ms",
                    "8",
                ],
                "--interval-ms cannot exceed --max-interval-ms",
            ),
            (
                &["--target", "o/r#1@a", "--target", "o/r#1@b"],
                "duplicate target: o/r#1",
            ),
            (
                &[
                    "--target",
                    "o/r#1@a",
                    "--deadline-seconds",
                    "9007199254740992",
                ],
                "--deadline-seconds is out of range",
            ),
        ];
        for (input, message) in cases {
            assert_eq!(parse_args(&argv(input)).unwrap_err(), message);
        }
        assert!(parse_args(&argv(&["-h"])).unwrap().help);
        assert_eq!(
            parse_args(&argv(&["--target", "o/r#1@a", "--deadline-seconds", "0"]))
                .unwrap()
                .deadline_seconds,
            0
        );
    }

    #[test]
    fn settle_decides_the_deadline_from_when_the_tool_was_last_seen_running() {
        let deadline = Instant::now() + Duration::from_secs(60);
        let run = |seen: Option<Instant>, code: i32, timed_out: bool| crate::core::proc::Run {
            code: Some(code),
            last_running: seen,
            timed_out,
            ..crate::core::proc::Run::default()
        };
        let late = deadline + Duration::from_millis(2);
        let early = deadline - Duration::from_millis(2);
        // Still running at the deadline, then failing: the deadline's failure, not the tool's.
        let seen_late = run(Some(late), 70, false);
        assert!(
            super::settle(&seen_late, deadline, false)
                .unwrap()
                .deadline_exceeded
        );
        // Killed for its timeout at the deadline.
        let killed = run(Some(deadline), 1, true);
        assert!(
            super::settle(&killed, deadline, false)
                .unwrap()
                .deadline_exceeded
        );
        // Ended before the deadline though polling noticed it after: a real result.
        let finished = run(Some(early), 0, false);
        assert!(super::settle(&finished, deadline, false).is_none());
        // A cancelled watch keeps the run's own result.
        assert!(super::settle(&seen_late, deadline, true).is_none());
        // A tool that never started was never seen running.
        assert!(super::settle(&run(None, 0, false), deadline, false).is_none());
    }

    #[test]
    fn a_tool_still_running_when_a_short_timeout_ends_is_a_deadline() {
        let mut tool = super::Tool {
            program: OsString::from("sleep"),
            deadline: Instant::now() + Duration::from_millis(1),
        };
        assert!(tool.run(&["5".to_string()]).deadline_exceeded);
    }
}
