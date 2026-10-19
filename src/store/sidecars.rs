//! The small files beside a repository's documents: the cached forge account, the PR/MR lookup
//! record and the record of where a `local/` repository lives.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::json::{persist_json, read_json};
use super::{Store, create_dir};
use crate::core::clock;
use crate::core::error::Result;
use crate::store::model::Forge;

pub(super) const ACCOUNT_FILE: &str = ".account.json";
pub(super) const REPO_FILE: &str = ".repo.json";
pub(super) const LOOKUP_FILE: &str = ".auto-sync.json";

/// How long `last_seen` in `.repo.json` may go without a refresh.
const SEEN_REFRESH_SECS: i64 = 24 * 60 * 60;

/// Where a `local/` repository lives and when agt last ran in it, so `prune` can tell it is
/// gone. Older agt ignores the file, which is why this is not a field of the target.
#[derive(Serialize, Deserialize)]
pub struct RepoFile {
    pub common_dir: String,
    /// RFC 3339. Absent in files written before it existed; their modification time stands in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
}

/// The forge login `sync` authenticated as, cached per repository for author defaults.
#[derive(Serialize, Deserialize)]
pub struct Account {
    pub forge: Forge,
    pub host: String,
    pub login: String,
}

/// What auto-sync remembers beside the documents it syncs: the branches it last asked the forge
/// for a PR/MR and found none, and what it last failed to sync, each with when, so reads ask
/// again only after a while. Older agt ignores the file, which is why this is not a field of
/// the target or tracker.
#[derive(Default, Serialize, Deserialize)]
pub struct Lookups {
    #[serde(default)]
    pub misses: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub failures: std::collections::BTreeMap<String, String>,
}
impl Store {
    pub fn load_account(&self, dir: &Path) -> Result<Option<Account>> {
        read_json(&dir.join(ACCOUNT_FILE))
    }

    pub fn save_account(&self, dir: &Path, account: &Account) -> Result<()> {
        create_dir(dir)?;
        persist_json(&dir.join(ACCOUNT_FILE), account, || Ok(()))
    }

    /// The PR/MR lookup misses of the repository. A file that cannot be read counts as none:
    /// the next lookup rewrites it.
    pub fn load_lookups(&self, dir: &Path) -> Lookups {
        read_json(&dir.join(LOOKUP_FILE))
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    pub fn save_lookups(&self, dir: &Path, lookups: &Lookups) -> Result<()> {
        create_dir(dir)?;
        persist_json(&dir.join(LOOKUP_FILE), lookups, || Ok(()))
    }

    /// Writes the repository's absolute git common dir and `now` beside its targets. A record
    /// that names the same directory and was refreshed within a day is left alone, so most
    /// commands write nothing.
    pub fn record_repo(&self, dir: &Path, common_dir: &str, now: i64) -> Result<()> {
        let path = dir.join(REPO_FILE);
        if let Ok(Some(known)) = read_json::<RepoFile>(&path)
            && known.common_dir == common_dir
            && let Some(seen) = known
                .last_seen
                .as_deref()
                .and_then(|seen| clock::parse(seen).ok())
            && (0..SEEN_REFRESH_SECS).contains(&(now - seen))
        {
            return Ok(());
        }
        create_dir(dir)?;
        let file = RepoFile {
            common_dir: common_dir.to_owned(),
            last_seen: Some(clock::format(now)),
        };
        persist_json(&path, &file, || Ok(()))
    }

    /// The git common dir recorded by [`Store::record_repo`], if any.
    pub fn recorded_repo(&self, dir: &Path) -> Result<Option<RepoFile>> {
        read_json(&dir.join(REPO_FILE))
    }
}
