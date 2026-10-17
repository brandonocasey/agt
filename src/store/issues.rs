//! What `agt issue` persists per tracker: one `tracker.json` and one document per issue.

use serde::{Deserialize, Serialize};

use crate::core::config::Scope;
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

#[derive(Serialize, Deserialize, Clone)]
pub struct Issue {
    pub version: u32,
    /// `owner/repo#12`, or `local-<id>` before the host knows it.
    pub key: String,
    pub tracker: String,
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
    pub forge: Forge,
    pub host: String,
    pub repo: String,
    pub scope: Scope,
    pub since_days: u32,
    pub push: bool,
    /// The newest `updated_at` seen; the next sync lists from just before it.
    pub watermark: Option<String>,
    pub login: Option<String>,
    pub synced_at: Option<String>,
}
