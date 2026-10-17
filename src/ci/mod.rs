//! `agt ci`: GitLab CI triage, a port of gl-ci. `failures` compares an MR's failing tests with
//! recent default-branch pipelines; `wait` polls a pipeline until it finishes.

mod api;
mod failures;
mod specs;
mod table;
mod wait;

use std::sync::LazyLock;

use clap::{Args, Subcommand};
use regex::Regex;

use crate::core::error::{Error, Result, fail};
use crate::core::output::Output;

use api::Gitlab;
use table::Out;

/// GitLab CI triage.
#[derive(Args)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Failed jobs and specs of an MR versus the default branch.
    ///
    /// <MR> is 123, !123, or an MR URL. Without -R or a URL, the project is the current
    /// directory's repository. It lists failed and retried jobs of the MR's head pipeline, the
    /// failing specs in their logs (Playwright, WebdriverIO, Karma, Jest), and how often each
    /// spec failed in the last N finished default-branch pipelines, including retried attempts.
    /// Verdicts:
    ///   new           never failed on the default branch
    ///   pre-existing  also fails in the latest default-branch pipeline that ran the job
    ///   flaky         passed on retry in the MR, or failed only in older or retried
    ///                 default-branch attempts (it may also have been fixed there)
    ///   pending       its job's retry is still running
    ///   unclear       its job's retry was canceled or skipped
    #[command(verbatim_doc_comment)]
    Failures {
        #[arg(value_name = "MR")]
        target: String,
        /// Default-branch pipelines to compare.
        #[arg(short = 'n', default_value_t = 10)]
        n: i64,
        /// group/project (default: current repository or the URL).
        #[arg(short = 'R', long = "repo")]
        repo: Option<String>,
    },
    /// Poll a pipeline until it finishes, then list its failed jobs.
    ///
    /// <PIPELINE> is an id or a pipeline URL; !123 or an MR URL waits for the MR's head
    /// pipeline, first for a pipeline on the MR's newest commit, or on --sha. Right after a
    /// push, pass --sha "$(git rev-parse HEAD)" because GitLab updates the MR asynchronously.
    /// Exit codes: 0 success, 1 failed, canceled, skipped, or blocked on a manual job, 2 usage
    /// or GitLab API error (wait retries 5 polls in a row first), 3 timeout.
    #[command(verbatim_doc_comment)]
    Wait {
        #[arg(value_name = "PIPELINE")]
        target: String,
        /// Seconds between polls.
        #[arg(long, default_value_t = 30)]
        interval: i64,
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 3600)]
        timeout: i64,
        /// With an MR, wait for the pipeline of this commit (use after a push).
        #[arg(long)]
        sha: Option<String>,
        /// group/project (default: current repository or the URL).
        #[arg(short = 'R', long = "repo")]
        repo: Option<String>,
    },
}

static URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https?://([^/]+)/(.+?)/-/(merge_requests|pipelines)/(\d+)")
        .expect("valid pattern")
});
static NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(!?)(\d+)$").expect("valid pattern"));

struct Target {
    host: Option<String>,
    project: Option<String>,
    merge_request: bool,
    number: u64,
}

/// An id, `!id` or GitLab URL; a bare id is an MR when `default_merge_request`.
fn parse_target(value: &str, default_merge_request: bool) -> Result<Target> {
    if let Some(found) = URL.captures(value) {
        return Ok(Target {
            host: Some(found[1].to_owned()),
            project: Some(found[2].to_owned()),
            merge_request: &found[3] == "merge_requests",
            number: found[4]
                .parse()
                .map_err(|_| Error("id out of range".into()))?,
        });
    }
    let Some(found) = NUMBER.captures(value) else {
        return fail(format!("expected an id, !id, or URL, got '{value}'"));
    };
    Ok(Target {
        host: None,
        project: None,
        merge_request: !found[1].is_empty() || default_merge_request,
        number: found[2]
            .parse()
            .map_err(|_| Error("id out of range".into()))?,
    })
}

fn connect(target: &Target, repo: Option<&str>) -> Result<Gitlab> {
    Gitlab::connect(repo.or(target.project.as_deref()), target.host.as_deref())
}

/// Runs the command; an error exits 2 with `agt ci: <message>`, as gl-ci does.
pub fn run(cli: Cli) -> Result<Output> {
    Ok(Output::Exit(match execute(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("agt ci: {error}");
            2
        }
    }))
}

fn execute(cli: Cli) -> Result<u8> {
    let out = Out;
    let code = match cli.command {
        Command::Failures { target, n, repo } => {
            if n < 1 {
                return fail("-n must be at least 1");
            }
            let target = parse_target(&target, true)?;
            let gitlab = connect(&target, repo.as_deref())?;
            if !target.merge_request {
                return fail("failures takes an MR, not a pipeline");
            }
            failures::run(
                &gitlab,
                target.number,
                usize::try_from(n).unwrap_or(1),
                &out,
            )?;
            0
        }
        Command::Wait {
            target,
            interval,
            timeout,
            sha,
            repo,
        } => {
            let target = parse_target(&target, false)?;
            let gitlab = connect(&target, repo.as_deref())?;
            let done = wait::run(
                &wait::Wait {
                    gitlab: &gitlab,
                    number: target.number,
                    merge_request: target.merge_request,
                    sha: sha.as_deref(),
                    interval: u64::try_from(interval).unwrap_or(0),
                    timeout: u64::try_from(timeout).unwrap_or(0),
                },
                &out,
            );
            match done {
                wait::Done::Status(code) => code,
                wait::Done::Error(message, code) => {
                    eprintln!("agt ci: {message}");
                    code
                }
            }
        }
    };
    Ok(code)
}
