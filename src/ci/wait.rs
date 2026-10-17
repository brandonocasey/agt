//! `agt ci wait`: poll a pipeline until it finishes, then list its failed jobs.

use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::core::error::Error;

use super::api::{Gitlab, RUNNING, display, text, truthy};
use super::failures::runs_commit;
use super::table::{Out, table};

const API_RETRIES: u32 = 5;

/// What the last progress line said.
#[derive(PartialEq)]
enum Last {
    Nothing,
    NoPipeline,
    Pipeline(String, usize),
}

/// How the wait ended: the process exit status, with the message to print when it is an error.
pub enum Done {
    Status(u8),
    Error(String, u8),
}

/// Why a poll stopped: a failed API call is retried, anything else ends the wait.
enum Stop {
    Api(Error),
    Fatal(String),
}

impl From<Error> for Stop {
    fn from(error: Error) -> Self {
        Self::Api(error)
    }
}

/// `%H:%M:%S` in local time.
fn clock() -> String {
    // SAFETY: time(2) accepts a null pointer; localtime_r writes only into `parts`.
    let parts = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut parts: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut parts);
        parts
    };
    format!(
        "{:02}:{:02}:{:02}",
        parts.tm_hour, parts.tm_min, parts.tm_sec
    )
}

pub struct Wait<'a> {
    pub gitlab: &'a Gitlab,
    /// A pipeline id, or an MR iid when `merge_request` is set.
    pub number: u64,
    pub merge_request: bool,
    pub sha: Option<&'a str>,
    pub interval: u64,
    pub timeout: u64,
}

fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

pub fn run(wait: &Wait, out: &Out) -> Done {
    let gitlab = wait.gitlab;
    let mut number = wait.number;
    let mut merge_request = wait.merge_request;
    let pause = || thread::sleep(Duration::from_secs(wait.interval));
    let deadline = Instant::now() + Duration::from_secs(wait.timeout);
    let mut last = Last::Nothing;
    let mut errors = 0;
    let (pipeline, finals) = loop {
        if Instant::now() > deadline {
            return Done::Error(format!("timed out after {}s", wait.timeout), 3);
        }
        let polled: Result<Option<(Value, Vec<Value>)>, Stop> = (|| {
            if merge_request {
                // GitLab updates the MR and its head_pipeline asynchronously after a push;
                // until then they describe the previous commit. --sha names the commit that
                // was pushed.
                let mr = gitlab.get(&format!("{}/merge_requests/{number}", gitlab.base))?;
                let head = mr.get("head_pipeline").cloned().unwrap_or(Value::Null);
                let sha = wait
                    .sha
                    .map_or_else(|| text(&mr, "sha").to_owned(), str::to_owned);
                if !truthy(Some(&head)) && wait.sha.is_none() {
                    return Err(Stop::Fatal(format!("!{number} has no pipeline")));
                }
                let matches = runs_commit(gitlab, &head, &sha)?;
                errors = 0;
                if !matches {
                    if last != Last::NoPipeline {
                        out.line(&format!(
                            "{} waiting for a pipeline on {}",
                            clock(),
                            short(&sha)
                        ));
                        last = Last::NoPipeline;
                    }
                    return Ok(None);
                }
                number = head.get("id").and_then(Value::as_u64).unwrap_or(0);
                merge_request = false;
            }
            let pipeline = gitlab.get(&format!("{}/pipelines/{number}", gitlab.base))?;
            let jobs = gitlab.jobs(&Value::from(number))?;
            errors = 0;
            let finals = jobs
                .into_values()
                .filter_map(|attempts| attempts.last().cloned())
                .collect();
            Ok(Some((pipeline, finals)))
        })();
        match polled {
            Err(Stop::Fatal(message)) => return Done::Error(message, 2),
            Err(Stop::Api(error)) => {
                errors += 1;
                eprintln!("agt ci: {error} ({errors}/{API_RETRIES})");
                if errors >= API_RETRIES {
                    return Done::Status(2);
                }
                pause();
                continue;
            }
            Ok(None) => {
                pause();
                continue;
            }
            Ok(Some((pipeline, finals))) => {
                let finals: Vec<Value> = finals;
                let status = text(&pipeline, "status").to_owned();
                let done = finals
                    .iter()
                    .filter(|job| !RUNNING.contains(&text(job, "status")))
                    .count();
                let state = Last::Pipeline(status.clone(), done);
                if state != last {
                    out.line(&format!(
                        "{} {status} ({done}/{} jobs finished) {}",
                        clock(),
                        finals.len(),
                        text(&pipeline, "web_url")
                    ));
                    last = state;
                }
                if !RUNNING.contains(&status.as_str()) {
                    break (pipeline, finals);
                }
                pause();
            }
        }
    };
    let failed: Vec<Vec<String>> = finals
        .iter()
        .filter(|job| text(job, "status") == "failed" && !truthy(job.get("allow_failure")))
        .map(|job| {
            vec![
                text(job, "name").to_owned(),
                display(job.get("stage")),
                text(job, "web_url").to_owned(),
            ]
        })
        .collect();
    if !failed.is_empty() {
        table(out, &["failed job", "stage", "log"], &failed);
    }
    Done::Status(u8::from(text(&pipeline, "status") != "success"))
}
