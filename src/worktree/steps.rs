//! Running one user command of a worktree step (an install, a hook) with its output in a log.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use serde_json::{Value, json};

use crate::core::error::{Result, ctx};
use crate::core::{clock, local, process};

/// `$XDG_CACHE_HOME/agents/output`, creating it.
pub(super) fn output_dir() -> Result<PathBuf> {
    let dir = local::agents_cache()?.join("output");
    fs::create_dir_all(&dir).map_err(ctx(format!("create {}", dir.display())))?;
    Ok(dir)
}

/// Runs `command` in `root` with `env` added, logging its output to a `worktree-<prefix>-` file; the report says how it went.
pub(super) fn run_step(
    root: &Path,
    command: &[String],
    prefix: &str,
    index: usize,
    env: &[(&str, String)],
) -> Result<Value> {
    let nanos = clock::unix_nanos();
    let name = format!(
        "worktree-{prefix}-{}-{nanos}-{index}.log",
        std::process::id()
    );
    let log = output_dir()?.join(name);
    let sink = File::create(&log).map_err(ctx(format!("create {}", log.display())))?;
    let again = sink.try_clone().map_err(ctx("duplicate the log handle"))?;
    let started = Instant::now();
    let mut run = Command::new(&command[0]);
    run.args(&command[1..])
        .current_dir(root)
        // An inherited PORT must not leak into a step; the caller's `env` may set it.
        .env_remove("PORT")
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdout(sink)
        .stderr(again);
    let status = process::status(&mut run, None);
    let mut step = json!({
        "command": command.join(" "),
        "exit": status.as_ref().ok().and_then(|status| status.code()),
        "ok": status.as_ref().is_ok_and(|status| status.success()),
        "duration_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "log": log,
    });
    if let Err(error) = status {
        step["error"] = json!(format!("run {}: {error}", command[0]));
    }
    Ok(step)
}
