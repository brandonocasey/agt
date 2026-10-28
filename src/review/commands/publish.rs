//! `publish`: plan what to post ([`plan`]), render the comments ([`render`]) and the summary
//! ([`summary`]), and run the writes ([`exec`]).

mod exec;
mod plan;
mod render;
mod summary;

use serde_json::{Value, json};

use super::sync::{self, Connection};
use super::{Ctx, random_id};
use crate::core::config::Config;
use crate::core::error::{Error, Result, fail};
use crate::forge::auto;
use crate::forge::service::Capabilities;
use crate::store::model::{PostState, RemoteKind, RemoteThread, Target};
use exec::run_writes;
use plan::Planner;
use plan::skips::Skipped;

/// `publish` flags.
pub(super) struct Options {
    pub dry_run: bool,
    pub not_own: bool,
    pub only: Vec<String>,
    /// The approval thread `--approve` just recorded: always posted, whatever `only` names.
    pub approval: Option<String>,
    pub run: Option<String>,
}

pub(super) fn publish(ctx: &mut Ctx, pr: Option<&str>, options: &Options) -> Result<Value> {
    let Options {
        dry_run, not_own, ..
    } = *options;
    if !dry_run {
        let config = Config::load_for_repo(&ctx.repo)?;
        if let Some(reason) = auto::kept_local(&config, "publish") {
            return Ok(json!({
                "dry_run": false,
                "posted": [],
                "approval": options.approval,
                "kept_local": reason,
            }));
        }
    }
    let connection = sync::connect(ctx, pr)?;
    let _publish_lock = if dry_run {
        None
    } else if options.run.is_some() {
        Some(
            ctx.store
                .try_publish_lock(&ctx.dir, &connection.key)?
                .ok_or_else(|| Error("publish is busy".into()))?,
        )
    } else {
        Some(ctx.store.publish_lock(&ctx.dir, &connection.key)?)
    };
    let plan = sync::plan(ctx, &connection)?;
    let author = plan.review.author.clone();
    if !not_own && author.as_deref() != Some(connection.login.as_str()) {
        return fail(format!(
            "{} is authored by {}, not {}; pass --not-own to post anyway",
            plan.review.url,
            author.as_deref().unwrap_or("an unknown user"),
            connection.login
        ));
    }
    if dry_run {
        let target = plan.preview(ctx, &connection);
        return dry(ctx, &connection, target, options);
    }
    sync::sync_planned(ctx, &connection, plan)?;
    ctx.selection.target = Some(connection.key.clone());
    if let Some(selected) = &options.run {
        let target = load(ctx, &connection)?;
        let url = target
            .review
            .as_ref()
            .ok_or_else(|| Error("synced target has no PR/MR".into()))?
            .url
            .clone();
        exec::change(ctx, &target, |target| select_run(target, selected, url))?;
    }
    // One attempt id per publish: sync claims a marked note only for a write of that attempt.
    let attempt = random_id().replacen("rv-", "at-", 1);
    let mut posted = Vec::new();
    let mut skipped = Vec::new();
    let target = load(ctx, &connection)?;
    let (writes, skips, _) =
        planner(ctx, &connection, &target, &attempt, options)?.threads(&target);
    skipped.extend(skips);
    run_writes(ctx, &connection, &target, writes, &attempt, &mut posted)?;
    sync::sync_with(ctx, &connection)?;
    let target = load(ctx, &connection)?;
    let (writes, skips) = planner(ctx, &connection, &target, &attempt, options)?.replies(&target);
    skipped.extend(skips);
    run_writes(ctx, &connection, &target, writes, &attempt, &mut posted)?;
    let target = load(ctx, &connection)?;
    let (writes, skips) =
        planner(ctx, &connection, &target, &attempt, options)?.records(&target)?;
    skipped.extend(skips);
    run_writes(ctx, &connection, &target, writes, &attempt, &mut posted)?;
    let synced = sync::sync_with(ctx, &connection)?;
    Ok(json!({
        "target": connection.key,
        "dry_run": false,
        "posted": posted,
        "skipped": skipped.iter().map(Skipped::view).collect::<Vec<_>>(),
        "sync": synced,
    }))
}

/// Marks the completed run `selected` as published to the PR/MR at `url`.
fn select_run(target: &mut Target, selected: &str, url: String) -> Result<()> {
    let missing = || Error(format!("completed run {selected} not found"));
    let publication = match target
        .runs
        .iter_mut()
        .find(|run| run.id == selected && run.ended_at.is_some())
    {
        Some(run) => &mut run.publication_target,
        None => {
            &mut target
                .qa_runs
                .iter_mut()
                .find(|run| run.id == selected && run.ended_at.is_some())
                .ok_or_else(missing)?
                .publication_target
        }
    };
    *publication = Some(url);
    Ok(())
}

fn load(ctx: &Ctx, connection: &Connection) -> Result<Target> {
    ctx.store
        .load(&ctx.dir, &connection.key)?
        .ok_or_else(|| Error(format!("target {} vanished during publish", connection.key)))
}

fn planner<'a>(
    ctx: &'a Ctx,
    connection: &'a Connection,
    target: &'a Target,
    attempt: &'a str,
    options: &'a Options,
) -> Result<Planner<'a>> {
    Planner::new(ctx, connection, target, attempt, options)
}

/// Every payload a publish would send, from an in-memory sync; nothing is written.
fn dry(ctx: &Ctx, connection: &Connection, mut target: Target, options: &Options) -> Result<Value> {
    // A dry run sends nothing, so its markers name no real attempt.
    let attempt = "<attempt>";
    // The real publish marks the selected run first, so the summary shows it.
    if let (Some(selected), Some(review)) = (&options.run, &target.review) {
        let url = review.url.clone();
        select_run(&mut target, selected, url)?;
    }
    let (threads, skipped, expected) =
        planner(ctx, connection, &target, attempt, options)?.threads(&target);
    // Treat the threads above as posted so their replies and resolves show too.
    for (id, kind) in expected {
        if let Some(thread) = target.thread_mut(&id) {
            thread.remote = Some(placeholder(&id, kind, connection.service.caps()));
            if let Some(first) = thread.comments.first_mut() {
                first.delivery.post = PostState::Posted;
                first.remote = Some(exec::own(
                    format!("<remote id of {}>", first.id),
                    None,
                    &connection.login,
                    None,
                ));
            }
        }
    }
    let planner = planner(ctx, connection, &target, attempt, options)?;
    let (replies, more) = planner.replies(&target);
    let (records, held) = planner.records(&target)?;
    let writes: Vec<Value> = threads
        .iter()
        .chain(&replies)
        .chain(&records)
        .flat_map(|write| write.views(connection))
        .collect();
    let skipped: Vec<Value> = skipped
        .iter()
        .chain(&more)
        .chain(&held)
        .map(Skipped::view)
        .collect();
    Ok(json!({
        "target": connection.key,
        "dry_run": true,
        "writes": writes,
        "skipped": skipped,
    }))
}

fn placeholder(id: &str, kind: RemoteKind, caps: &Capabilities) -> RemoteThread {
    RemoteThread::new(
        kind,
        format!("<remote id of {id}>"),
        kind != RemoteKind::General || caps.general_resolvable,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_selected_run_is_marked_as_published_here() {
        let mut target: Target = serde_json::from_value(json!({
            "version": 1, "revision": 1, "key": "k", "repo": "r", "kind": "branch",
            "branch": null, "branch_birth": null, "base": null, "head": null,
            "created_at": "t", "updated_at": "t", "review": null, "records": [], "threads": [],
            "runs": [{
                "id": "rv-1", "started_at": "t", "ended_at": "t", "passes": [],
                "repaired_head": null, "repair": [], "reveal": null, "record": null,
            }],
        }))
        .unwrap();
        select_run(&mut target, "rv-1", "https://h/pr/1".into()).unwrap();
        assert_eq!(
            target.runs[0].publication_target.as_deref(),
            Some("https://h/pr/1")
        );
        assert!(select_run(&mut target, "rv-2", "https://h/pr/1".into()).is_err());
    }
}
