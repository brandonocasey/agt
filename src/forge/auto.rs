//! Auto-sync: the policy that lets a read pull from the forge before it reads local data. The
//! domains decide what to sync; this module decides whether to, how long a forge call may take
//! and what a failure looks like. A failed auto-sync never fails the read.

use std::path::Path;
use std::time::Duration;

use crate::core::clock;
use crate::core::config::Config;
use crate::core::error::Result;
use crate::forge::transport;
use crate::store::Store;

/// Entries of the lookup file older than this are of no use to any max age worth setting.
pub const DAY: i64 = 24 * 60 * 60;

/// How long all forge calls of one command may take together.
const TIME_CAP: Duration = Duration::from_secs(15);

/// Test-only override of [`TIME_CAP`], in milliseconds, so a slow forge can be checked fast.
const CAP_OVERRIDE: &str = "AGT_SYNC_CAP_MS";

/// What a read syncs by: the configuration, resolved against `--offline` and `AGT_OFFLINE`.
pub struct Policy {
    pub config: Config,
    pub max_age_secs: u64,
}

impl Policy {
    /// `None` when the read stays local: `--offline`, `AGT_OFFLINE`, `sync.auto = false`, or a
    /// configuration that cannot be read, which is reported as a skipped sync because a read
    /// needs no configuration. A read that stays offline never loads the file.
    pub fn resolve(offline: bool) -> Option<Self> {
        if offline || offline_env() {
            return None;
        }
        let config = match Config::load() {
            Ok(config) => config,
            Err(error) => {
                skipped(&error.0);
                return None;
            }
        };
        if !config.sync.auto {
            return None;
        }
        let max_age_secs = config.sync.max_age_secs;
        Some(Self {
            config,
            max_age_secs,
        })
    }

    /// Whether something last synced at `synced_at` is due for another sync. A time that cannot
    /// be read, or lies in the future, is due.
    pub fn is_stale(&self, synced_at: Option<&str>) -> Result<bool> {
        let Some(at) = synced_at.and_then(|at| clock::parse(at).ok()) else {
            return Ok(true);
        };
        let age = clock::now_secs()? - at;
        Ok(u64::try_from(age).map_or(true, |age| age >= self.max_age_secs))
    }

    /// Whether a lookup that found nothing at `checked_at` still holds.
    pub fn is_recent(&self, checked_at: Option<&str>) -> Result<bool> {
        Ok(!self.is_stale(checked_at)?)
    }

    /// Whether the sync named `key` in `dir` failed less than the max age ago, so it is not
    /// tried again yet.
    pub fn backed_off(&self, store: &Store, dir: &Path, key: &str) -> Result<bool> {
        let failed = store.load_lookups(dir).failures.get(key).cloned();
        self.is_recent(failed.as_deref())
    }
}

/// Remembers that the sync named `key` in `dir` failed now. Best effort: a store that cannot be
/// written only means the next read tries again.
pub fn record_failure(store: &Store, dir: &Path, key: &str) {
    let Ok(now) = clock::now_secs() else {
        return;
    };
    let mut lookups = store.load_lookups(dir);
    lookups
        .failures
        .retain(|_, at| clock::parse(at).is_ok_and(|at| (0..DAY).contains(&(now - at))));
    lookups.failures.insert(key.to_owned(), clock::format(now));
    let _ = store.save_lookups(dir, &lookups);
}

/// Forgets a failure of the sync named `key`, after the sync worked.
pub fn clear_failure(store: &Store, dir: &Path, key: &str) {
    let mut lookups = store.load_lookups(dir);
    if lookups.failures.remove(key).is_some() {
        let _ = store.save_lookups(dir, &lookups);
    }
}

/// Runs `work` with every forge call capped at the time budget in total. A failure is reported
/// as one line on stderr and returns `None`.
pub fn attempt<T>(work: impl FnOnce() -> Result<T>) -> Option<T> {
    transport::set_deadline(time_cap());
    let result = work();
    transport::clear_deadline();
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            skipped(&error.0);
            None
        }
    }
}

/// The line a skipped sync leaves on stderr.
pub fn skipped(reason: &str) {
    let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    let reason: String = reason.chars().take(300).collect();
    eprintln!("agt: sync skipped ({reason}); showing local data");
}

/// `AGT_OFFLINE` set to anything but empty, `0` or `false`.
fn offline_env() -> bool {
    std::env::var("AGT_OFFLINE").is_ok_and(|value| !matches!(value.trim(), "" | "0" | "false"))
}

fn time_cap() -> Duration {
    std::env::var(CAP_OVERRIDE)
        .ok()
        .and_then(|millis| millis.parse().ok())
        .map_or(TIME_CAP, Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max_age_secs: u64) -> Policy {
        Policy {
            config: Config::default(),
            max_age_secs,
        }
    }

    #[test]
    fn a_missing_or_unreadable_time_is_stale() {
        let policy = policy(300);
        assert!(policy.is_stale(None).unwrap());
        assert!(policy.is_stale(Some("yesterday")).unwrap());
    }

    #[test]
    fn age_is_compared_with_the_max_age() {
        let policy = policy(300);
        let ago = |secs: i64| clock::format(clock::now_secs().unwrap() - secs);
        assert!(!policy.is_stale(Some(&ago(10))).unwrap());
        assert!(policy.is_stale(Some(&ago(400))).unwrap());
        assert!(policy.is_stale(Some(&ago(-400))).unwrap());
    }
}
