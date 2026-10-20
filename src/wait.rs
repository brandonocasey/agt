//! `agt wait`: rerun a check until it succeeds, for at most `--max` seconds. Agents run it in
//! the background so they wake within `--max` (default 270 s, under the 5-minute prompt cache)
//! to check on a long job.

use std::ffi::OsString;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::Args;

use crate::core::error::Result;
use crate::core::output::{Output, TIMED_OUT};
use crate::core::process;

#[derive(Args)]
pub struct Cli {
    /// Give up after this many seconds.
    #[arg(long, default_value_t = 270, value_parser = clap::value_parser!(u64).range(1..))]
    max: u64,
    /// Seconds between checks.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..))]
    every: u64,
    /// The check and its arguments, after `--`. Its output is discarded.
    #[arg(last = true, required = true, value_name = "CHECK")]
    check: Vec<OsString>,
}

fn passes(check: &[OsString]) -> bool {
    let mut command = Command::new(&check[0]);
    command
        .args(&check[1..])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    process::status(&mut command, None).is_ok_and(|status| status.success())
}

pub fn run(cli: Cli) -> Result<Output> {
    let start = Instant::now();
    loop {
        if passes(&cli.check) {
            println!("agt wait: done after {}s", start.elapsed().as_secs());
            return Ok(Output::Exit(0));
        }
        if start.elapsed().as_secs() >= cli.max {
            println!(
                "agt wait: still running after {}s; check the job and re-arm",
                cli.max
            );
            return Ok(Output::Exit(TIMED_OUT));
        }
        std::thread::sleep(Duration::from_secs(cli.every));
    }
}
