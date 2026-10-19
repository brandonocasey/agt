//! `agt ci failures`: the failed and retried jobs of an MR's head pipeline, the failing specs
//! in their logs, and how often each failed on the default branch.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use serde_json::Value;

use crate::core::error::{Result, fail};

use super::api::{Jobs, Project, RUNNING, id_text, parallel};
use super::specs::{family, parse_specs};
use super::table::{Out, table};
use crate::core::transcripts::json::text;

/// `(state, final, job)` of a spec: whether it failed or only flaked, whether it failed in the
/// job's last attempt, and the job that last reported it.
type Spec = (&'static str, bool, Option<String>);

#[derive(Default)]
pub struct Entry {
    final_failed: bool,
    running: HashSet<String>,
    unparsed: HashSet<String>,
    failed_jobs: HashSet<String>,
    final_failed_jobs: HashSet<String>,
    last_status: IndexMap<String, String>,
    specs: IndexMap<String, Spec>,
}

fn status(attempt: &Value) -> &str {
    text(attempt, "status")
}

fn has_failed(attempts: &[Value]) -> bool {
    attempts.iter().any(|attempt| status(attempt) == "failed")
}

/// Failed attempts per job family in one pipeline. `running` names jobs whose retry has not
/// finished; `unparsed` names jobs whose failed last attempt shows no failed spec.
pub fn pipeline_failures(
    gitlab: &Project,
    by_name: &Jobs,
    families: Option<&HashSet<String>>,
) -> Result<IndexMap<String, Entry>> {
    let wanted: Vec<(&String, &Vec<Value>)> = by_name
        .iter()
        .filter(|(name, attempts)| {
            families.is_none_or(|set| set.contains(&family(name))) && has_failed(attempts)
        })
        .collect();
    let mut failed: Vec<(&String, &Value, bool)> = Vec::new();
    for (name, attempts) in &wanted {
        for (index, attempt) in attempts.iter().enumerate() {
            if status(attempt) == "failed" {
                failed.push((name, attempt, index + 1 == attempts.len()));
            }
        }
    }
    let traces: Vec<Result<String>> = parallel(&failed, 8, |(_, attempt, _)| {
        gitlab.trace(attempt.get("id").unwrap_or(&Value::Null))
    });
    let mut result: IndexMap<String, Entry> = IndexMap::new();
    for (name, attempts) in &wanted {
        let entry = result.entry(family(name)).or_default();
        entry.failed_jobs.insert((*name).clone());
        let last = attempts.last().map_or("", status);
        entry.last_status.insert((*name).clone(), last.to_owned());
        if last == "failed" {
            entry.final_failed = true;
            entry.final_failed_jobs.insert((*name).clone());
        }
        if RUNNING.contains(&last) {
            entry.running.insert((*name).clone());
        }
    }
    let mut final_parsed: HashMap<&str, IndexMap<String, &'static str>> = HashMap::new();
    let mut earlier: HashSet<(String, String)> = HashSet::new();
    for ((name, _, is_final), trace) in failed.iter().zip(traces) {
        let parsed = parse_specs(&trace?, &gitlab.path);
        let Some(entry) = result.get_mut(&family(name)) else {
            continue;
        };
        if *is_final {
            if !parsed.values().any(|state| *state == "failed") {
                entry.unparsed.insert((*name).clone());
            }
        } else {
            earlier.extend(
                parsed
                    .iter()
                    .filter(|(_, state)| **state == "failed")
                    .map(|(spec, _)| ((*name).clone(), spec.clone())),
            );
        }
        for (spec, state) in &parsed {
            let (old_state, old_final, _) = entry
                .specs
                .get(spec)
                .map_or(("flaky", false, None), |(s, f, j)| (*s, *f, j.clone()));
            let merged = if *state == "failed" || old_state == "failed" {
                "failed"
            } else {
                "flaky"
            };
            entry.specs.insert(
                spec.clone(),
                (
                    merged,
                    old_final || (*is_final && *state == "failed"),
                    Some((*name).clone()),
                ),
            );
        }
        if *is_final {
            final_parsed.insert(name, parsed);
        }
    }
    // When the last attempt failed without naming a spec (timeout, setup error), a spec that
    // failed earlier never passed on retry, unless the last attempt reports it as flaky or passed.
    for entry in result.values_mut() {
        let updates: Vec<(String, Spec)> = entry
            .specs
            .iter()
            .filter_map(|(spec, (state, is_final, job))| {
                let job = job.as_ref()?;
                let applies = !is_final
                    && entry.unparsed.contains(job)
                    && earlier.contains(&(job.clone(), spec.clone()))
                    && !final_parsed
                        .get(job.as_str())
                        .is_some_and(|parsed| parsed.contains_key(spec));
                applies.then(|| (spec.clone(), (*state, true, Some(job.clone()))))
            })
            .collect();
        entry.specs.extend(updates);
    }
    Ok(result)
}

fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

fn day(stamp: &Value) -> String {
    text(stamp, "created_at").chars().take(10).collect()
}

struct Loaded {
    history: IndexMap<String, Entry>,
    ran: HashSet<String>,
}

fn verdict_order(verdict: &str) -> usize {
    ["new", "pre-existing", "flaky", "unclear", "pending"]
        .iter()
        .position(|name| *name == verdict)
        .unwrap_or(usize::MAX)
}

pub fn run(gitlab: &Project, iid: u64, count: usize, out: &Out) -> Result<()> {
    let mr = gitlab.merge_request(iid)?;
    let pipeline = mr.get("head_pipeline").cloned().unwrap_or(Value::Null);
    if pipeline.is_null() {
        return fail(format!("!{iid} has no pipeline"));
    }
    let sha = text(&mr, "sha");
    out.line(&format!(
        "!{iid} {}\npipeline {} {} for {}: {}",
        text(&mr, "title"),
        id_text(&pipeline["id"]),
        text(&pipeline, "status"),
        short(sha),
        text(&pipeline, "web_url")
    ));
    if !gitlab.runs_commit(&pipeline, sha)? {
        out.line(&format!(
            "⚠ newest commit {} has no pipeline yet; this one is older",
            short(sha)
        ));
    }
    out.line("");
    let by_name = gitlab.jobs(pipeline.get("id").unwrap_or(&Value::Null))?;
    let flagged: Jobs = by_name
        .into_iter()
        .filter(|(_, attempts)| has_failed(attempts))
        .collect();
    if flagged.is_empty() {
        out.line("No failed or retried jobs.");
        return Ok(());
    }
    let mut rows: Vec<Vec<String>> = Vec::new();
    for (name, attempts) in &flagged {
        let last = &attempts[attempts.len() - 1];
        let note = if last["allow_failure"] == true && status(last) == "failed" {
            " (allowed to fail)"
        } else {
            ""
        };
        rows.push(vec![
            name.clone(),
            attempts.iter().map(status).collect::<Vec<_>>().join(" → "),
            format!("{}{note}", status(last)),
            text(last, "web_url").to_owned(),
        ]);
    }
    table(out, &["job", "attempts", "final", "log"], &rows);

    let mr_fail = pipeline_failures(gitlab, &flagged, None)?;
    let families: HashSet<String> = mr_fail.keys().cloned().collect();
    // Canceled and skipped pipelines say nothing about tests, so compare completed runs only.
    let listing = gitlab.finished_pipelines(100.min(count * 2))?;
    let pipelines: Vec<Value> = listing
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|p| matches!(text(p, "status"), "success" | "failed"))
        .take(count)
        .cloned()
        .collect();
    let loaded: Vec<Result<Loaded>> = parallel(&pipelines, 4, |p| {
        let jobs = gitlab.jobs(p.get("id").unwrap_or(&Value::Null))?;
        // Jobs that finished: rules, schedules, or path filters can leave a job out of a pipeline.
        let ran = jobs
            .iter()
            .filter(|(_, attempts)| {
                attempts
                    .last()
                    .is_some_and(|last| matches!(status(last), "success" | "failed"))
            })
            .map(|(name, _)| name.clone())
            .collect();
        Ok(Loaded {
            history: pipeline_failures(gitlab, &jobs, Some(&families))?,
            ran,
        })
    });
    let loaded: Vec<Loaded> = loaded.into_iter().collect::<Result<_>>()?;
    let total = loaded.len();
    let branch = &gitlab.default_branch;
    if let (Some(oldest), Some(newest)) = (pipelines.last(), pipelines.first()) {
        out.line(&format!(
            "\nCompared with the last {total} finished {branch} pipelines ({} to {}):",
            day(oldest),
            day(newest)
        ));
    } else {
        out.line(&format!("\nNo finished {branch} pipelines to compare."));
    }

    let mut rows: Vec<[String; 5]> = Vec::new();
    for (fam, info) in &mr_fail {
        // A job whose failed last attempt names no spec (timeout, setup or lint error) is
        // compared with the same job name on the default branch.
        let mut items: Vec<(String, Spec, Option<String>)> = info
            .specs
            .iter()
            .map(|(spec, found)| (spec.clone(), found.clone(), None))
            .collect();
        let mut unparsed: Vec<&String> = info.unparsed.iter().collect();
        unparsed.sort();
        items.extend(unparsed.into_iter().map(|job| {
            (
                format!("(whole job {job})"),
                ("failed", true, Some(job.clone())),
                Some(job.clone()),
            )
        }));
        if items.is_empty() {
            items.push((
                format!("(whole job {fam})"),
                ("flaky", false, None),
                Some(String::new()),
            ));
        }
        for (spec, (_, is_final, job), whole) in items {
            let hits: Vec<Option<&Entry>> =
                loaded.iter().map(|item| item.history.get(fam)).collect();
            // Compare with the newest pipeline that ran this job (or any job of the family).
            let latest: Option<&Entry> = loaded
                .iter()
                .find(|item| match whole.as_deref() {
                    Some(name) if !name.is_empty() => item.ran.contains(name),
                    _ => item.ran.iter().any(|name| family(name) == *fam),
                })
                .and_then(|item| item.history.get(fam));
            let (failed_in, latest_final) = match whole.as_deref() {
                None => (
                    hits.iter()
                        .flatten()
                        .filter(|hit| hit.specs.contains_key(&spec))
                        .count(),
                    latest
                        .and_then(|entry| entry.specs.get(&spec))
                        .is_some_and(|found| found.1),
                ),
                Some(name) if !name.is_empty() => (
                    hits.iter()
                        .flatten()
                        .filter(|hit| hit.failed_jobs.contains(name))
                        .count(),
                    latest.is_some_and(|entry| entry.final_failed_jobs.contains(name)),
                ),
                Some(_) => (
                    hits.iter().flatten().count(),
                    latest.is_some_and(|entry| entry.final_failed),
                ),
            };
            // A family-wide item (no job) checks every shard's last attempt.
            let ends: Vec<&str> = match &job {
                Some(job) => vec![info.last_status.get(job).map_or("success", String::as_str)],
                None => info.last_status.values().map(String::as_str).collect(),
            };
            let odd = ends
                .iter()
                .find(|end| !matches!(**end, "success" | "failed"));
            let retry_running = match &job {
                Some(job) => info.running.contains(job),
                None => !info.running.is_empty(),
            };
            let (verdict, reason) = if !is_final && retry_running {
                ("pending", "retry still running".to_owned())
            } else if let (false, Some(odd)) = (is_final, odd) {
                ("unclear", format!("retry ended {odd}"))
            } else if !is_final {
                ("flaky", "passed on retry in the MR".to_owned())
            } else if failed_in == 0 {
                ("new", String::new())
            } else if latest_final {
                ("pre-existing", format!("fails in latest {branch}"))
            } else {
                ("flaky", format!("failed in older or retried {branch} runs"))
            };
            rows.push([
                verdict.to_owned(),
                format!("{failed_in}/{total}"),
                fam.clone(),
                reason,
                spec,
            ]);
        }
    }
    rows.sort_by(|a, b| {
        (verdict_order(&a[0]), &a[2], &a[4]).cmp(&(verdict_order(&b[0]), &b[2], &b[4]))
    });
    let rows: Vec<Vec<String>> = rows.into_iter().map(Vec::from).collect();
    table(out, &["verdict", branch, "job", "why", "spec"], &rows);
    Ok(())
}
