//! `agt ci`: `failures` compares an MR's failing tests with recent default-branch pipelines
//! (GitLab only); `wait` polls PRs, MRs and pipelines until their checks finish.

mod api;
mod classify;
mod failures;
mod poll;
mod specs;
mod table;
mod target;
mod wait;

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use clap::{Args, Subcommand};
use regex::Regex;

use crate::core::error::{Error, Result, fail};
use crate::core::output::Output;

use api::Project;
use table::Out;

/// GitHub and GitLab CI: wait for checks, and triage GitLab failures.
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
    /// Poll PRs, MRs and pipelines until their checks finish, fail or change.
    ///
    /// Each TARGET is GitHub OWNER/REPO#PR[@SHA], GitLab GROUP/PROJECT!IID[@SHA], a PR or MR URL,
    /// #PR or !IID in the current repository, or a GitLab pipeline id or URL. Without @SHA, the
    /// head read at the start is pinned. Right after a push, pass --sha "$(git rev-parse HEAD)"
    /// with one target to wait for checks on that commit first.
    ///
    /// Stdout gets one JSON event per line: `initial` and `change` per target, then `final` with
    /// each target's state and failed jobs or checks. Exit codes: 0 all targets passed or
    /// merged, 1 a target needs action (failed, canceled, skipped or manual, head changed,
    /// closed), 2 usage or query error, 3 timeout, 130 or 143 after SIGINT or SIGTERM.
    #[command(verbatim_doc_comment)]
    Wait {
        #[arg(value_name = "TARGET", required = true)]
        targets: Vec<String>,
        /// Give up after this many seconds.
        #[arg(long, value_name = "SEC", default_value_t = 3600.0, value_parser = seconds)]
        timeout: f64,
        /// Seconds before the second poll; each wait doubles up to --max-interval.
        #[arg(long, value_name = "SEC", default_value_t = 15.0, value_parser = positive)]
        interval: f64,
        /// The longest wait between polls, in seconds.
        #[arg(long, value_name = "SEC", default_value_t = 120.0, value_parser = positive)]
        max_interval: f64,
        /// Wait for checks on this commit (one target; use after a push).
        #[arg(long)]
        sha: Option<String>,
    },
}

fn seconds(text: &str) -> std::result::Result<f64, String> {
    match text.parse::<f64>() {
        Ok(value) if (0.0..=1e9).contains(&value) => Ok(value),
        _ => Err("expected a number of seconds".to_owned()),
    }
}

fn positive(text: &str) -> std::result::Result<f64, String> {
    seconds(text).and_then(|value| {
        if value > 0.0 {
            Ok(value)
        } else {
            Err("must be greater than 0".to_owned())
        }
    })
}

static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^!?(\d+)$").expect("valid pattern"));

struct Target {
    host: Option<String>,
    project: Option<String>,
    merge_request: bool,
    number: u64,
}

/// An id, `!id` or GitLab URL; a bare id is an MR.
fn parse_target(value: &str) -> Result<Target> {
    if let Some(found) = target::GITLAB_URL.captures(value) {
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
        merge_request: true,
        number: found[1]
            .parse()
            .map_err(|_| Error("id out of range".into()))?,
    })
}

fn connect(target: &Target, repo: Option<&str>) -> Result<Project> {
    Project::connect(repo.or(target.project.as_deref()), target.host.as_deref())
}

/// Runs the command; an error exits 2 with `agt ci: <message>`.
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
            let target = parse_target(&target)?;
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
            targets,
            timeout,
            interval,
            max_interval,
            sha,
        } => return wait_targets(&targets, timeout, interval, max_interval, sha),
    };
    Ok(code)
}

fn wait_targets(
    targets: &[String],
    timeout: f64,
    interval: f64,
    max_interval: f64,
    sha: Option<String>,
) -> Result<u8> {
    if interval > max_interval {
        return fail("--interval cannot exceed --max-interval");
    }
    let mut parsed = targets
        .iter()
        .map(|arg| target::parse(arg))
        .collect::<Result<Vec<_>>>()?;
    if let Some(sha) = sha {
        match parsed.as_mut_slice() {
            [only] if !only.pipeline => {
                only.sha = Some(sha);
                only.awaiting = true;
            }
            _ => return fail("--sha needs exactly one PR or MR target"),
        }
    }
    let resolved = parsed
        .into_iter()
        .map(target::resolve)
        .collect::<Result<Vec<_>>>()?;
    let mut seen = HashSet::new();
    if let Some(duplicate) = resolved.iter().find(|target| !seen.insert(&target.key)) {
        return fail(format!("duplicate target: {}", duplicate.key));
    }
    Ok(wait::run(&wait::Options {
        targets: resolved,
        timeout: Duration::from_secs_f64(timeout),
        interval: Duration::from_secs_f64(interval),
        max_interval: Duration::from_secs_f64(max_interval),
    }))
}
