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

/// Removes the expired targets of one repository directory and returns them. Targets that cannot
/// be read, hold unknown fields, or are locked by another agt stay; `agt cleanup` reports them.
pub(super) fn prune_dir(store: &Store, dir: &Path, now: i64) -> Result<Vec<Value>> {
    let mut pruned = Vec::new();
    let repo = store.relative(dir);
    let (targets, _left_out) = store.targets_intact_by_key(dir)?;
    for target in targets {
        if !expired(&target, now)? {
            continue;
        }
        // Skips a target whose publish is in flight or that another agt holds; the next run
        // picks it up.
        if let Removal::Removed(_) =
            store.try_remove_if(dir, &target.key, |current| expired(current, now))?
        {
            pruned.push(json!({
                "repo": repo,
                "target": target.key,
                "updated_at": target.updated_at,
                "closed_at": target.review.as_ref().and_then(|review| review.closed_at.as_ref()),
            }));
        }
    }
    Ok(pruned)
}
