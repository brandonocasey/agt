//! Trackers: the configured repositories, their stored state, and finding an issue by key.

use std::path::PathBuf;

use crate::core::config::{self, Config, TrackerForge};
use crate::core::error::{Error, Result, fail};
use crate::store::Store;
use crate::store::issues::{Issue, TRACKER_FILE, TrackerDoc, VERSION};
use crate::store::model::Forge;

/// A tracker's directory and document.
pub struct Tracker {
    pub dir: PathBuf,
    pub doc: TrackerDoc,
}

pub fn forge_of(forge: TrackerForge) -> Forge {
    match forge {
        TrackerForge::Github => Forge::Github,
        TrackerForge::Gitlab => Forge::Gitlab,
    }
}

/// `gh-<host>-<owner>-<repo>`, lowercase with slashes as dashes.
pub fn tracker_key(forge: Forge, host: &str, repo: &str) -> String {
    let prefix = crate::forge::service::kind(forge).key_prefix;
    format!("{prefix}-{host}-{repo}")
        .to_ascii_lowercase()
        .replace('/', "-")
}

/// The configured trackers `filter` names (by tracker key or `owner/repo`), or all of them.
pub fn configured<'a>(
    config: &'a Config,
    filter: Option<&str>,
) -> Result<Vec<&'a config::Tracker>> {
    let found: Vec<&config::Tracker> = config
        .issues
        .tracker
        .iter()
        .filter(|tracker| {
            filter.is_none_or(|filter| {
                matches(
                    filter,
                    &tracker_key(forge_of(tracker.forge), &tracker.host, &tracker.repo),
                    &tracker.repo,
                )
            })
        })
        .collect();
    if found.is_empty() {
        return match filter {
            Some(filter) => fail(format!("no configured tracker matches {filter}")),
            None => fail(format!(
                "no [[issues.tracker]] is configured in {}",
                config::path()?.display()
            )),
        };
    }
    Ok(found)
}

fn matches(filter: &str, key: &str, repo: &str) -> bool {
    filter.eq_ignore_ascii_case(key) || filter.eq_ignore_ascii_case(repo)
}

/// The tracker document for `tracker` with its configuration refreshed, created if new.
pub fn ensure(store: &Store, tracker: &config::Tracker) -> Result<Tracker> {
    let forge = forge_of(tracker.forge);
    let key = tracker_key(forge, &tracker.host, &tracker.repo);
    let dir = store.repo_dir(&key)?;
    let doc = store.update_doc(
        &dir,
        TRACKER_FILE,
        || TrackerDoc {
            version: VERSION,
            key: key.clone(),
            forge,
            host: tracker.host.clone(),
            repo: tracker.repo.clone(),
            scope: tracker.scope,
            since_days: tracker.since_days,
            push: tracker.push,
            watermark: None,
            login: None,
            synced_at: None,
        },
        |doc| {
            doc.scope = tracker.scope;
            doc.since_days = tracker.since_days;
            doc.push = tracker.push;
            Ok(doc.clone())
        },
    )?;
    Ok(Tracker { dir, doc })
}

/// The stored trackers `filter` names, or all of them.
pub fn stored(store: &Store, filter: Option<&str>) -> Result<Vec<Tracker>> {
    let mut found = Vec::new();
    for dir in store.child_dirs()? {
        let Some(doc) = store.load_doc::<TrackerDoc>(&dir, TRACKER_FILE)? else {
            continue;
        };
        if filter.is_none_or(|filter| matches(filter, &doc.key, &doc.repo)) {
            found.push(Tracker { dir, doc });
        }
    }
    if found.is_empty()
        && let Some(filter) = filter
    {
        return fail(format!("no synced tracker matches {filter}"));
    }
    Ok(found)
}

/// What a user typed for an issue: `owner/repo#12`, `#12`, `12` or `local-xxxx`.
pub struct IssueRef {
    pub repo: Option<String>,
    pub name: String,
}

pub fn parse_ref(text: &str) -> Result<IssueRef> {
    let invalid = || Error(format!("not an issue key: {text}"));
    if text.starts_with("local-") && text.len() > 6 {
        return Ok(IssueRef {
            repo: None,
            name: text.to_owned(),
        });
    }
    let (repo, number) = match text.rsplit_once('#') {
        Some((repo, number)) => ((!repo.is_empty()).then(|| repo.to_owned()), number),
        None => (None, text),
    };
    number.parse::<u64>().map_err(|_| invalid())?;
    Ok(IssueRef {
        repo,
        name: number.to_owned(),
    })
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

/// The name of the document for `issue`: its number, or its local key.
pub fn doc_name(issue: &Issue) -> String {
    issue
        .number
        .map_or_else(|| issue.key.clone(), |number| number.to_string())
}
