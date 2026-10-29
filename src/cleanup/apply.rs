//! Deleting a plan, item by item, each under its own lock.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::files::OUTPUT_DAYS;
use super::plan::{Action, Item, Plan};
use crate::core::error::Result;
use crate::issue::prune::{parents, prunable};
use crate::review::commands::prune::expired;
use crate::store::prune::Removal;
use crate::store::{Namespace, Store};

pub struct Outcome {
    deleted: Vec<(&'static str, String, std::path::PathBuf, u64)>,
    skipped: Vec<(&'static str, String, std::path::PathBuf, String)>,
    /// Process ids and the last signal each was sent.
    signalled: Vec<(String, &'static str)>,
}

impl Outcome {
    pub fn json(&self) -> Map<String, Value> {
        let freed: u64 = self.deleted.iter().map(|(_, _, _, bytes)| bytes).sum();
        let mut map = Map::new();
        map.insert(
            "deleted".into(),
            self.deleted
                .iter()
                .map(|(category, key, path, bytes)| {
                    json!({ "category": category, "key": key, "path": path, "bytes": bytes })
                })
                .collect(),
        );
        map.insert(
            "skipped".into(),
            self.skipped
                .iter()
                .map(|(category, key, path, why)| {
                    json!({ "category": category, "key": key, "path": path, "why": why })
                })
                .collect(),
        );
        map.insert(
            "signalled".into(),
            self.signalled
                .iter()
                .map(|(pid, signal)| json!({ "pid": pid, "signal": signal }))
                .collect(),
        );
        map.insert("bytes_freed".into(), json!(freed));
        map
    }
}

/// Deletes every item whose lock is free and which still qualifies; the rest are reported as
/// skipped with the reason. An error on one item skips it and does not stop the others.
/// Processes are signalled only when `signal` is set, all together, after the files.
pub fn run(plan: &Plan, now: i64, signal: bool) -> Outcome {
    let mut outcome = Outcome {
        deleted: Vec::new(),
        skipped: Vec::new(),
        signalled: Vec::new(),
    };
    let mut stopping = Vec::new();
    let mut issue_guards = HashMap::new();
    for item in &plan.items {
        if let Action::Process(proc) = &item.action {
            if signal {
                stopping.push(proc.clone());
            } else {
                outcome.skipped.push((
                    item.category,
                    item.key.clone(),
                    item.path.clone(),
                    "processes are signalled only with --only processes".into(),
                ));
            }
            continue;
        }
        let result = delete(item, now, &mut issue_guards);
        let item_id = (item.category, item.key.clone(), item.path.clone());
        match result {
            Ok(Removal::Removed(bytes)) => outcome
                .deleted
                .push((item_id.0, item_id.1, item_id.2, bytes)),
            Ok(Removal::Kept) => outcome.skipped.push((
                item_id.0,
                item_id.1,
                item_id.2,
                "no longer qualifies or already gone".into(),
            )),
            Ok(Removal::Busy) => outcome.skipped.push((
                item_id.0,
                item_id.1,
                item_id.2,
                "locked by another agt process".into(),
            )),
            Err(error) => {
                outcome
                    .skipped
                    .push((item_id.0, item_id.1, item_id.2, format!("error: {error}")))
            }
        }
    }
    for (proc, result) in super::procs::stop(&stopping) {
        let key = proc.pid.to_string();
        match result {
            Ok(signal) => outcome.signalled.push((key, signal)),
            Err(why) => outcome
                .skipped
                .push(("processes", key, std::path::PathBuf::new(), why)),
        }
    }
    outcome
}

struct IssueGuard {
    _lock: File,
    parents: HashSet<String>,
}

type IssueGuards = HashMap<PathBuf, std::result::Result<Option<IssueGuard>, String>>;

fn issue_guard(root: &Path) -> Result<Option<IssueGuard>> {
    let store = Store::at_root(Namespace::Issues, root.to_path_buf())?;
    let Some(lock) = store.try_publish_lock(store.root(), "hierarchy")? else {
        return Ok(None);
    };
    let mut issues = Vec::new();
    for dir in store.child_dirs()? {
        issues.extend(
            store
                .docs_intact::<crate::store::issues::Issue>(
                    &dir,
                    &[crate::store::issues::TRACKER_FILE],
                )?
                .0,
        );
    }
    Ok(Some(IssueGuard {
        _lock: lock,
        parents: parents(&issues),
    }))
}

fn delete(item: &Item, now: i64, issue_guards: &mut IssueGuards) -> Result<Removal> {
    match &item.action {
        Action::Review { root, dir, key } => Store::at_root(Namespace::Reviews, root.clone())?
            .try_remove_if(dir, key, |target| expired(target, now)),
        Action::Issue { root, dir, name } => {
            let guard = issue_guards
                .entry(root.clone())
                .or_insert_with(|| issue_guard(root).map_err(|error| error.to_string()));
            let parents = match guard {
                Ok(Some(guard)) => &guard.parents,
                Ok(None) => return Ok(Removal::Busy),
                Err(error) => return crate::core::error::fail(error.clone()),
            };
            Store::at_root(Namespace::Issues, root.clone())?
                .try_remove_doc_if::<crate::store::issues::Issue>(dir, name, |issue| {
                    !parents.contains(&issue.key) && prunable(issue, now)
                })
        }
        Action::Check { root, name } => {
            Store::at_root(Namespace::Checks, root.clone())?
                .try_remove_doc_if::<crate::check::prune::Entry>(root, name, |entry| {
                    entry.expired(now, OUTPUT_DAYS)
                })
        }
        Action::UnreadableCheck { root, name } => {
            Store::at_root(Namespace::Checks, root.clone())?
                .remove_unreadable_doc::<crate::check::prune::Entry>(root, name)
        }
        Action::Task(task) => crate::task::prune::remove(task, now),
        Action::TaskLock(path) => crate::task::prune::remove_orphan_lock(path),
        Action::Lock {
            root,
            namespace,
            path,
        } => Store::at_root(*namespace, root.clone())?.remove_stray_lock(path),
        Action::Backup(backup) => Store::open(Namespace::Reviews)?.remove_backup(backup),
        Action::LocalRepo {
            root,
            dir,
            had_targets,
        } => Store::at_root(Namespace::Reviews, root.clone())?.remove_local_repo(
            dir,
            now,
            *had_targets,
        ),
        Action::Files {
            root,
            days,
            harness,
        } => super::files::remove(root, &item.path, *days, now, *harness),
        Action::BrowserProfile => super::files::remove_browser_profile(&item.path, now),
        // `run` signals processes on its own.
        Action::Process(_) => Ok(Removal::Kept),
    }
}
