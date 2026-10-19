//! Background setup for a new worktree: a detached `agt worktree prepare --setup ...` child
//! runs the dependency install and the create hooks while `create` returns, and records its
//! progress in `agt-setup.json` in the worktree's own git directory, so the file goes away with
//! the worktree. `prepare --wait` reads it.

use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::info::toplevel;
use super::{emit, hooks, prepare};
use crate::core::error::{Error, Result, ctx, fail};
use crate::core::output::Output;
use crate::git::Repo;

const STATUS_FILE: &str = "agt-setup.json";

/// Exit status when `--max` passes first, as for `agt wait`.
const TIMED_OUT: u8 = 3;

/// How long a status without a child pid counts as starting.
const STARTING_GRACE: Duration = Duration::from_secs(30);

/// A step the background child runs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Prepare,
    Hooks,
}

impl Step {
    pub(super) fn parse(name: &str) -> Option<Self> {
        match name {
            "prepare" => Some(Self::Prepare),
            "hooks" => Some(Self::Hooks),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::Hooks => "hooks",
        }
    }
}

fn status_path(root: &Path) -> Result<PathBuf> {
    let git_dir = Repo::at(root).git(&["rev-parse", "--absolute-git-dir"])?;
    Ok(PathBuf::from(git_dir).join(STATUS_FILE))
}

fn write_status(file: &Path, value: &Value) -> Result<()> {
    let temp = file.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value).map_err(ctx("serialize the setup status"))?;
    fs::write(&temp, text + "\n").map_err(ctx(format!("write {}", temp.display())))?;
    fs::rename(&temp, file).map_err(ctx(format!("replace {}", file.display())))
}

fn read_status(file: &Path) -> Result<Option<Value>> {
    match fs::read(file) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(ctx(format!("parse {}", file.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => fail(format!("read {}: {error}", file.display())),
    }
}

/// The report `{"pid", "status", "log", "already_running": true}` of a background setup that
/// still runs in the worktree `root`.
pub(super) fn running(root: &Path) -> Result<Option<Value>> {
    let status = status_path(root)?;
    let Some(existing) = read_status(&status)? else {
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
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let log =
        prepare::output_dir()?.join(format!("worktree-setup-{}-{nanos}.log", std::process::id()));
    let sink = File::create(&log).map_err(ctx(format!("create {}", log.display())))?;
    let again = sink.try_clone().map_err(ctx("duplicate the log handle"))?;
    // The child cannot write its own start before this returns, and a late write here could
    // overwrite its finish; so the file exists first, with pid 0 for "starting".
    write_status(&status, &json!({"state": "running", "pid": 0, "log": log}))?;
    let list = steps.iter().map(|step| step.name()).collect::<Vec<_>>();
    let spawned = Command::new(std::env::current_exe().map_err(ctx("find the agt binary"))?)
        .args(["worktree", "prepare", "--setup", &list.join(",")])
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
pub(super) fn child(dir: &Path, steps: &[Step]) -> Result<Output> {
    let root = toplevel(dir)?;
    let file = status_path(&root)?;
    let log = read_status(&file)?
        .and_then(|value| value.get("log").cloned())
        .unwrap_or(Value::Null);
    let pid = std::process::id();
    write_status(&file, &json!({"state": "running", "pid": pid, "log": log}))?;
    let mut done = json!({"state": "done", "pid": pid, "log": log});
    let ok = match execute(&root, steps, &mut done) {
        Ok(ok) => ok,
        Err(error) => {
            done["error"] = json!(error.0);
            false
        }
    };
    done["ok"] = json!(ok);
    write_status(&file, &done)?;
    emit(done, ok)
}

fn execute(root: &Path, steps: &[Step], done: &mut Value) -> Result<bool> {
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
        return alive(pid);
    }
    fs::metadata(file)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < STARTING_GRACE)
}

fn alive(pid: u64) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    found || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Waits for the background setup of the worktree containing `dir` and prints its report.
pub(super) fn wait(dir: &Path, max: u64) -> Result<Output> {
    let root = toplevel(dir)?;
    let file = status_path(&root)?;
    let start = Instant::now();
    loop {
        let Some(status) = read_status(&file)? else {
            return fail(format!("no background setup for {}", root.display()));
        };
        if status["state"] == "done" {
            return emit(status.clone(), status["ok"] == true);
        }
        let pid = status["pid"].as_u64().unwrap_or(0);
        if !live(pid, &file) {
            let log = status["log"].as_str().unwrap_or("(none)");
            return Err(Error(if pid == 0 {
                format!("the background setup never started; see its log {log}")
            } else {
                format!(
                    "the background setup (pid {pid}) is gone without finishing; see its log {log}"
                )
            }));
        }
        if start.elapsed().as_secs() >= max {
            println!("agt worktree prepare --wait: setup still running after {max}s; wait again");
            return Ok(Output::Exit(TIMED_OUT));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
