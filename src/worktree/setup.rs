//! Background setup for a new worktree: a detached hidden `agt worktree setup --steps ...` child
//! runs the dependency install and the create hooks while `create` returns, and records its
//! progress in `agt-setup.json` in the worktree's own git directory, so the file goes away with
//! the worktree. `prepare --wait` reads it.

use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use clap::{Args, ValueEnum};
use serde_json::{Value, json};

use super::info::{dir_or_cwd, toplevel};
use super::{emit, hooks, prepare, steps};
use crate::core::error::{Error, Result, ctx, fail};
use crate::core::local::{read_json, write_json};
use crate::core::output::{Output, TIMED_OUT};
use crate::core::process::{self, Polled};
use crate::core::{agents, clock};
use crate::git::Repo;

const STATUS_FILE: &str = "agt-setup.json";

/// How long a status without a child pid counts as starting.
const STARTING_GRACE: Duration = Duration::from_secs(30);

/// The hidden entry point of the background child: `agt worktree setup --steps prepare,hooks [PATH]`.
#[derive(Args)]
pub struct Cli {
    /// The worktree; default: the current one.
    path: Option<PathBuf>,
    /// The steps to run, in the order `prepare`, `hooks`.
    #[arg(long, required = true, value_delimiter = ',', value_enum)]
    steps: Vec<Step>,
}

pub fn run(cli: Cli) -> Result<Output> {
    child(&dir_or_cwd(cli.path)?, &cli.steps)
}

/// A step the background child runs.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(super) enum Step {
    Prepare,
    Hooks,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Hooks => "hooks",
        }
    }
}

fn status_path(root: &Path) -> Result<PathBuf> {
    Ok(Repo::at(root).git_dir()?.join(STATUS_FILE))
}

/// The report `{"pid", "status", "log", "already_running": true}` of a background setup that
/// still runs in the worktree `root`.
pub(super) fn running(root: &Path) -> Result<Option<Value>> {
    let status = status_path(root)?;
    let Some(existing) = read_json::<Value>(&status)? else {
        return Ok(None);
    };
    let pid = existing["pid"].as_u64().unwrap_or(0);
    if existing["state"] != "running" || !live(pid, &status) {
        return Ok(None);
    }
    Ok(Some(json!({
        "pid": pid,
        "status": status,
        "log": existing["log"],
        "already_running": true,
    })))
}

/// Fails while a background setup runs in the worktree `root`, so its installs and hooks never
/// run twice at once.
pub(super) fn refuse_while_running(root: &Path) -> Result<()> {
    match running(root)? {
        Some(report) => fail(format!(
            "a background setup (pid {}) is still running in {}; wait for it with `agt worktree prepare --wait`",
            report["pid"],
            root.display()
        )),
        None => Ok(()),
    }
}

/// Starts the detached child for `steps` in the worktree `root` and returns the report
/// `{"pid", "status", "log"}`.
pub(super) fn spawn(root: &Path, steps: &[Step]) -> Result<Value> {
    if let Some(report) = running(root)? {
        return Ok(report);
    }
    let status = status_path(root)?;
    let nanos = clock::unix_nanos();
    let log =
        steps::output_dir()?.join(format!("worktree-setup-{}-{nanos}.log", std::process::id()));
    let sink = File::create(&log).map_err(ctx(format!("create {}", log.display())))?;
    let again = sink.try_clone().map_err(ctx("duplicate the log handle"))?;
    // The child cannot write its own start before this returns, and a late write here could
    // overwrite its finish; so the file exists first, with pid 0 for "starting".
    write_json(
        &status,
        &json!({"state": "running", "pid": 0, "log": log}),
        false,
    )?;
    let list = steps.iter().map(|step| step.name()).collect::<Vec<_>>();
    let spawned = Command::new(std::env::current_exe().map_err(ctx("find the agt binary"))?)
        .args(["worktree", "setup", "--steps", &list.join(",")])
        .arg(root)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(sink)
        .stderr(again)
        .process_group(0)
        .spawn();
    match spawned {
        Ok(child) => Ok(json!({"pid": child.id(), "status": status, "log": log})),
        Err(error) => {
            let _ = fs::remove_file(&status);
            fail(format!("start the background setup: {error}"))
        }
    }
}

/// The background child: runs `steps`, recording `running` then `done` in the status file.
fn child(dir: &Path, steps: &[Step]) -> Result<Output> {
    let root = toplevel(dir)?;
    let file = status_path(&root)?;
    let log = read_json::<Value>(&file)?
        .and_then(|value| value.get("log").cloned())
        .unwrap_or(Value::Null);
    let pid = std::process::id();
    write_json(
        &file,
        &json!({"state": "running", "pid": pid, "log": log}),
        false,
    )?;
    let mut done = json!({"state": "done", "pid": pid, "log": log});
    let ok = match execute(&root, steps, &mut done) {
        Ok(ok) => ok,
        Err(error) => {
            done["error"] = json!(error.0);
            false
        }
    };
    done["ok"] = json!(ok);
    write_json(&file, &done, false)?;
    emit(done, ok)
}

/// Runs `steps` in the worktree `root` in order, adding each report to `done` under the step's name;
/// the flag says every step succeeded.
pub(super) fn execute(root: &Path, steps: &[Step], done: &mut Value) -> Result<bool> {
    let mut ok = true;
    if steps.contains(&Step::Prepare) {
        let (report, passed) = prepare::prepare(root, false)?;
        done["prepare"] = report;
        ok &= passed;
    }
    if steps.contains(&Step::Hooks) {
        let (report, passed) = hooks::run(root, hooks::Kind::Create)?;
        done["hooks"] = report;
        ok &= passed;
    }
    Ok(ok)
}

/// Whether the run recorded in `file` with `pid` still runs. Pid 0 means the child has not
/// recorded itself yet; it does so at once, so a pid 0 older than `STARTING_GRACE` is a child
/// that died first or a parent killed before the spawn.
fn live(pid: u64, file: &Path) -> bool {
    if pid != 0 {
        return i32::try_from(pid).is_ok_and(agents::exists);
    }
    fs::metadata(file)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < STARTING_GRACE)
}

/// Waits for the background setup of the worktree containing `dir` and prints its report.
pub(super) fn wait(dir: &Path, timeout: u64) -> Result<Output> {
    let root = toplevel(dir)?;
    let file = status_path(&root)?;
    let polled = process::poll_until(Duration::from_secs(timeout), Duration::from_secs(1), |_| {
        let Some(status) = read_json::<Value>(&file)? else {
            return fail(format!("no background setup for {}", root.display()));
        };
        if status["state"] == "done" {
            return emit(status.clone(), status["ok"] == true).map(Some);
        }
        let pid = status["pid"].as_u64().unwrap_or(0);
        if live(pid, &file) {
            return Ok(None);
        }
        let log = status["log"].as_str().unwrap_or("(none)");
        Err(Error(if pid == 0 {
            format!("the background setup never started; see its log {log}")
        } else {
            format!("the background setup (pid {pid}) is gone without finishing; see its log {log}")
        }))
    })?;
    Ok(match polled {
        Polled::Done(output) => output,
        Polled::TimedOut => Output::Text(format!(
            "agt worktree prepare --wait: setup still running after {timeout}s; wait again\n"
        ))
        .exiting(TIMED_OUT),
    })
}
