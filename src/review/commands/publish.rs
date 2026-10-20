//! `publish`: plan what to post ([`plan`]), render the comments ([`render`]) and the summary
//! ([`summary`]), and run the writes ([`exec`]).

mod exec;
mod plan;
mod render;
mod summary;

use serde_json::{Value, json};

use super::sync::{self, Connection};
use super::{Ctx, random_id};
use crate::core::error::{Error, Result, fail};
use crate::forge::service::Capabilities;
use crate::store::model::{PostState, RemoteKind, RemoteThread, Target};
use exec::run_writes;
use plan::Planner;
use plan::skips::Skipped;

/// `publish` flags.
pub(super) struct Options {
    pub dry_run: bool,
    pub not_own: bool,
    pub findings: bool,
    pub only: Vec<String>,
    pub run: Option<String>,
}

pub(super) fn publish(ctx: &mut Ctx, pr: Option<&str>, options: &Options) -> Result<Value> {
    let Options {
        dry_run, not_own, ..
    } = *options;
    if options.findings && options.only.is_empty() {
        return fail("--findings posts on someone else's PR/MR only what --only <id,...> names");
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
        exec::change(ctx, &target, |target| {
            let run = target
                .runs
                .iter_mut()
                .find(|run| &run.id == selected && run.ended_at.is_some())
                .ok_or_else(|| Error(format!("completed run {selected} not found")))?;
            run.publication_target = Some(url);
            Ok(())
        })?;
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
    let (threads, skipped, expected) =
        planner(ctx, connection, &target, attempt, options)?.threads(&target);
    // Treat the threads above as posted so their replies and resolves show too.
    for (id, kind) in expected {
        if let Some(thread) = target.threads.iter_mut().find(|thread| thread.id == id) {
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
