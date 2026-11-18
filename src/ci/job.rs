//! `agt ci log` prints one job's log. `agt ci retry` retries a job, or the failed jobs of a GitHub
//! run.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use super::target::{self, actions_url};
use crate::core::config::{Config, Kind};
use crate::core::error::{Error, Result, fail};
use crate::forge::auto::pr_write_allowed;
use crate::forge::select::at_location;
use crate::forge::transport::TransportError;
use crate::forge::{github, gitlab};
use crate::git::Repo;
use crate::store::model::Forge;

const NO_LOG: &str = " (or has no log)";

static GITLAB_JOB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https?://([^/\s]+)/([^\s?#]+?)/-/jobs/(\d+)(?:[/?#]\S*)?$")
        .expect("valid pattern")
});

/// What a target names on a forge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Subject {
    Job(u64),
    /// A GitHub Actions run.
    Run(u64),
}

/// A job or run with the host and repository it is in.
#[derive(Debug)]
pub struct Located {
    pub forge: Forge,
    pub host: String,
    pub repo: String,
    pub subject: Subject,
}

/// The job or run `arg` names. A bare job id is read in the tracker `agt pr` picks.
pub fn locate(arg: &str, remote: Option<&str>) -> Result<Located> {
    if let Some(run) = actions_url(arg) {
        let subject = match run.job {
            Some(job) => Subject::Job(job),
            None => Subject::Run(run.run),
        };
        return Ok(Located {
            forge: run.forge,
            host: run.host,
            repo: run.repo,
            subject,
        });
    }
    if let Some(found) = GITLAB_JOB.captures(arg) {
        return Ok(Located {
            forge: Forge::Gitlab,
            host: found[1].to_ascii_lowercase(),
            repo: found[2].to_owned(),
            subject: Subject::Job(id(&found[3])?),
        });
    }
    if arg.is_empty() || !arg.bytes().all(|byte| byte.is_ascii_digit()) {
        return fail(format!(
            "expected a GitLab job URL, a GitHub Actions job URL or a job id, got '{arg}'"
        ));
    }
    let job = id(arg)?;
    let (host, repo, forge) = target::tracker(None, remote)?;
    Ok(Located {
        forge,
        host,
        repo,
        subject: Subject::Job(job),
    })
}

fn id(text: &str) -> Result<u64> {
    match text.parse() {
        Ok(id) if id > 0 => Ok(id),
        _ => fail(format!("invalid job id: {text}")),
    }
}

fn forgejo_refusal(what: &str, at: &Located) -> Error {
    Error(format!(
        "agt does not support {what} on Forgejo; {}/{} is a Forgejo repository",
        at.host, at.repo
    ))
}

/// Names a job or run a host does not know, not the tool's raw error. `detail` follows the name.
fn explain(error: TransportError, at: &Located, detail: &str) -> Error {
    if error.status == Some(404) {
        let what = match at.subject {
            Subject::Job(id) => format!("job {id}"),
            Subject::Run(id) => format!("run {id}"),
        };
        return Error(format!(
            "{what} was not found in {}/{}{detail}",
            at.host, at.repo
        ));
    }
    error.into()
}

/// The last `tail` lines of the log of the job `at` names.
pub fn log(at: &Located, tail: usize) -> Result<String> {
    let text = match (at.forge, at.subject) {
        (Forge::Forgejo, _) => return Err(forgejo_refusal("reading job logs", at)),
        (_, Subject::Run(_)) => {
            return fail("agt ci log takes a job, not a run: give a job URL or id");
        }
        (Forge::Gitlab, Subject::Job(job)) => gitlab::connect(&at.host)
            .job_trace(&gitlab::project_path(&at.repo), job)
            .map_err(|error| explain(error, at, NO_LOG))?,
        (Forge::Github, Subject::Job(job)) => github::connect(&at.host)
            .job_log(&at.repo, job)
            .map_err(|error| explain(error, at, NO_LOG))?,
    };
    Ok(last_lines(&text, tail))
}

fn last_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = lines[lines.len().saturating_sub(count)..].join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Refuses a write to `host`/`repo` when its tracker's `push` was set to `off`. `agt pr` writes
/// are refused the same way.
fn allow_write(what: &str, at: &Located) -> Result<()> {
    let (target, push) = match Repo::discover() {
        Ok(checkout) => {
            let config = Config::load_for_repo(&checkout)?;
            let selected = at_location(&checkout, &config, Kind::PullRequests, &at.host, &at.repo)?;
            (selected.name(), selected.push)
        }
        Err(_) => (
            format!("{}/{}", at.host, at.repo),
            Config::load()?.push_for(Kind::PullRequests),
        ),
    };
    pr_write_allowed(what, &target, push)
}

/// Retries the job `at` names, or the failed jobs of a GitHub run.
pub fn retry(at: &Located) -> Result<Value> {
    if at.forge == Forge::Forgejo {
        return Err(forgejo_refusal("rerunning jobs", at));
    }
    allow_write("agt ci retry", at)?;
    let mut done = json!({"forge": at.forge, "host": at.host, "repo": at.repo});
    match (at.forge, at.subject) {
        (Forge::Gitlab, Subject::Job(job)) => {
            let started = gitlab::connect(&at.host)
                .retry_job(&gitlab::project_path(&at.repo), job)
                .map_err(|error| explain(error, at, ""))?;
            done["retried"] = json!("job");
            done["id"] = json!(job);
            done["new_job"] = json!({"id": started["id"], "url": started["web_url"], "status": started["status"]});
        }
        (_, Subject::Job(job)) => {
            github::connect(&at.host)
                .rerun_job(&at.repo, job)
                .map_err(|error| explain(error, at, ""))?;
            done["retried"] = json!("job");
            done["id"] = json!(job);
        }
        (_, Subject::Run(run)) => {
            github::connect(&at.host)
                .rerun_failed_jobs(&at.repo, run)
                .map_err(|error| explain(error, at, ""))?;
            done["retried"] = json!("failed_jobs");
            done["run"] = json!(run);
        }
    }
    Ok(done)
}
