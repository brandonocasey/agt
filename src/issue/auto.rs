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

pub fn sync_stale(
    store: &Store,
    repo: &crate::git::Repo,
    filter: Option<&str>,
    offline: bool,
) -> Result<()> {
    let Some(mut policy) = Policy::resolve_for_repo(repo, offline) else {
        return Ok(());
    };
    if let Err(error) = trackers::destinations(&mut policy.config, repo, filter, true) {
        auto::skipped(&error.0);
        return Ok(());
    }
    // A filter that matches nothing is the read's own error to report.
    let Ok(configured) = trackers::configured(&policy.config, filter) else {
        return Ok(());
    };
    let mut due = Vec::new();
    for tracker in configured {
        let key = trackers::tracker_key(tracker.forge, &tracker.host, &tracker.repo);
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

fn destination<'a>(
    config: &'a crate::core::config::Config,
    filter: Option<&str>,
) -> Result<&'a Tracker> {
    let targets = trackers::configured(config, filter)?;
    let [target] = targets.as_slice() else {
        return crate::core::error::fail("several issue destinations; pass --tracker");
    };
    if target.push == Some(false) {
        return crate::core::error::fail("tracker has push = false");
    }
    Ok(*target)
}

fn attach(
    store: &Store,
    configured: &Tracker,
    source: &trackers::Tracker,
    issue: &crate::store::issues::Issue,
) -> Result<trackers::Tracker> {
    let target = trackers::ensure(store, configured)?;
    let name = trackers::doc_name(issue);
    store.update_existing_doc(
        &source.dir,
        &name,
        |issue: &mut crate::store::issues::Issue| {
            issue.tracker.clone_from(&target.doc.key);
            Ok(())
        },
    )?;
    store.attach_issue(&source.dir, &target.dir, &name)?;
    Ok(target)
}

pub fn attach_pending(
    store: &Store,
    config: &crate::core::config::Config,
    filter: Option<&str>,
) -> Result<()> {
    let locals = trackers::stored(store, Some("local")).unwrap_or_default();
    let mut target = None;
    for source in locals {
        for issue in store.docs::<crate::store::issues::Issue>(&source.dir, &[TRACKER_FILE])? {
            let target = match target {
                Some(target) => target,
                None => *target.insert(destination(config, filter)?),
            };
            attach(store, target, &source, &issue)?;
        }
    }
    Ok(())
}

pub fn push_changed(
    store: &Store,
    repo: &crate::git::Repo,
    filter: Option<&str>,
    offline: bool,
    value: &mut serde_json::Value,
) {
    if auto::is_offline(offline) {
        return;
    }
    let config = match crate::core::config::Config::load_for_repo(repo) {
        Ok(config) => config,
        Err(error) => {
            auto::skipped(&error.0);
            return;
        }
    };
    if !config.sync.auto_push {
        return;
    }
    let Some(key) = value
        .get("key")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    let mut destination_dir = None;
    let result = auto::attempt(|| {
        let (source, issue) = trackers::find(store, None, &key)?;
        let target = if source.doc.forge.is_none() {
            let mut config = config;
            trackers::destinations(&mut config, repo, filter, true)?;
            attach(store, destination(&config, filter)?, &source, &issue)?
        } else {
            let target = config.issues.tracker.iter().find(|target| {
                trackers::tracker_key(target.forge, &target.host, &target.repo) == source.doc.key
            });
            if target.is_some_and(|target| target.push == Some(false)) {
                return crate::core::error::fail("tracker has push = false");
            }
            source
        };
        destination_dir = Some(target.dir.clone());
        let name = trackers::doc_name(&issue);
        let report = super::push::one(store, &target, &name)?;
        if report["failed"]
            .as_array()
            .is_some_and(|items| !items.is_empty())
        {
            return crate::core::error::fail("issue upload failed; changes remain queued");
        }
        let canonical = report["sent"]
            .as_array()
            .and_then(|sent| {
                sent.iter()
                    .find_map(|item| item.get("number").and_then(serde_json::Value::as_u64))
            })
            .map(|number| number.to_string())
            .unwrap_or(name);
        store
            .load_doc::<crate::store::issues::Issue>(&target.dir, &canonical)?
            .map(|issue| super::view::summary(&issue))
            .ok_or_else(|| Error("changed issue vanished".into()))
    });
    if let Some(updated) = result {
        *value = updated;
    } else if let Some(dir) = destination_dir {
        let saved = store.docs::<crate::store::issues::Issue>(&dir, &[TRACKER_FILE]);
        if let Ok(saved) = saved
            && let Some(issue) = saved.iter().find(|issue| {
                issue.key == key
                    || issue
                        .create
                        .as_ref()
                        .and_then(|create| create.sent.as_deref())
                        .and_then(crate::forge::markers::agent_id)
                        .as_deref()
                        == Some(key.as_str())
            })
        {
            *value = super::view::summary(issue);
        }
    }
}
