use std::path::Path;

use serde_json::{Value, json};

use crate::core::clock;
use crate::core::error::Result;
use crate::store::Store;
use crate::store::model::{ReviewState, Target};

use super::PRUNE_AFTER_SECS;

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

pub(super) fn prune_dir(store: &Store, dir: &Path, now: i64, dry_run: bool) -> Result<Vec<Value>> {
    let mut pruned = Vec::new();
    for target in store.targets(dir)? {
        if now - last_change(&target)? < PRUNE_AFTER_SECS {
            continue;
        }
        let removed = dry_run
            || store.remove_if(dir, &target.key, |current| {
                Ok(now - last_change(current)? >= PRUNE_AFTER_SECS)
            })?;
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
