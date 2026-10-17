//! What a prune would delete, found without changing anything.

use std::fs;
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::core::error::Result;
use crate::issue::prune::prunable;
use crate::review::commands::prune::expired;
use crate::store::issues::{Issue, TRACKER_FILE};
use crate::store::model::Target;
use crate::store::prune::{Backup, PRUNE_AFTER_SECS};
use crate::store::{Namespace, Store};
use crate::task::prune::{Expired, expired as expired_tasks};

pub const CATEGORIES: [&str; 7] = [
    "reviews",
    "local_repos",
    "issues",
    "tasks",
    "locks",
    "partial_backups",
    "backups",
];

/// How to delete one planned item.
pub enum Action {
    Review { dir: PathBuf, key: String },
    LocalRepo { dir: PathBuf },
    Issue { dir: PathBuf, name: String },
    Task(Expired),
    Lock { namespace: Namespace, path: PathBuf },
    Backup(Backup),
}

pub struct Item {
    pub category: &'static str,
    pub path: PathBuf,
    pub key: String,
    pub reason: String,
    pub bytes: u64,
    pub action: Action,
}

#[derive(Default)]
pub struct Plan {
    pub items: Vec<Item>,
    /// Places that could not be read, so their contents are missing from the plan.
    pub warnings: Vec<String>,
}

fn size(path: &std::path::Path) -> u64 {
    fs::metadata(path).map_or(0, |meta| meta.len())
}

impl Plan {
    fn add(
        &mut self,
        category: &'static str,
        path: PathBuf,
        key: String,
        reason: String,
        bytes: u64,
        action: Action,
    ) {
        self.items.push(Item {
            category,
            path,
            key,
            reason,
            bytes,
            action,
        });
    }

    fn attempt(&mut self, place: &std::path::Path, result: Result<()>) {
        if let Err(error) = result {
            self.warnings.push(format!("{}: {error}", place.display()));
        }
    }

    /// Reviews past the review rule, in every repository, and `local/` repositories that hold
    /// nothing newer.
    pub fn reviews(&mut self, now: i64) -> Result<()> {
        let store = Store::open(Namespace::Reviews)?;
        for dir in store.repo_dirs()? {
            let result = self.review_dir(&store, &dir, now);
            self.attempt(&dir, result);
        }
        let local = store.root().join("local");
        if let Ok(entries) = fs::read_dir(&local) {
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.path())
                .collect();
            dirs.sort();
            for dir in dirs {
                let result = self.local_repo(&store, dir.clone(), now);
                self.attempt(&dir, result);
            }
        }
        Ok(())
    }

    fn review_dir(&mut self, store: &Store, dir: &std::path::Path, now: i64) -> Result<()> {
        for target in store.targets(dir)? {
            if !expired(&target, now)? {
                continue;
            }
            let path = dir.join(format!("{}.json", target.key));
            self.add(
                "reviews",
                path.clone(),
                format!("{}/{}", store.relative(dir), target.key),
                review_reason(&target),
                size(&path),
                Action::Review {
                    dir: dir.to_path_buf(),
                    key: target.key.clone(),
                },
            );
        }
        Ok(())
    }

    /// `local/<name>-<hash>` is named after a hash of the git common dir, and nothing in the
    /// store records the path, so whether it still exists cannot be checked. A repository is
    /// listed only when every target in it has expired, which is all that can be said.
    fn local_repo(&mut self, store: &Store, dir: PathBuf, now: i64) -> Result<()> {
        let targets = store.targets(&dir)?;
        let mut leftovers = 0;
        for entry in fs::read_dir(&dir)
            .map_err(crate::core::error::ctx("read repo dir"))?
            .flatten()
        {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                return Ok(());
            }
            leftovers += size(&entry.path());
        }
        for target in &targets {
            if !expired(target, now)? {
                return Ok(());
            }
        }
        let name = store.relative(&dir);
        self.add(
            "local_repos",
            dir.clone(),
            name,
            format!(
                "local repository: its git common dir cannot be checked because the store keeps only a hash of it; {} target(s), all older than {} days",
                targets.len(),
                PRUNE_AFTER_SECS / 86_400
            ),
            leftovers,
            Action::LocalRepo {
                dir,
            },
        );
        Ok(())
    }

    /// Closed issues with nothing local to lose, past the issue rule.
    pub fn issues(&mut self, now: i64) -> Result<()> {
        let store = Store::open(Namespace::Issues)?;
        for dir in store.child_dirs()? {
            let result = (|| {
                for issue in store.docs::<Issue>(&dir, &[TRACKER_FILE])? {
                    if !prunable(&issue, now) {
                        continue;
                    }
                    let name = issue
                        .number
                        .map_or_else(|| issue.key.clone(), |n| n.to_string());
                    let path = dir.join(format!("{name}.json"));
                    self.add(
                        "issues",
                        path.clone(),
                        issue.key.clone(),
                        format!(
                            "closed, last updated {}; nothing queued, no notes, conflicts or unsent comments",
                            issue.updated_at
                        ),
                        size(&path),
                        Action::Issue {
                            dir: dir.clone(),
                            name,
                        },
                    );
                }
                Ok(())
            })();
            self.attempt(&dir, result);
        }
        Ok(())
    }

    /// Complete tasks last updated 90 days ago. The session index is a cache and is left alone.
    pub fn tasks(&mut self, now: i64) -> Result<()> {
        for task in expired_tasks(now)? {
            let reason = format!("status complete, last updated {}", task.updated_at);
            self.add(
                "tasks",
                task.path.clone(),
                task.id.clone(),
                reason,
                task.bytes,
                Action::Task(task),
            );
        }
        Ok(())
    }

    /// Lock files of reviews and issues whose document is gone. Task locks are directories that
    /// exist only while held, so they are not looked at.
    pub fn locks(&mut self) -> Result<()> {
        for namespace in [Namespace::Reviews, Namespace::Issues] {
            let store = Store::open(namespace)?;
            for path in store.stray_locks()? {
                self.add(
                    "locks",
                    path.clone(),
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    "lock file whose document no longer exists".into(),
                    size(&path),
                    Action::Lock { namespace, path },
                );
            }
        }
        Ok(())
    }

    /// agt's own backups: unfinished copies, and finished ones older than 90 days except the
    /// newest of each namespace. Other tools' backups in the same directory are never listed.
    pub fn backups(&mut self, now: i64) -> Result<()> {
        let store = Store::open(Namespace::Reviews)?;
        let all = store.agt_backups()?;
        for namespace in Namespace::ALL {
            let newest = all
                .iter()
                .filter(|backup| backup.namespace == namespace && !backup.partial)
                .max_by_key(|backup| (backup.taken, backup.path.clone()))
                .map(|backup| backup.path.clone());
            for backup in all.iter().filter(|backup| backup.namespace == namespace) {
                let (category, reason) = if backup.partial {
                    ("partial_backups", "unfinished backup copy".to_owned())
                } else if Some(&backup.path) == newest.as_ref()
                    || now - backup.taken.0 < PRUNE_AFTER_SECS
                {
                    continue;
                } else {
                    (
                        "backups",
                        format!(
                            "pre-migration backup from {}, not the newest of {}",
                            crate::core::clock::format(backup.taken.0),
                            namespace.dir_name()
                        ),
                    )
                };
                self.add(
                    category,
                    backup.path.clone(),
                    backup
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    reason,
                    backup.bytes,
                    Action::Backup(backup.clone()),
                );
            }
        }
        Ok(())
    }

    pub fn json(&self) -> Value {
        let mut map = Map::new();
        for category in CATEGORIES {
            let items: Vec<Value> = self
                .items
                .iter()
                .filter(|item| item.category == category)
                .map(|item| {
                    json!({
                        "path": item.path,
                        "key": item.key,
                        "reason": item.reason,
                        "bytes": item.bytes,
                    })
                })
                .collect();
            map.insert(category.to_owned(), Value::Array(items));
        }
        Value::Object(map)
    }

    pub fn totals(&self) -> Value {
        let mut by_category = Map::new();
        for category in CATEGORIES {
            let chosen = self.items.iter().filter(|item| item.category == category);
            let (count, bytes) = chosen.fold((0, 0), |(count, bytes), item| {
                (count + 1, bytes + item.bytes)
            });
            by_category.insert(
                category.to_owned(),
                json!({ "items": count, "bytes": bytes }),
            );
        }
        json!({
            "items": self.items.len(),
            "bytes": self.items.iter().map(|item| item.bytes).sum::<u64>(),
            "by_category": by_category,
        })
    }
}

fn review_reason(target: &Target) -> String {
    match target.review.as_ref().and_then(|review| {
        review
            .closed_at
            .as_deref()
            .filter(|_| review.state != crate::store::model::ReviewState::Open)
    }) {
        Some(closed) => format!("PR/MR closed {closed}, last updated {}", target.updated_at),
        None => format!("last changed {}", target.updated_at),
    }
}
