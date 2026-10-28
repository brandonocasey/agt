//! Trackers: the configured repositories, their stored state, and finding an issue by key.

use std::path::PathBuf;

use crate::core::config::{self, Config, Kind};
use crate::core::error::{Result, fail};
use crate::forge::service;
use crate::store::Store;
use crate::store::issues::{Issue, TRACKER_FILE, TrackerDoc, parse_ref};
use crate::store::model::Forge;

/// A tracker's directory and document.
pub struct Tracker {
    pub dir: PathBuf,
    pub doc: TrackerDoc,
}

/// `gh-<host>-<owner>-<repo>`, lowercase with slashes as dashes.
pub fn tracker_key(forge: Forge, host: &str, repo: &str) -> String {
    let prefix = service::kind(forge).key_prefix;
    format!("{prefix}-{host}-{repo}")
        .to_ascii_lowercase()
        .replace('/', "-")
}

/// The trackers `filter` names (by tracker key or `owner/repo`), or all of them.
pub fn configured<'a>(
    trackers: &'a [config::Tracker],
    filter: Option<&str>,
) -> Result<Vec<&'a config::Tracker>> {
    for (index, tracker) in trackers.iter().enumerate() {
        let key = tracker_key(tracker.forge, &tracker.host, &tracker.repo);
        for other in &trackers[..index] {
            if key == tracker_key(other.forge, &other.host, &other.repo) {
                check_identity(&TrackerDoc::new(key.clone(), other), tracker)?;
            }
        }
    }
    let found: Vec<&config::Tracker> = trackers
        .iter()
        .filter(|tracker| {
            filter.is_none_or(|filter| {
                matches(
                    filter,
                    &tracker_key(tracker.forge, &tracker.host, &tracker.repo),
                    &tracker.repo,
                )
            })
        })
        .collect();
    if found.is_empty() {
        return match filter {
            Some(filter) => fail(format!("no configured tracker matches {filter}")),
            None => no_tracker_serves(false),
        };
    }
    Ok(found)
}

/// The error for a repository no tracker serves issues for; `remotes_none` adds why remotes are
/// no help.
fn no_tracker_serves<T>(remotes_none: bool) -> Result<T> {
    let hint = if remotes_none {
        "; `[sync] remotes = \"none\"` turns off remote trackers"
    } else {
        ""
    };
    fail(format!(
        "no tracker serves issues: add a supported origin remote or a [[tracker]] in {}{hint}",
        config::path()?.display()
    ))
}

fn matches(filter: &str, key: &str, repo: &str) -> bool {
    filter.eq_ignore_ascii_case(key) || filter.eq_ignore_ascii_case(repo)
}

/// The tracker document for `tracker` with its configuration refreshed, created if new.
pub fn ensure(store: &Store, tracker: &config::Tracker) -> Result<Tracker> {
    ensure_mode(store, tracker, false)
}

pub fn ensure_mode(store: &Store, tracker: &config::Tracker, preview: bool) -> Result<Tracker> {
    let forge = tracker.forge;
    let key = tracker_key(forge, &tracker.host, &tracker.repo);
    let dir = store.repo_dir(&key)?;
    if preview {
        let doc = store
            .load_doc(&dir, TRACKER_FILE)?
            .unwrap_or_else(|| TrackerDoc::new(key, tracker));
        check_identity(&doc, tracker)?;
        return Ok(Tracker { dir, doc });
    }
    let doc = store.update_doc(
        &dir,
        TRACKER_FILE,
        || TrackerDoc::new(key.clone(), tracker),
        |doc| {
            check_identity(doc, tracker)?;
            doc.refresh(tracker);
            Ok(doc.clone())
        },
    )?;
    Ok(Tracker { dir, doc })
}

pub(super) fn check_identity(doc: &TrackerDoc, tracker: &config::Tracker) -> Result<()> {
    if doc.forge == Some(tracker.forge)
        && doc.host.eq_ignore_ascii_case(&tracker.host)
        && doc.repo.eq_ignore_ascii_case(&tracker.repo)
    {
        return Ok(());
    }
    fail(format!(
        "tracker identity collision for {}: stored {:?} {}/{} differs from configured {:?} {}/{}",
        doc.key, doc.forge, doc.host, doc.repo, tracker.forge, tracker.host, tracker.repo
    ))
}

/// The stored trackers `filter` names, or all of them.
pub fn stored(store: &Store, filter: Option<&str>) -> Result<Vec<Tracker>> {
    let found: Vec<Tracker> = store
        .issue_trackers()?
        .into_iter()
        .filter(|(_, doc)| filter.is_none_or(|filter| matches(filter, &doc.key, &doc.repo)))
        .map(|(dir, doc)| Tracker { dir, doc })
        .collect();
    if found.is_empty()
        && let Some(filter) = filter
    {
        return fail(format!("no synced tracker matches {filter}"));
    }
    Ok(found)
}

/// The issue document `text` names and the tracker it is in.
pub fn find(store: &Store, filter: Option<&str>, text: &str) -> Result<(Tracker, Issue)> {
    let reference = parse_ref(text)?;
    let mut hits = Vec::new();
    for tracker in stored(store, filter)? {
        if reference
            .repo
            .as_deref()
            .is_some_and(|repo| !repo.eq_ignore_ascii_case(&tracker.doc.repo))
        {
            continue;
        }
        if let Some(issue) = store.load_doc::<Issue>(&tracker.dir, &reference.name)? {
            hits.push((tracker, issue));
        }
    }
    match hits.len() {
        0 => fail(format!("no issue {text}; run `agt issue sync` first")),
        1 => Ok(hits.remove(0)),
        _ => fail(format!(
            "{text} is in several trackers; pass --tracker or owner/repo#<number>"
        )),
    }
}

pub fn local(store: &Store) -> Result<Tracker> {
    let dir = store.repo_dir("local")?;
    let doc = store.update_doc(&dir, TRACKER_FILE, TrackerDoc::local, |doc| Ok(doc.clone()))?;
    Ok(Tracker { dir, doc })
}

/// The trackers that serve issues for a command. A command that runs by itself (`automatic`)
/// and names no tracker works on the tracker of `origin` alone.
pub fn destinations(
    config: &Config,
    repo: &crate::git::Repo,
    filter: Option<&str>,
    automatic: bool,
) -> Result<Vec<config::Tracker>> {
    let mut trackers = config.trackers_for(repo, Kind::Issues)?;
    if trackers.is_empty() && filter.is_none() && !automatic {
        return no_tracker_serves(config.sync.remotes == config::RemoteSet::None);
    }
    if automatic && filter.is_none() {
        trackers.retain(|tracker| tracker.remote.as_deref() == Some("origin"));
    }
    if automatic && configured(&trackers, filter)?.len() > 1 {
        return fail("several automatic issue destinations; pass --tracker");
    }
    Ok(trackers)
}
