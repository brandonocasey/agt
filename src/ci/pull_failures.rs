//! `agt ci failures` for a GitHub or Forgejo PR. It has no verdicts, which need GitLab's
//! default-branch pipelines.

use indexmap::IndexMap;
use serde_json::Value;

use crate::core::error::{Error, Result};
use crate::core::text::prefix;
use crate::core::transcripts::json::text;
use crate::forge::{forgejo, github};
use crate::store::model::Forge;

use super::api::{id_text, parallel};
use super::specs::parse_github_specs;
use super::table::{Out, table};
use super::target::Resolved;

/// Jobs whose logs are read at once.
const LOG_THREADS: usize = 8;

pub fn run(target: &Resolved, out: &Out) -> Result<()> {
    let (view, checks) = match target.target.platform {
        Forge::Github => {
            let pull =
                github::connect(&target.host).pull_checks(&target.repo, target.target.number)?;
            (pull.view, pull.all_checks)
        }
        Forge::Forgejo => {
            let pull =
                forgejo::connect(&target.host).pull_checks(&target.repo, target.target.number)?;
            (pull.view, pull.checks)
        }
        Forge::Gitlab => return Err(Error("a GitLab MR is read by its own path".to_owned())),
    };
    let blocking: Vec<&Value> = checks
        .iter()
        .filter(|check| matches!(check["bucket"].as_str(), Some("fail" | "cancel")))
        .collect();
    let cancelled = blocking
        .iter()
        .filter(|check| check["bucket"] == "cancel")
        .count();
    let pending = checks
        .iter()
        .filter(|check| check["bucket"] == "pending")
        .count();
    let cancelled_note = if cancelled > 0 {
        format!(", {cancelled} cancelled")
    } else {
        String::new()
    };
    out.line(&format!(
        "{} {}\nhead {}: {} failed{cancelled_note} of {} checks, {pending} still running\n",
        target.key,
        text(&view, "url"),
        prefix(text(&view, "headRefOid"), 8),
        blocking.len() - cancelled,
        checks.len(),
    ));
    if blocking.is_empty() {
        out.line("No failed or cancelled checks.");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = blocking
        .iter()
        .map(|check| {
            vec![
                id_or_dash(&check["id"]),
                text(check, "name").to_owned(),
                text(check, "state").to_owned(),
                text(check, "link").to_owned(),
            ]
        })
        .collect();
    table(out, &["id", "job", "state", "log"], &rows);
    if !out.is_open() {
        return Ok(());
    }
    match target.target.platform {
        Forge::Github => specs(target, &blocking, out),
        _ => out.line("\nagt does not read job logs on Forgejo, so no failing specs are listed."),
    }
    Ok(())
}

fn id_or_dash(id: &Value) -> String {
    if id.is_null() {
        "-".to_owned()
    } else {
        id_text(id)
    }
}

/// Prints the failing specs of the failed Actions jobs. A cancelled job's log is not read.
fn specs(target: &Resolved, blocking: &[&Value], out: &Out) {
    let jobs: Vec<(&str, u64)> = blocking
        .iter()
        .filter(|check| check["bucket"] == "fail")
        .filter(|check| text(check, "link").contains("/actions/runs/"))
        .filter_map(|check| Some((text(check, "name"), check["id"].as_u64()?)))
        .collect();
    let logs = parallel(&jobs, LOG_THREADS, |(_, id)| {
        github::connect(&target.host).job_log(&target.repo, *id)
    });
    let mut found: IndexMap<String, Vec<&str>> = IndexMap::new();
    for ((name, id), log) in jobs.iter().zip(logs) {
        match log {
            Ok(log) => {
                for spec in failing_specs(&log, &target.repo) {
                    found.entry(spec).or_default().push(name);
                }
            }
            Err(error) => eprintln!("agt: could not read the log of job {id} ({name}): {error}"),
        }
    }
    if found.is_empty() {
        return;
    }
    out.line("");
    let rows: Vec<Vec<String>> = found
        .into_iter()
        .map(|(spec, jobs)| vec![spec, jobs.join(", ")])
        .collect();
    table(out, &["spec", "jobs"], &rows);
}

fn failing_specs(log: &str, repo: &str) -> Vec<String> {
    parse_github_specs(log, repo)
        .into_iter()
        .filter(|(_, state)| *state == "failed")
        .map(|(spec, _)| spec)
        .collect()
}
