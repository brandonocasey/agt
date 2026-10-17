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

/// What one pass over a repository directory did: the targets removed (or, on a dry run, that
/// would be) and the ones left alone, each with its reason.
#[derive(Default)]
pub(super) struct Pruned {
    pub pruned: Vec<Value>,
    pub skipped: Vec<Value>,
}

pub(super) fn prune_dir(store: &Store, dir: &Path, now: i64, dry_run: bool) -> Result<Pruned> {
    let mut done = Pruned::default();
    let repo = store.relative(dir);
    // Targets that cannot be read or hold unknown fields are left alone and reported as skipped.
    let (targets, left_out) = store.targets_intact_by_key(dir)?;
    for (key, why) in left_out {
        done.skipped
            .push(json!({ "repo": repo, "key": key, "why": why }));
    }
    for target in targets {
        if !expired(&target, now)? {
            continue;
        }
        // Skips a target whose publish is in flight or that another agt holds, as `agt cleanup`
        // does; the next run picks it up.
        let removed = dry_run
            || match store.try_remove_if(dir, &target.key, |current| expired(current, now))? {
                Removal::Removed(_) => true,
                Removal::Busy => {
                    done.skipped.push(json!({
                        "repo": repo,
                        "key": target.key,
                        "why": "locked by another agt process",
                    }));
                    false
                }
                Removal::Kept => false,
            };
        if removed {
            done.pruned.push(json!({
                "repo": repo,
                "target": target.key,
                "updated_at": target.updated_at,
                "closed_at": target.review.as_ref().and_then(|review| review.closed_at.as_ref()),
            }));
        }
    }
    Ok(done)
}

/// Prunes every repository. One that cannot be read or holds a corrupt file is reported and the
/// rest still go.
pub(super) fn prune_all(store: &Store, dry_run: bool) -> Result<Value> {
    let now = clock::now_secs()?;
    let (dirs, mut warnings) = store.repo_dirs_lenient();
    let mut all = Pruned::default();
    for dir in dirs {
        match prune_dir(store, &dir, now, dry_run) {
            Ok(done) => {
                all.pruned.extend(done.pruned);
                all.skipped.extend(done.skipped);
            }
            Err(error) => warnings.push(format!("{}: {error}; kept", dir.display())),
        }
    }
    Ok(json!({
        "dry_run": dry_run,
        "pruned": all.pruned,
        "skipped": all.skipped,
        "warnings": warnings,
    }))
}
