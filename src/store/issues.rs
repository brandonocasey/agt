//! What `agt issue` persists per tracker: one `tracker.json` and one document per issue.

use serde::{Deserialize, Serialize};

use crate::core::config::{self, Scope};
use crate::core::local::lock_file;
use crate::store::doc_path;
use crate::store::model::{Comment, Forge};

pub const VERSION: u32 = 1;

/// The file that holds a tracker's own state next to its issues.
pub const TRACKER_FILE: &str = "tracker";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    Open,
    Closed,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

/// Every field of an issue that can be written back. `base` holds the last synced copy.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Fields {
    pub title: String,
    #[serde(default)]
    pub parent: Option<String>,
    pub body: String,
    pub state: Category,
    /// Why a closed issue closed (GitHub `completed` or `not_planned`); `None` when open or the
    /// service gives no reason.
    pub reason: Option<String>,
    /// Sorted, without duplicates.
    pub labels: Vec<String>,
    /// Sorted, without duplicates.
    pub assignees: Vec<String>,
}

impl Fields {
    pub fn normalize(&mut self) {
        self.labels.sort();
        self.labels.dedup();
        self.assignees.sort();
        self.assignees.dedup();
        if self.state == Category::Open {
            self.reason = None;
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RemoteInfo {
    /// The host's id of the issue: GitHub's node id, GitLab's global id.
    pub id: String,
    pub url: String,
    /// The host's `updated_at` as of the last sync; a change means the issue moved.
    pub updated_at: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum LinkSource {
    Remote,
    Local,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct PrLink {
    pub forge: Forge,
    pub repo: String,
    pub number: u64,
    pub url: Option<String>,
    /// The source branch of the PR/MR, when the host said.
    pub branch: Option<String>,
    pub state: Option<String>,
    pub source: LinkSource,
}

impl PrLink {
    /// Adds the link to `links` unless one for the same PR/MR is there.
    pub fn push_unique(self, links: &mut Vec<Self>) {
        if !links
            .iter()
            .any(|known| known.number == self.number && known.repo == self.repo)
        {
            links.push(self);
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
pub struct Links {
    pub prs: Vec<PrLink>,
    /// Keys of related issues.
    pub issues: Vec<String>,
    /// Keys of review targets.
    pub reviews: Vec<String>,
    pub tasks: Vec<String>,
    pub branches: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Note {
    pub id: String,
    pub text: String,
    pub created_at: String,
}

/// A field both sides changed. The remote value won; `local` is what `push --force` sends.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Conflict {
    pub field: String,
    pub local: Fields,
    pub remote: Fields,
    pub seen_at: String,
}

/// Why the last send of something failed; the thing stays queued.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WriteError {
    /// `create`, `update` or a local comment id.
    pub what: String,
    pub message: String,
    /// The host refused it (a 4xx other than a timeout or rate limit), so repeating it
    /// unchanged will fail again.
    pub refused: bool,
    pub at: String,
}

impl Issue {
    /// The name of the document: the number, or the local key before the host knows it.
    pub fn doc_name(&self) -> String {
        self.number
            .map_or_else(|| self.key.clone(), |number| number.to_string())
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Issue {
    pub version: u32,
    /// `owner/repo#12`, or `local-<id>` before the host knows it.
    pub key: String,
    pub tracker: String,
    #[serde(default)]
    pub local_key: Option<String>,
    #[serde(default)]
    pub task: bool,
    #[serde(default)]
    pub sub_issues: Vec<String>,
    pub number: Option<u64>,
    pub remote: Option<RemoteInfo>,
    pub fields: Fields,
    /// `None` until the first sync or send; then the last value both sides agreed on.
    pub base: Option<Fields>,
    pub author: Option<String>,
    /// The host's own state word, such as `opened`.
    pub native_state: Option<String>,
    pub milestone: Option<String>,
    pub due_date: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub synced_at: Option<String>,
    pub comments: Vec<Comment>,
    pub links: Links,
    pub notes: Vec<Note>,
    pub conflicts: Vec<Conflict>,
    pub errors: Vec<WriteError>,
    /// The crash-safe state of creating the issue, for a local issue.
    pub create: Option<crate::store::model::Delivery>,
}

/// A tracker's configuration as of its last sync, and its sync bookkeeping.
#[derive(Serialize, Deserialize, Clone)]
pub struct TrackerDoc {
    pub version: u32,
    pub key: String,
    pub forge: Option<Forge>,
    pub host: String,
    pub repo: String,
    pub scope: Scope,
    pub since_days: u32,
    pub push: bool,
    /// The newest `updated_at` seen; the next sync lists from just before it.
    pub watermark: Option<String>,
    pub login: Option<String>,
    pub synced_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hierarchy_cursor: Option<u64>,
}

impl TrackerDoc {
    /// The document of a configured tracker, as first created.
    pub fn new(key: String, tracker: &config::Tracker) -> Self {
        let mut doc = Self {
            version: VERSION,
            key,
            forge: Some(tracker.forge),
            host: tracker.host.clone(),
            repo: tracker.repo.clone(),
            scope: tracker.scope,
            since_days: tracker.since_days,
            push: false,
            watermark: None,
            login: None,
            synced_at: None,
            hierarchy_cursor: None,
        };
        doc.refresh(tracker);
        doc
    }

    /// The document of the tracker for issues that exist only on this machine.
    pub fn local() -> Self {
        Self {
            version: VERSION,
            key: "local".into(),
            forge: None,
            host: String::new(),
            repo: "local".into(),
            scope: Scope::All,
            since_days: 90,
            push: false,
            watermark: None,
            login: None,
            synced_at: None,
            hierarchy_cursor: None,
        }
    }

    /// Takes the settings of `tracker` that may change between syncs.
    pub fn refresh(&mut self, tracker: &config::Tracker) {
        self.scope = tracker.scope;
        self.since_days = tracker.since_days;
        self.push = tracker.push_mode() != config::Mode::Off;
    }
}

/// What a user typed for an issue: `owner/repo#12`, `#12`, `12` or `local-xxxx`.
pub struct IssueRef {
    pub repo: Option<String>,
    pub name: String,
}

pub fn parse_ref(text: &str) -> crate::core::error::Result<IssueRef> {
    let invalid = || crate::core::error::Error(format!("not an issue key: {text}"));
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

impl super::Store {
    /// Every stored tracker: its directory and document.
    pub fn issue_trackers(
        &self,
    ) -> crate::core::error::Result<Vec<(std::path::PathBuf, TrackerDoc)>> {
        let mut found = Vec::new();
        for dir in self.child_dirs()? {
            if let Some(doc) = self.load_doc::<TrackerDoc>(&dir, TRACKER_FILE)? {
                found.push((dir, doc));
            }
        }
        Ok(found)
    }

    pub fn attach_issue(
        &self,
        source: &std::path::Path,
        target: &std::path::Path,
        name: &str,
    ) -> crate::core::error::Result<()> {
        super::validate_key(name)?;
        let _source = lock_file(&super::lock_path(source, name))?;
        let _target = lock_file(&super::lock_path(target, name))?;
        let destination = doc_path(target, name);
        match std::fs::symlink_metadata(&destination) {
            Ok(_) => {
                return crate::core::error::fail(format!(
                    "{} already exists",
                    destination.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(crate::core::error::Error(format!(
                    "stat {}: {error}",
                    destination.display()
                )));
            }
        }
        std::fs::rename(doc_path(source, name), destination)
            .map_err(crate::core::error::ctx("attach local issue"))?;
        for directory in [source, target] {
            std::fs::File::open(directory)
                .and_then(|file| file.sync_all())
                .map_err(crate::core::error::ctx("sync attached issue directory"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn attaching_keeps_a_broken_destination_symlink_and_the_local_issue() {
        let temp = tempfile::tempdir().unwrap();
        let store = super::super::Store::at(temp.path(), super::super::Namespace::Issues);
        let source = temp.path().join("local");
        let target = temp.path().join("remote");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        let file = source.join("local-test.json");
        std::fs::write(&file, "private issue").unwrap();
        let destination = target.join("local-test.json");
        std::os::unix::fs::symlink(temp.path().join("missing"), &destination).unwrap();

        assert!(store.attach_issue(&source, &target, "local-test").is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "private issue");
        assert!(
            std::fs::symlink_metadata(destination)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
