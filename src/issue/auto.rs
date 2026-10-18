//! Auto-sync for the reads: before `list`, `show`, `search`, `status` and `find`, sync each
//! configured tracker in scope whose last sync is older than the max age. Nothing configured
//! means nothing to do. A tracker that cannot be synced is reported on stderr and its local
//! data is read as it is.

use super::sync::sync_tracker;
use super::trackers;
use crate::core::config::Tracker;
use crate::core::error::{Error, Result};
use crate::forge::auto::{self, Policy};
use crate::store::Store;
use crate::store::issues::{TRACKER_FILE, TrackerDoc};

/// Held while a tracker syncs, so two reads do not fetch the same tracker twice.
const SYNC_LOCK: &str = "sync";
/// Held by `push` for the whole tracker; a sync does not run under it.
const PUSH_LOCK: &str = "push";
/// The failure of a tracker's sync is remembered under this key in the tracker's directory.
pub(super) const FAILURE_KEY: &str = "sync";

pub fn sync_stale(store: &Store, filter: Option<&str>, offline: bool) -> Result<()> {
    let Some(policy) = Policy::resolve(offline) else {
        return Ok(());
    };
    // A filter that matches nothing is the read's own error to report.
    let Ok(configured) = trackers::configured(&policy.config, filter) else {
        return Ok(());
    };
    let mut due = Vec::new();
    for tracker in configured {
        let key = trackers::tracker_key(
            trackers::forge_of(tracker.forge),
            &tracker.host,
            &tracker.repo,
        );
        let dir = store.repo_dir(&key)?;
        let doc = store.load_doc::<TrackerDoc>(&dir, TRACKER_FILE)?;
        let synced_at = doc.as_ref().and_then(|doc| doc.synced_at.as_deref());
        if policy.is_stale(synced_at)? && !policy.backed_off(store, &dir, FAILURE_KEY)? {
            due.push((tracker, dir));
        }
    }
    if due.is_empty() {
        return Ok(());
    }
    // One deadline for all trackers, and one line for the first that fails.
    auto::attempt(|| {
        let mut first_failure = None;
        for (tracker, dir) in due {
            let result = sync_one(store, tracker, &dir);
            if let Err(Stop::Failed(_)) = &result {
                auto::record_failure(store, &dir, FAILURE_KEY);
            }
            if let Err(Stop::Busy(error) | Stop::Failed(error)) = result
                && first_failure.is_none()
            {
                first_failure = Some(error);
            }
        }
        first_failure.map_or(Ok(()), Err)
    });
    Ok(())
}

/// Why a tracker was not synced: another agt holds it (not a failure of the host), or the sync
/// failed, which keeps the next reads from trying for a while.
enum Stop {
    Busy(Error),
    Failed(Error),
}

fn sync_one(store: &Store, configured: &Tracker, dir: &std::path::Path) -> Result<(), Stop> {
    let busy = |why: &str| Stop::Busy(Error(why.into()));
    let Some(_sync) = store
        .try_publish_lock(dir, SYNC_LOCK)
        .map_err(Stop::Failed)?
    else {
        return Err(busy("another agt is syncing"));
    };
    let Some(_push) = store
        .try_publish_lock(dir, PUSH_LOCK)
        .map_err(Stop::Failed)?
    else {
        return Err(busy("a push is running"));
    };
    let tracker = trackers::ensure(store, configured).map_err(Stop::Failed)?;
    sync_tracker(store, tracker).map(drop).map_err(Stop::Failed)
}
