//! `sync`: list what changed on each configured tracker, fetch those issues and merge them into
//! the local copies.

use serde_json::{Value, json};

use super::hierarchy::Index;
use super::merge;
use super::trackers::{self, Tracker};
use crate::core::clock;
use crate::core::config;
use crate::core::error::{Error, Result};
use crate::forge::issue::{IssueData, ListQuery};
use crate::forge::markers::is_landed_as;
use crate::forge::service::{self, IssueService, IssueSubject};
use crate::store::Store;
use crate::store::issues::{Issue, TRACKER_FILE};

/// A sync lists from this long before the newest `updated_at` it saw, so an issue the host
/// stamped just before the previous listing but showed only after it is not missed.
const OVERLAP_SECS: i64 = 300;

pub fn connect(tracker: &Tracker) -> Result<(Box<dyn IssueService>, String)> {
    let forge = tracker
        .doc
        .forge
        .ok_or_else(|| Error("local tracker has no remote destination".into()))?;
    let service = (service::kind(forge).connect_issue)(&tracker.doc.host);
    let login = service.login()?;
    Ok((service, login))
}

/// What a sync did: one report per tracker, and the trackers (named `host/repo`) that failed.
pub struct Synced {
    pub report: Value,
    pub failed: Vec<(String, String)>,
}

pub fn run(
    store: &Store,
    destinations: &[config::Tracker],
    filter: Option<&str>,
) -> Result<Synced> {
    let mut reports = Vec::new();
    let mut failed = Vec::new();
    let targets = trackers::configured(destinations, filter)?;
    for configured in targets {
        let outcome = trackers::ensure(store, configured)
            .and_then(|tracker| sync_tracker(store, tracker, None));
        match outcome {
            Ok(report) => reports.push(report),
            Err(error) => {
                reports.push(json!({"tracker": configured.name(), "error": error.0}));
                failed.push((configured.name(), error.0));
            }
        }
    }
    Ok(Synced {
        report: json!({ "trackers": reports }),
        failed,
    })
}

pub(super) fn sync_tracker(
    store: &Store,
    tracker: Tracker,
    hierarchy_budget: Option<usize>,
) -> Result<Value> {
    let (service, login) = connect(&tracker)?;
    let now = clock::now()?;
    let since = match &tracker.doc.watermark {
        Some(mark) => clock::format(clock::parse(mark)? - OVERLAP_SECS),
        None => clock::format(
            clock::now_secs()? - i64::from(tracker.doc.since_days) * clock::SECS_PER_DAY,
        ),
    };
    let listed = service.list_changed(&ListQuery {
        repo: &tracker.doc.repo,
        scope: tracker.doc.scope,
        since: &since,
        login: &login,
    })?;
    let claimed = claim_created(store, &tracker, &listed, &login)?;
    let (mut new, mut updated, mut unchanged, mut conflicts) = (0, 0, 0, 0);
    let index = Index::load(store, &tracker)?;
    for data in &listed {
        let name = data.number.to_string();
        let existing = store.load_doc::<Issue>(&tracker.dir, &name)?;
        let current = existing.as_ref().is_some_and(|issue| {
            issue
                .remote
                .as_ref()
                .is_some_and(|remote| remote.updated_at == data.updated_at)
        });
        let mut hierarchy = service
            .hierarchy(IssueSubject {
                repo: &tracker.doc.repo,
                number: data.number,
            })?
            .ok_or_else(|| {
                Error(format!(
                    "listed issue {}#{} is no longer available; retry sync",
                    tracker.doc.repo, data.number
                ))
            })?;
        hierarchy.parent = hierarchy.parent.map(|key| index.canonical(&key).to_owned());
        if current {
            let count = store.update_existing_doc(&tracker.dir, &name, |issue: &mut Issue| {
                index.normalize(issue);
                let count = merge::apply_parent(issue, &hierarchy.parent, hierarchy.task, &now);
                issue.sub_issues.clone_from(&hierarchy.children);
                Ok(count)
            })?;
            conflicts += count;
            unchanged += 1;
            continue;
        }
        let mut fetched = service.fetch_issue(IssueSubject {
            repo: &tracker.doc.repo,
            number: data.number,
        })?;
        fetched.issue.parent = hierarchy.parent;
        fetched.issue.task = hierarchy.task;
        let merged = store.update_doc(
            &tracker.dir,
            &name,
            || merge::from_remote(&tracker.doc.key, &tracker.doc.repo, data, &now),
            |issue| {
                index.normalize(issue);
                issue.sub_issues.clone_from(&hierarchy.children);
                Ok(merge::apply(issue, &fetched, &login, &now))
            },
        )?;
        conflicts += merged.conflicts;
        if existing.is_some() {
            updated += 1;
        } else {
            new += 1;
        }
    }
    let listed_numbers: std::collections::HashSet<_> =
        listed.iter().map(|data| data.number).collect();
    let mut hierarchy_refreshed = listed.len();
    let mut unavailable = Vec::new();
    let mut cached: Vec<_> = index
        .issues()
        .filter(|issue| {
            issue
                .number
                .is_some_and(|number| !listed_numbers.contains(&number))
        })
        .collect();
    cached.sort_unstable_by_key(|issue| issue.number.unwrap());
    if hierarchy_budget.is_some() {
        cached.retain(|issue| {
            tracker
                .doc
                .hierarchy_cursor
                .is_none_or(|cursor| issue.number.unwrap() > cursor)
        });
    }
    let remaining = cached.len();
    let limit = hierarchy_budget.unwrap_or(remaining);
    for (position, existing) in cached.into_iter().take(limit).enumerate() {
        let hierarchy = service.hierarchy(IssueSubject {
            repo: &tracker.doc.repo,
            number: existing.number.unwrap(),
        })?;
        if let Some(hierarchy) = hierarchy {
            let parent = hierarchy.parent.map(|key| index.canonical(&key).to_owned());
            conflicts += store.update_existing_doc(
                &tracker.dir,
                &existing.doc_name(),
                |issue: &mut Issue| {
                    index.normalize(issue);
                    let count = merge::apply_parent(issue, &parent, hierarchy.task, &now);
                    issue.sub_issues.clone_from(&hierarchy.children);
                    Ok(count)
                },
            )?;
            hierarchy_refreshed += 1;
        } else {
            unavailable.push(existing.key.clone());
        }
        if hierarchy_budget.is_some() {
            store.update_existing_doc(
                &tracker.dir,
                TRACKER_FILE,
                |doc: &mut crate::store::issues::TrackerDoc| {
                    doc.hierarchy_cursor = if position + 1 == remaining {
                        None
                    } else {
                        existing.number
                    };
                    Ok(())
                },
            )?;
        }
    }
    unavailable.sort();
    let newest = listed.iter().map(|data| data.updated_at.clone()).max();
    store.update_doc(
        &tracker.dir,
        TRACKER_FILE,
        || tracker.doc.clone(),
        |doc| {
            if hierarchy_budget.is_none() || remaining == 0 {
                doc.hierarchy_cursor = None;
            }
            doc.login = Some(login.clone());
            doc.synced_at = Some(now.clone());
            if newest > doc.watermark {
                doc.watermark = newest.clone();
            }
            Ok(())
        },
    )?;
    crate::forge::auto::clear_failure(store, &tracker.dir, super::auto::FAILURE_KEY);
    Ok(json!({
        "tracker": tracker.doc.key,
        "listed": listed.len(),
        "hierarchy_refreshed": hierarchy_refreshed,
        "unavailable": unavailable,
        "new": new,
        "updated": updated,
        "unchanged": unchanged,
        "claimed": claimed,
        "conflicts": conflicts,
    }))
}

/// Local issues whose create was sent but whose response was lost: the host's copy carries the
/// marker `push` put in its body. Gives each its number and moves it to its numbered name.
fn claim_created(
    store: &Store,
    tracker: &Tracker,
    listed: &[IssueData],
    login: &str,
) -> Result<usize> {
    let mut claimed = 0;
    for local in store.docs::<Issue>(&tracker.dir, &[TRACKER_FILE])? {
        if local.number.is_some()
            || !local
                .create
                .as_ref()
                .is_some_and(|create| create.is_posting())
        {
            continue;
        }
        let Some(found) = listed
            .iter()
            .find(|data| is_created_by(&local, data, login))
        else {
            continue;
        };
        adopt(store, tracker, &local.key, found)?;
        claimed += 1;
    }
    Ok(claimed)
}

/// Whether `data` is the issue `local`'s posting create made.
pub fn is_created_by(local: &Issue, data: &IssueData, login: &str) -> bool {
    data.author.as_deref() == Some(login)
        && local
            .create
            .as_ref()
            .is_some_and(|create| is_landed_as(create, &local.key, &data.body))
}

/// Gives the local issue `local_key` the identity of `data` and renames it to its number. The
/// comments and fields stay; the next fetch merges the host's copy in.
pub fn adopt(store: &Store, tracker: &Tracker, local_key: &str, data: &IssueData) -> Result<()> {
    let key = format!("{}#{}", tracker.doc.repo, data.number);
    let seed = store
        .load_doc::<Issue>(&tracker.dir, local_key)?
        .ok_or_else(|| Error(format!("issue {local_key} vanished")))?;
    store.update_doc(
        &tracker.dir,
        local_key,
        || seed.clone(),
        |issue| {
            if issue.local_key.is_none() {
                issue.local_key = Some(local_key.to_owned());
            }
            issue.key = key.clone();
            issue.number = Some(data.number);
            issue.remote = Some(crate::store::issues::RemoteInfo {
                id: data.id.clone(),
                url: data.url.clone(),
                updated_at: String::new(),
            });
            issue.author.clone_from(&data.author);
            issue.task = data.task;
            if let Some(create) = issue.create.as_mut() {
                create.post = crate::store::model::PostState::Posted;
            }
            let mut sent = merge::remote_fields(data);
            sent.state = crate::store::issues::Category::Open;
            sent.reason = None;
            issue.base = Some(sent);
            Ok(())
        },
    )?;
    store.rename_doc(&tracker.dir, local_key, &data.number.to_string())
}
