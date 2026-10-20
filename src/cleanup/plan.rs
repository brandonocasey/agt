//! What a cleanup would delete, found without changing anything.

use std::fs;
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use super::files::{self, OUTPUT_DAYS, SCRATCH_DAYS};
use super::procs;
use crate::core::agents::Proc;
use crate::core::clock;
use crate::core::error::Result;
use crate::issue::prune::prunable;
use crate::review::commands::prune::expired;
use crate::store::issues::{Issue, TRACKER_FILE};
use crate::store::model::Target;
use crate::store::prune::PRUNE_AFTER_SECS;
use crate::store::prune::backups::Backup;
use crate::store::{Namespace, Store};
use crate::task::prune::{Expired, expired as expired_tasks};

pub const CATEGORIES: [&str; 10] = [
    "reviews",
    "local_repos",
    "issues",
    "tasks",
    "locks",
    "partial_backups",
    "backups",
    "output",
    "scratch",
    "processes",
];

/// How to delete one planned item.
pub enum Action {
    Review {
        root: PathBuf,
        dir: PathBuf,
        key: String,
    },
    /// `had_targets`: the plan found targets, all expired, so none left at delete time is normal.
    LocalRepo {
        root: PathBuf,
        dir: PathBuf,
        had_targets: bool,
    },
    Issue {
        root: PathBuf,
        dir: PathBuf,
        name: String,
    },
    Task(Expired),
    Lock {
        root: PathBuf,
        namespace: Namespace,
        path: PathBuf,
    },
    Backup(Backup),
    /// A tree under the cache root, deleted while it is still `days` days old.
    Files {
        root: PathBuf,
        days: i64,
        /// A harness session directory, whose emptied parent goes too.
        harness: bool,
    },
    Process(Proc),
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

/// A file's own size; a link counts as the link, never what it points at.
fn size(path: &std::path::Path) -> u64 {
    fs::symlink_metadata(path).map_or(0, |meta| meta.len())
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

    /// The value, or `None` after recording a warning, so one unreadable place does not stop
    /// the rest of the pass.
    fn attempt<T>(&mut self, place: &std::path::Path, result: Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.warnings.push(format!("{}: {error}", place.display()));
                None
            }
        }
    }

    /// Reviews past the review rule, in every repository, and `local/` repositories that hold
    /// nothing newer.
    pub fn reviews(&mut self, now: i64) -> Result<()> {
        for store in stores(Namespace::Reviews)? {
            let (dirs, warnings) = store.repo_dirs_lenient();
            self.warnings.extend(warnings);
            for dir in dirs {
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
        }
        Ok(())
    }

    fn review_dir(&mut self, store: &Store, dir: &std::path::Path, now: i64) -> Result<()> {
        let (targets, warnings) = store.targets_intact(dir)?;
        self.warnings.extend(warnings);
        for target in targets {
            if !expired(&target, now)? {
                continue;
            }
            let path = dir.join(format!("{}.json", target.key));
            let repository = store.relative(dir);
            let key = if repository.is_empty() {
                target.key.clone()
            } else {
                format!("{repository}/{}", target.key)
            };
            self.add(
                "reviews",
                path.clone(),
                key,
                review_reason(&target),
                size(&path),
                Action::Review {
                    root: store.root().to_path_buf(),
                    dir: dir.to_path_buf(),
                    key: target.key.clone(),
                },
            );
        }
        Ok(())
    }

    /// `local/<name>-<hash>` is named after a hash of the git common dir. A `.repo.json`
    /// that records the path lists the repository whatever its age once the path is missing,
    /// unseen for 30 days and not below a mount root (see `Store::presence`). Otherwise it is
    /// listed only when every target in it is already planned as expired. Only target files, cache files and
    /// stray locks may be in it; anything else keeps it. The same check runs again when
    /// deleting, so the plan lists nothing the deletion would skip.
    fn local_repo(&mut self, store: &Store, dir: PathBuf, now: i64) -> Result<()> {
        let Some(repo) = store.scan_local_repo(&dir, now)? else {
            return Ok(());
        };
        // With no targets at all there is nothing expired to wait for; the directory goes only
        // when agt has not written in it for as long as a target is kept.
        if repo.targets.is_empty() && !repo.presence.is_gone() && repo.idle_secs < PRUNE_AFTER_SECS
        {
            return Ok(());
        }
        let mut bytes = 0;
        for target in &repo.targets {
            let path = dir.join(format!("{}.json", target.key));
            // Expired targets are items of their own, with their own bytes.
            let planned = self
                .items
                .iter()
                .any(|item| item.category == "reviews" && item.path == path);
            if planned {
                continue;
            }
            if !repo.presence.is_gone() {
                return Ok(());
            }
            bytes += size(&path);
        }
        bytes += repo
            .extras
            .iter()
            .chain(&repo.stray_locks)
            .map(|path| size(path))
            .sum::<u64>();
        let reason = if repo.presence.is_gone() {
            repo.presence.describe()
        } else if repo.targets.is_empty() {
            format!(
                "no targets left; only cache files, last written {} day(s) ago (over {} days); {}",
                repo.idle_secs / clock::SECS_PER_DAY,
                PRUNE_AFTER_SECS / clock::SECS_PER_DAY,
                repo.presence.describe()
            )
        } else {
            format!(
                "{} target(s), all older than {} days (every target expired); {}",
                repo.targets.len(),
                PRUNE_AFTER_SECS / clock::SECS_PER_DAY,
                repo.presence.describe()
            )
        };
        let name = store.relative(&dir);
        self.add(
            "local_repos",
            dir.clone(),
            name,
            reason,
            bytes,
            Action::LocalRepo {
                root: store.root().to_path_buf(),
                dir,
                had_targets: !repo.targets.is_empty(),
            },
        );
        Ok(())
    }

    /// Closed issues with nothing local to lose, past the issue rule.
    pub fn issues(&mut self, now: i64) -> Result<()> {
        for store in stores(Namespace::Issues)? {
            let found = store.child_dirs();
            for dir in self.attempt(store.root(), found).unwrap_or_default() {
                let result = (|| {
                    let (issues, unknown) = store.docs_intact::<Issue>(&dir, &[TRACKER_FILE])?;
                    self.warnings.extend(unknown);
                    for issue in issues {
                        if !prunable(&issue, now) {
                            continue;
                        }
                        let name = issue.doc_name();
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
                            root: store.root().to_path_buf(),
                            dir: dir.clone(),
                            name,
                        },
                    );
                    }
                    Ok(())
                })();
                self.attempt(&dir, result);
            }
        }
        Ok(())
    }

    /// Complete tasks last updated 90 days ago. The session index is a cache and is left alone.
    pub fn tasks(&mut self, now: i64) -> Result<()> {
        let found = expired_tasks(now);
        let root = PathBuf::from("tasks");
        for task in self.attempt(&root, found).unwrap_or_default() {
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

    /// Lock files of reviews and issues whose document is gone. A task's lock file goes with its
    /// record, so task locks are not looked at.
    pub fn locks(&mut self) -> Result<()> {
        for namespace in [Namespace::Reviews, Namespace::Issues] {
            for store in stores(namespace)? {
                let found = store.stray_locks();
                for path in self.attempt(store.root(), found).unwrap_or_default() {
                    // A local repository that is listed deletes its own stray locks.
                    let inside = self.items.iter().any(|item| {
                    matches!(&item.action, Action::LocalRepo { dir, .. } if path.parent() == Some(dir))
                });
                    if inside {
                        continue;
                    }
                    self.add(
                        "locks",
                        path.clone(),
                        path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        "lock file whose document no longer exists".into(),
                        size(&path),
                        Action::Lock {
                            root: store.root().to_path_buf(),
                            namespace,
                            path,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// agt's own backups: unfinished copies, and finished ones older than 90 days except the
    /// newest of each namespace. Other tools' backups in the same directory are never listed.
    pub fn backups(&mut self, now: i64) -> Result<()> {
        let store = Store::open(Namespace::Reviews)?;
        let found = store.agt_backups();
        let all = self.attempt(store.root(), found).unwrap_or_default();
        for namespace in Namespace::ALL {
            for backup in all.iter().filter(|backup| backup.namespace == namespace) {
                let newest = all
                    .iter()
                    .filter(|candidate| {
                        candidate.namespace == namespace
                            && !candidate.partial
                            && candidate.taken.0 <= now
                            && candidate.path.parent() == backup.path.parent()
                    })
                    .max_by_key(|candidate| (candidate.taken, candidate.path.clone()))
                    .map(|candidate| candidate.path.clone());
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

    /// Long-output copies untouched for a week.
    pub fn output(&mut self, now: i64) -> Result<()> {
        for stale in files::output(now, &mut self.warnings)? {
            let reason = files::reason(&stale, now, OUTPUT_DAYS);
            self.add(
                "output",
                stale.path,
                stale.key,
                reason,
                stale.bytes,
                Action::Files {
                    root: stale.root,
                    days: OUTPUT_DAYS,
                    harness: false,
                },
            );
        }
        Ok(())
    }

    /// Scratch and harness session directories untouched for 30 days.
    pub fn scratch(&mut self, now: i64) -> Result<()> {
        let harness = files::harness_roots()?;
        for stale in files::scratch(now, &mut self.warnings)? {
            let reason = files::reason(&stale, now, SCRATCH_DAYS);
            let in_harness = harness.iter().any(|root| stale.path.starts_with(root));
            self.add(
                "scratch",
                stale.path,
                stale.key,
                reason,
                stale.bytes,
                Action::Files {
                    root: stale.root,
                    days: SCRATCH_DAYS,
                    harness: in_harness,
                },
            );
        }
        Ok(())
    }

    /// Processes whose agent session is gone, a day or more old.
    pub fn processes(&mut self, now: i64) -> Result<()> {
        for orphan in procs::orphans(now)? {
            let reason = procs::reason(&orphan);
            self.add(
                "processes",
                PathBuf::new(),
                orphan.proc.pid.to_string(),
                reason,
                0,
                Action::Process(orphan.proc),
            );
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

fn stores(namespace: Namespace) -> Result<Vec<Store>> {
    let mut found = vec![Store::open(namespace)?];
    let mut roots = crate::worktree::roots::repositories()?;
    if let Ok(repo) = crate::git::Repo::discover() {
        roots.push(repo.root().to_path_buf());
    }
    let mut seen = std::collections::BTreeSet::new();
    for root in roots {
        let repo = crate::git::Repo::at(root);
        let Ok(main) = repo.main_checkout_root() else {
            continue;
        };
        if seen.insert(main) {
            found.push(Store::inspect_repo(namespace, &repo)?);
        }
    }
    Ok(found)
}
