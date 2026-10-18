//! Auto-sync for the reads: before `list`, `show` and `status`, sync the target's PR/MR when
//! its last sync is older than the max age. A target with no known PR/MR, or whose PR/MR is
//! closed, is looked up by its branch on the forge at most once per max age, and a miss is
//! remembered in the repository's lookup file, as is a failure, which pauses that sync for the
//! max age.
//! No origin, or a host that is no known forge, skips quietly; any other failure is one line
//! on stderr and the read goes on with the local data.

use super::sync::{self, Connection};
use super::{Ctx, Scope};
use crate::core::clock;
use crate::core::error::{Error, Result};
use crate::forge::auto::{self, Policy};
use crate::store::model::{ReviewState, TargetKind};

/// Held while the repository syncs or looks a PR/MR up, so two reads do not repeat each other.
const SYNC_LOCK: &str = "auto-sync";

pub(super) fn sync_stale(ctx: &Ctx, offline: bool) -> Result<()> {
    let Some(policy) = Policy::resolve(offline) else {
        return Ok(());
    };
    // A scope that cannot be worked out is the read's own error to report.
    let Ok(scope) = ctx.scope() else {
        return Ok(());
    };
    let branch = branch_to_look_up(&scope);
    let known = scope
        .existing
        .as_ref()
        .and_then(|target| target.review.as_ref());
    // The PR/MR of a branch may be closed with a newer one open: look for it, as `sync` does.
    // Nothing is looked up for a target with an open PR/MR.
    let look = match (known, &branch) {
        (Some(review), Some(branch)) if review.state != ReviewState::Open => Some(branch),
        (None, Some(branch)) => Some(branch),
        _ => None,
    };
    let look_up_due = match look {
        Some(branch) => lookup_due(ctx, &policy, branch)?,
        None => false,
    };
    let sync_due = match known {
        Some(review) => {
            policy.is_stale(Some(&review.synced_at))?
                && !policy.backed_off(
                    &ctx.store,
                    &ctx.dir,
                    &format!("pr-{}", sync::key_of(review)),
                )?
        }
        None => false,
    };
    if !look_up_due && !sync_due {
        return Ok(());
    }
    // One deadline and at most one skip line for the whole read: a failed lookup does not stop
    // the sync of the known PR/MR, and the first failure is the one reported.
    auto::attempt(|| {
        let mut first: Option<Error> = None;
        if let (true, Some(branch)) = (look_up_due, look) {
            match look_up(ctx, branch, known.map(|review| review.number)) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => first = Some(error),
            }
        }
        if let (true, Some(review)) = (sync_due, known) {
            let key = sync::key_of(review);
            let failure = format!("pr-{key}");
            let synced = locked(ctx, Some(&key), &failure, |_| {
                Ok(Some(sync::connection_of(review)?))
            });
            if let Err(error) = synced {
                first.get_or_insert(error);
            }
        }
        first.map_or(Ok(()), Err)
    });
    Ok(())
}

/// Only a branch target has a PR/MR to find by its branch.
fn branch_to_look_up(scope: &Scope) -> Option<String> {
    if scope.kind != TargetKind::Branch {
        return None;
    }
    scope.branch.clone().filter(|branch| !branch.is_empty())
}

fn lookup_failure(branch: &str) -> String {
    format!("branch-{branch}")
}

/// Whether the branch is to be asked about: no miss and no failure within the max age.
fn lookup_due(ctx: &Ctx, policy: &Policy, branch: &str) -> Result<bool> {
    let lookups = ctx.store.load_lookups(&ctx.dir);
    let missed = lookups.misses.get(branch).map(String::as_str);
    Ok(!policy.is_recent(missed)?
        && !policy.backed_off(&ctx.store, &ctx.dir, &lookup_failure(branch))?)
}

/// Looks the branch's PR/MR up and syncs it; whether there was one. Finding only the PR/MR
/// `known` is nothing new, so it counts as a miss.
fn look_up(ctx: &Ctx, branch: &str, known: Option<u64>) -> Result<bool> {
    // No origin or no known forge is skipped before anything is locked or written.
    let located = sync::locate(ctx).inspect_err(|_| {
        auto::record_failure(&ctx.store, &ctx.dir, &lookup_failure(branch));
    })?;
    let Some((forge, host, repo)) = located else {
        return Ok(false);
    };
    locked(ctx, None, &lookup_failure(branch), |ctx| {
        let found = sync::find_for_branch(forge, &host, &repo, branch)?;
        if found
            .as_ref()
            .is_none_or(|found| Some(found.number) == known)
        {
            remember_miss(ctx, branch)?;
        }
        Ok(found)
    })
}

/// Runs `find` under the sync lock, and under the target's publish lock when its PR/MR is
/// known, then syncs the connection it returns; whether it returned one. A lock another
/// process holds ends the attempt. A failure after the locks is remembered under `failure`, a
/// success forgets it.
fn locked(
    ctx: &Ctx,
    key: Option<&str>,
    failure: &str,
    find: impl FnOnce(&Ctx) -> Result<Option<Connection>>,
) -> Result<bool> {
    let Some(_sync) = ctx.store.try_publish_lock(&ctx.dir, SYNC_LOCK)? else {
        return Err(Error("another agt is syncing".into()));
    };
    let _publish = match key {
        Some(key) => match ctx.store.try_publish_lock(&ctx.dir, key)? {
            Some(lock) => Some(lock),
            None => return Err(Error("a publish is running".into())),
        },
        None => None,
    };
    let result = find(ctx).and_then(|found| match found {
        Some(connection) => sync::sync_with(ctx, &connection).map(|_| true),
        None => Ok(false),
    });
    match &result {
        Ok(_) => auto::clear_failure(&ctx.store, &ctx.dir, failure),
        Err(_) => auto::record_failure(&ctx.store, &ctx.dir, failure),
    }
    result
}

fn remember_miss(ctx: &Ctx, branch: &str) -> Result<()> {
    let now = clock::now_secs()?;
    let mut lookups = ctx.store.load_lookups(&ctx.dir);
    lookups
        .misses
        .retain(|_, at| clock::parse(at).is_ok_and(|at| (0..auto::DAY).contains(&(now - at))));
    lookups.misses.insert(branch.to_owned(), clock::format(now));
    ctx.store.save_lookups(&ctx.dir, &lookups)
}
