//! `agt wait`: rerun a check until it succeeds, for at most `--max` seconds. Agents run it in
//! the background so they wake within `--max` (default 270 s, under the 5-minute prompt cache)
//! to check on a long job.

use std::ffi::OsString;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::Args;

use crate::core::error::{Error, Result};
use crate::core::output::{Output, TIMED_OUT};
use crate::core::process;

#[derive(Args)]
pub struct Cli {
    /// Give up after this many seconds, stopping any check still running and its children.
    #[arg(long, default_value_t = 270, value_parser = clap::value_parser!(u64).range(1..))]
    max: u64,
    /// Seconds between checks.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..))]
    every: u64,
    /// The check and its arguments, after `--`. Its output is discarded.
    #[arg(last = true, required = true, value_name = "CHECK")]
    check: Vec<OsString>,
}

fn passes(check: &[OsString], deadline: Instant) -> bool {
    let mut command = Command::new(&check[0]);
    command
        .args(&check[1..])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    process::status_until(&mut command, deadline).is_ok_and(|status| status.success())
}

pub fn run(cli: Cli) -> Result<Output> {
    let start = Instant::now();
    let deadline = start
        .checked_add(Duration::from_secs(cli.max))
        .ok_or_else(|| Error("maximum wait is too large".into()))?;
    loop {
        if Instant::now() < deadline && passes(&cli.check, deadline) && Instant::now() < deadline {
            return Ok(Output::Text(format!(
                "agt wait: done after {}s\n",
                start.elapsed().as_secs()
            )));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(Output::Text(format!(
                "agt wait: still running after {}s; check the job and re-arm\n",
                cli.max
            ))
            .exiting(TIMED_OUT));
        }
        std::thread::sleep(Duration::from_secs(cli.every).min(remaining));
    }
}
