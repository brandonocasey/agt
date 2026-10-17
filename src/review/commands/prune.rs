use std::path::Path;

use serde_json::{Value, json};

use crate::core::clock;
use crate::core::error::Result;
use crate::store::Store;
use crate::store::model::{ReviewState, Target};
use crate::store::prune::{PRUNE_AFTER_SECS, Removal};

/// Age reference for pruning: when the PR/MR closed, else the last change.
pub(super) fn last_change(target: &Target) -> Result<i64> {
    let closed = target
        .review
        .as_ref()
        .filter(|review| review.state != ReviewState::Open)
        .and_then(|review| review.closed_at.as_deref());
    // A target touched after its PR/MR closed (a new run on a reused branch) ages from then.
    let updated = clock::parse(&target.updated_at)?;
    Ok(match closed {
        Some(closed) => updated.max(clock::parse(closed)?),
        None => updated,
    })
}

/// Whether the target is old enough to drop at `now`.
pub fn expired(target: &Target, now: i64) -> Result<bool> {
    Ok(now - last_change(target)? >= PRUNE_AFTER_SECS)
}

pub(super) fn prune_dir(store: &Store, dir: &Path, now: i64, dry_run: bool) -> Result<Vec<Value>> {
    let mut pruned = Vec::new();
    // Targets that cannot be read or hold unknown fields are left alone; `agt prune` reports them.
    let (targets, _) = store.targets_intact(dir)?;
    for target in targets {
        if !expired(&target, now)? {
            continue;
        }
        // Skips a target whose publish is in flight or that another agt holds, as `agt prune`
        // does; the next run picks it up.
        let removed = dry_run
            || matches!(
                store.try_remove_if(dir, &target.key, |current| expired(current, now))?,
                Removal::Removed(_)
            );
        if removed {
            pruned.push(json!({
                "repo": store.relative(dir),
                "target": target.key,
                "updated_at": target.updated_at,
                "closed_at": target.review.as_ref().and_then(|review| review.closed_at.as_ref()),
            }));
        }
    }
    Ok(pruned)
}

pub(super) fn prune_all(store: &Store, dry_run: bool) -> Result<Value> {
    let now = clock::now_secs()?;
    let mut pruned = Vec::new();
    for dir in store.repo_dirs()? {
        pruned.extend(prune_dir(store, &dir, now, dry_run)?);
    }
    Ok(json!({ "dry_run": dry_run, "pruned": pruned }))
}
