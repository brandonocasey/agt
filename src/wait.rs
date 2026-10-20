//! `agt wait`: rerun a check until it succeeds, for at most `--timeout` seconds. Agents run it in
//! the background so they wake within `--timeout` (default 270 s, under the 5-minute prompt cache)
//! to check on a long job.

use std::ffi::OsString;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::Args;

use crate::core::error::{Error, Result};
use crate::core::output::{Output, TIMED_OUT};
use crate::core::process::{self, Polled};

#[derive(Args)]
pub struct Cli {
    /// Give up after this many seconds, stopping any check still running and its children.
    #[arg(long, alias = "max", default_value_t = 270, value_parser = clap::value_parser!(u64).range(1..=process::MAX_WAIT_SECS))]
    timeout: u64,
    /// Seconds between checks.
    #[arg(long, alias = "every", default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=process::MAX_WAIT_SECS))]
    interval: u64,
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
    let polled = process::poll_until(
        Duration::from_secs(cli.timeout),
        Duration::from_secs(cli.interval),
        |deadline| {
            // A check that passes after the deadline passed only because it was killed.
            Ok::<_, Error>(
                (passes(&cli.check, deadline) && Instant::now() < deadline).then_some(()),
            )
        },
    )?;
    Ok(match polled {
        Polled::Done(()) => Output::Text(format!(
            "agt wait: done after {}s\n",
            start.elapsed().as_secs()
        )),
        Polled::TimedOut => Output::Text(format!(
            "agt wait: still running after {}s; check the job and re-arm\n",
            cli.timeout
        ))
        .exiting(TIMED_OUT),
    })
}
