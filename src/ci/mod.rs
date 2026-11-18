//! `agt ci` triages failed jobs, waits for checks, prints job logs and retries jobs.

mod api;
mod classify;
mod failures;
mod job;
mod poll;
mod pull_failures;
mod specs;
mod table;
mod target;
mod wait;

use std::collections::HashSet;
use std::time::Duration;

use clap::{Args, Subcommand};

use crate::core::error::{Error, Result, fail};
use crate::core::output::{FAILED, Output};
use crate::forge::auto::Offline;
use crate::store::model::Forge;

use api::Project;
use table::Out;

/// GitHub, GitLab and Forgejo CI: wait for checks, triage failures, read job logs, retry jobs.
#[derive(Args)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Failed jobs and specs of a PR or MR (an MR versus the default branch).
    ///
    /// <MR> is 123, !123, group/project!123 or an MR URL. Without a group/project!N or a URL,
    /// the project is the current directory's repository. It lists the failed and retried jobs
    /// of the MR's head pipeline. It also lists the failing specs in their logs (Playwright,
    /// WebdriverIO, Karma, Jest). Each spec gets a verdict. It counts how often the spec failed
    /// in the last --limit finished default-branch pipelines. Retried attempts count too.
    ///
    /// A GitHub PR is OWNER/REPO#PR, #PR or a PR URL. It lists its failed checks with id, name
    /// and url. So does a Forgejo PR (a PR URL, or #PR in a Forgejo repository). On GitHub it
    /// also lists the failing specs in the logs of failed Actions jobs. agt does not read job
    /// logs on Forgejo, so it lists no specs there. A PR has no verdicts and ignores --limit.
    /// `agt ci log` and `agt ci retry` take the id of a GitHub check.
    ///
    /// GitLab verdicts:
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
        #[arg(long, default_value_t = 10)]
        limit: i64,
        /// Tracker for an MR given as a bare id or !id: a git remote name or `host/repo`;
        /// default: origin's tracker, else the only one.
        #[arg(long, value_name = "NAME|HOST/REPO")]
        remote: Option<String>,
    },
    /// Poll PRs, MRs and pipelines until their checks finish, fail or change.
    ///
    /// Each TARGET is GitHub OWNER/REPO#PR[@SHA], GitLab GROUP/PROJECT!IID[@SHA], a PR or MR URL,
    /// #PR or !IID in the current repository, or a GitLab pipeline id or URL. Without @SHA, the
    /// head read at the start is pinned. Right after a push, pass `<target>@$(git rev-parse HEAD)`
    /// to wait for checks on that commit first (a PR or MR; a pipeline pins it only).
    ///
    /// A commit is a target, so a push can be followed without a PR. OWNER/REPO@SHA is the
    /// checks and statuses of the commit on GitHub. GROUP/PROJECT@SHA is its pipelines on
    /// GitLab. @SHA is the commit in the current repository's tracker (GitHub, GitLab or
    /// Forgejo). SHA is a full or abbreviated commit id.
    /// A path of three or more parts is GitLab. A two-part path uses the forge of the current
    /// repository's tracker for it, else GitHub.
    /// A commit with no checks is pending for the first 120 seconds, then blocked. On GitLab
    /// the newest pipeline of each ref and source for the commit counts. Older superseded
    /// pipelines do not.
    /// A GitHub Actions run URL (https://github.com/OWNER/REPO/actions/runs/N) waits on that
    /// run's jobs.
    ///
    /// Stdout gets one JSON event per line: `initial` and `change` per target, then `final` with
    /// each target's state and failed jobs or checks. Exit codes: 0 all targets passed or
    /// merged, 1 a target needs action (failed, canceled, skipped or manual, head changed,
    /// closed), 2 usage or query error, 3 timeout, 130 or 143 after SIGINT or SIGTERM.
    ///
    /// With --job, only the jobs (on GitHub and Forgejo, the checks or commit statuses of the
    /// head, by exact context name) with those exact names count, whatever else the pipeline
    /// runs. A named job that is absent while checks still run is pending; once they are idle
    /// (GitLab: the pipeline stopped running) it is `job_not_found` (exit 1).
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
        /// Tracker for a target that names no repository (#PR, !IID, a pipeline id): a git
        /// remote name or `host/repo`; default: origin's tracker, else the only one.
        #[arg(long, value_name = "NAME|HOST/REPO")]
        remote: Option<String>,
        /// Wait only for the job or check with this exact name; repeat for several.
        #[arg(long = "job", value_name = "NAME")]
        jobs: Vec<String>,
    },
    /// Print the end of one job's log.
    ///
    /// <JOB> is one of:
    ///   a GitLab job URL (https://HOST/GROUP/PROJECT/-/jobs/N)
    ///   a GitHub Actions job URL (https://github.com/OWNER/REPO/actions/runs/N/job/M)
    ///   a job id in the current repository's tracker (GitLab or GitHub; see --remote)
    ///
    /// On GitHub the id is that of an Actions check run in `agt ci failures`.
    /// An unknown job exits 2. agt does not read job logs on Forgejo, so a Forgejo job is
    /// refused.
    #[command(verbatim_doc_comment)]
    Log {
        #[arg(value_name = "JOB")]
        job: String,
        /// How many of the last lines to print.
        #[arg(long, value_name = "N", default_value_t = 200, value_parser = at_least_one)]
        tail: usize,
        /// Tracker for a job id: a git remote name or `host/repo`.
        /// Default: origin's tracker, else the only one.
        #[arg(long, value_name = "NAME|HOST/REPO")]
        remote: Option<String>,
    },
    /// Retry a job, or rerun the failed jobs of a GitHub run.
    ///
    /// <TARGET> is one of:
    ///   a GitLab job URL or a job id (GitLab retries it)
    ///   a GitHub Actions job URL or a job id (GitHub reruns it)
    ///   a GitHub Actions run URL (GitHub reruns its failed jobs)
    ///
    /// A bare id is a job in the current repository's tracker (see --remote). Prints JSON that
    /// names what was retried. It writes to the host. It is refused offline, and when the
    /// tracker's `push` is explicitly "off". `agt pr` writes follow the same rule. agt does not
    /// rerun jobs on Forgejo, so a Forgejo target is refused.
    #[command(verbatim_doc_comment)]
    Retry {
        #[arg(value_name = "TARGET")]
        target: String,
        /// Tracker for a job id: a git remote name or `host/repo`.
        /// Default: origin's tracker, else the only one.
        #[arg(long, value_name = "NAME|HOST/REPO")]
        remote: Option<String>,
    },
}

fn at_least_one(text: &str) -> std::result::Result<usize, String> {
    match text.parse::<usize>() {
        Ok(count) if count > 0 => Ok(count),
        _ => Err("expected a whole number of at least 1".to_owned()),
    }
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

/// An MR or PR in any form `agt ci wait` takes. A bare id is an MR.
fn parse_target(value: &str) -> Result<target::Target> {
    let parsed = target::parse_as(value, false)
        .map_err(|_| {
            Error(format!(
                "expected an MR or PR (an id, !id, GROUP/PROJECT!IID, OWNER/REPO#PR, #PR or a PR or MR URL), got '{value}'"
            ))
        })?;
    if parsed.sha.is_some() {
        return fail("failures takes no @SHA");
    }
    Ok(parsed)
}

fn connect(target: &target::Target) -> Result<Project> {
    Project::connect(target.repo.as_deref(), target.host.as_deref())
}

/// Runs the command; an error exits 2 rather than 1.
pub fn run(cli: Cli) -> Result<Output> {
    Ok(execute(cli).unwrap_or_else(|error| Output::Failure(error, FAILED)))
}

fn execute(cli: Cli) -> Result<Output> {
    Offline::current().forbid("agt ci")?;
    let code = match cli.command {
        Command::Failures {
            target,
            limit,
            remote,
        } => {
            if limit < 1 {
                return fail("--limit must be at least 1");
            }
            failures_of(&parse_target(&target)?, limit, remote.as_deref())?;
            0
        }
        Command::Wait {
            targets,
            timeout,
            interval,
            max_interval,
            remote,
            jobs,
        } => {
            let pace = Pace {
                timeout,
                interval,
                max_interval,
            };
            wait_targets(&targets, &jobs, &pace, remote.as_deref())?
        }
        Command::Log { job, tail, remote } => {
            let at = job::locate(&job, remote.as_deref())?;
            return Ok(Output::Text(job::log(&at, tail)?));
        }
        Command::Retry { target, remote } => {
            let at = job::locate(&target, remote.as_deref())?;
            return Ok(Output::Json(job::retry(&at)?));
        }
    };
    Ok(Output::Exit(code))
}

/// Prints the failures of a GitLab MR, or of a GitHub or Forgejo PR.
fn failures_of(parsed: &target::Target, limit: i64, remote: Option<&str>) -> Result<()> {
    let out = Out::default();
    if parsed.platform != Forge::Gitlab {
        if parsed.pipeline {
            return fail("failures takes a PR, not an Actions run");
        }
        let resolved = target::resolve(parsed.clone(), remote)?;
        return pull_failures::run(&resolved, &out);
    }
    let mut parsed = parsed.clone();
    if remote.is_some() && parsed.repo.is_none() {
        let located = target::resolve(parsed.clone(), remote)?;
        parsed.host = Some(located.host);
        parsed.repo = Some(located.repo);
    }
    let gitlab = connect(&parsed)?;
    if parsed.pipeline {
        return fail("failures takes an MR, not a pipeline");
    }
    failures::run(
        &gitlab,
        parsed.number,
        usize::try_from(limit).unwrap_or(1),
        &out,
    )
}

/// The timing options of `agt ci wait`, in seconds.
struct Pace {
    timeout: f64,
    interval: f64,
    max_interval: f64,
}

fn wait_targets(
    targets: &[String],
    jobs: &[String],
    pace: &Pace,
    remote: Option<&str>,
) -> Result<u8> {
    if pace.interval > pace.max_interval {
        return fail("--interval cannot exceed --max-interval");
    }
    if jobs.iter().any(|job| job.trim().is_empty()) {
        return fail("--job needs a job name");
    }
    let resolved = targets
        .iter()
        .map(|arg| {
            let parsed = target::parse(arg)?;
            let mut resolved = target::resolve(parsed, remote)?;
            resolved.jobs = jobs.to_vec();
            Ok(resolved)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut seen = HashSet::new();
    if let Some(duplicate) = resolved.iter().find(|target| !seen.insert(&target.key)) {
        return fail(format!("duplicate target: {}", duplicate.key));
    }
    Ok(wait::run(&wait::Options {
        targets: resolved,
        timeout: Duration::from_secs_f64(pace.timeout),
        interval: Duration::from_secs_f64(pace.interval),
        max_interval: Duration::from_secs_f64(pace.max_interval),
    }))
}
