//! Deleting a plan, item by item, each under its own lock.

use serde_json::{Map, Value, json};

use super::plan::{Action, Item, Plan};
use crate::core::error::Result;
use crate::issue::prune::prunable;
use crate::review::commands::prune::expired;
use crate::store::prune::Removal;
use crate::store::{Namespace, Store};

pub struct Outcome {
    deleted: Vec<(&'static str, String, std::path::PathBuf, u64)>,
    skipped: Vec<(&'static str, String, std::path::PathBuf, String)>,
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
        map.insert("bytes_freed".into(), json!(freed));
        map
    }
}

/// Deletes every item whose lock is free and which still qualifies; the rest are reported as
/// skipped with the reason. An error on one item skips it and does not stop the others.
pub fn run(plan: &Plan, now: i64) -> Outcome {
    let mut outcome = Outcome {
        deleted: Vec::new(),
        skipped: Vec::new(),
    };
    for item in &plan.items {
        let result = delete(item, now);
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
    outcome
}

fn delete(item: &Item, now: i64) -> Result<Removal> {
    match &item.action {
        Action::Review { dir, key } => {
            Store::open(Namespace::Reviews)?.try_remove_if(dir, key, |target| expired(target, now))
        }
        Action::Issue { dir, name } => Store::open(Namespace::Issues)?
            .try_remove_doc_if::<crate::store::issues::Issue>(dir, name, |issue| {
                prunable(issue, now)
            }),
        Action::Task(task) => crate::task::prune::remove(task, now),
        Action::Lock { namespace, path } => Store::open(*namespace)?.remove_stray_lock(path),
        Action::Backup(backup) => Store::open(Namespace::Reviews)?.remove_backup(backup),
        Action::LocalRepo { dir, had_targets } => {
            Store::open(Namespace::Reviews)?.remove_local_repo(dir, now, *had_targets)
        }
    }
}
