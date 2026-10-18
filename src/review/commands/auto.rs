//! Auto-sync for the reads: before `list`, `show` and `status`, sync the target's PR/MR when
//! its last sync is older than the max age. A target with no known PR/MR is looked up on the
//! forge at most once per max age, and a miss is remembered in the repository's lookup file.
//! No origin, or a host that is no known forge, skips quietly; any other failure is one line
//! on stderr and the read goes on with the local data.

use super::sync::{self, Connection};
use super::{Ctx, Scope};
use crate::core::clock;
use crate::core::error::{Error, Result};
use crate::forge::auto::{self, Policy};
use crate::store::model::TargetKind;

/// Held while the repository syncs or looks a PR/MR up, so two reads do not repeat each other.
const SYNC_LOCK: &str = "auto-sync";

pub(super) fn sync_stale(ctx: &Ctx, offline: bool) -> Result<()> {
    let Some(policy) = Policy::resolve(offline)? else {
        return Ok(());
    };
    // A scope that cannot be worked out is the read's own error to report.
    let Ok(scope) = ctx.scope() else {
        return Ok(());
    };
    let known = scope
        .existing
        .as_ref()
        .and_then(|target| target.review.as_ref());
    match known {
        Some(review) => {
            if !policy.is_stale(Some(&review.synced_at))? {
                return Ok(());
            }
            auto::attempt(|| {
                let connection = sync::connection_of(review)?;
                let key = connection.key.clone();
                locked(ctx, Some(&key), |_| Ok(Some(connection)))
            });
        }
        None => {
            let Some(branch) = branch_to_look_up(&scope) else {
                return Ok(());
            };
            let missed = ctx
                .store
                .load_lookups(&ctx.dir)
                .misses
                .get(&branch)
                .cloned();
            if policy.is_recent(missed.as_deref())? {
                return Ok(());
            }
            auto::attempt(|| look_up(ctx, &branch));
        }
    }
    Ok(())
}

/// Only a branch target has a PR/MR to find by its branch.
fn branch_to_look_up(scope: &Scope) -> Option<String> {
    if scope.kind != TargetKind::Branch {
        return None;
    }
    scope.branch.clone().filter(|branch| !branch.is_empty())
}

fn look_up(ctx: &Ctx, branch: &str) -> Result<()> {
    let Some((forge, host, repo)) = sync::locate(ctx)? else {
        return Ok(());
    };
    locked(ctx, None, |ctx| {
        let found = sync::find_for_branch(forge, &host, &repo, branch)?;
        if found.is_none() {
            remember_miss(ctx, branch)?;
        }
        Ok(found)
    })
}

/// Runs `find` under the sync lock, and under the target's publish lock when its PR/MR is
/// known, then syncs the connection it returns. A lock another process holds ends the attempt.
fn locked(
    ctx: &Ctx,
    key: Option<&str>,
    find: impl FnOnce(&Ctx) -> Result<Option<Connection>>,
) -> Result<()> {
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
    if let Some(connection) = find(ctx)? {
        sync::sync_with(ctx, &connection)?;
    }
    Ok(())
}

fn remember_miss(ctx: &Ctx, branch: &str) -> Result<()> {
    let now = clock::now_secs()?;
    let mut lookups = ctx.store.load_lookups(&ctx.dir);
    // Entries older than a day are no use to any max age worth setting.
    lookups
        .misses
        .retain(|_, at| clock::parse(at).is_ok_and(|at| (0..24 * 60 * 60).contains(&(now - at))));
    lookups.misses.insert(branch.to_owned(), clock::format(now));
    ctx.store.save_lookups(&ctx.dir, &lookups)
}
