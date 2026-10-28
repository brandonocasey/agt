//! Auto-sync: the policy that lets a read pull from the forge before it reads local data. The
//! domains decide what to sync; this module decides whether to, how long a forge call may take
//! and what a failure looks like. A failed auto-sync never fails the read.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::Args;

use crate::core::clock;
use crate::core::config::{Config, Kind, Mode, Tracker};
use crate::core::env;
use crate::core::error::{Result, fail};
use crate::forge::transport;
use crate::store::Store;
use crate::store::sidecars::Lookups;

/// How long all forge calls of one command may take together.
const TIME_CAP: Duration = Duration::from_secs(15);

/// Test-only override of [`TIME_CAP`], in milliseconds, so a slow forge can be checked fast.
const CAP_OVERRIDE: &str = "AGT_SYNC_CAP_MS";

/// What a read syncs by: the configuration, resolved against `--offline` and `AGT_OFFLINE`.
pub struct Policy {
    pub config: Config,
    pub kind: Kind,
    /// The trackers that serve `kind`.
    pub trackers: Vec<Tracker>,
    pub max_age_secs: u64,
}

impl Policy {
    /// The policy for reads of `kind`; `None` when offline, or when no tracker pulls
    /// automatically.
    pub fn resolve_for_repo(repo: &crate::git::Repo, offline: Offline, kind: Kind) -> Option<Self> {
        let config = load_unless_offline(repo, offline)?;
        if !config.may_be_automatic(kind, true) {
            return None;
        }
        let trackers = match config.trackers_for(repo, kind) {
            Ok(trackers) => trackers,
            Err(error) => {
                skipped(&error.0);
                return None;
            }
        };
        let pulls = trackers.iter().any(|tracker| tracker.pull == Mode::Auto)
            || (trackers.is_empty() && config.pull_for(kind) == Mode::Auto);
        pulls.then_some(Self {
            max_age_secs: config.sync.max_age_secs,
            kind,
            trackers,
            config,
        })
    }

    /// Whether reads of the repository `repo` on `host` pull on their own.
    pub fn pulls_automatically(&self, host: &str, repo: &str) -> bool {
        self.trackers
            .iter()
            .find(|tracker| tracker.is_at(host, repo))
            .map_or_else(|| self.config.pull_for(self.kind), |tracker| tracker.pull)
            == Mode::Auto
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
    Lookups::remember(&mut lookups.failures, key, now);
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

/// The configuration when the command is not offline and some tracker of `kind` pushes
/// automatically; otherwise `None`, with a configuration that cannot be read left as a
/// skipped-sync line on stderr.
pub fn push_config(repo: &crate::git::Repo, offline: Offline, kind: Kind) -> Option<Config> {
    let config = load_unless_offline(repo, offline)?;
    if !config.may_be_automatic(kind, false) {
        return None;
    }
    let trackers = match config.trackers_for(repo, kind) {
        Ok(trackers) => trackers,
        Err(error) => {
            skipped(&error.0);
            return None;
        }
    };
    let pushes = trackers
        .iter()
        .any(|tracker| tracker.push_mode() == Mode::Auto)
        || (trackers.is_empty() && config.push_for(kind) == Some(Mode::Auto));
    pushes.then_some(config)
}

/// Why `what` stays local: `None` unless `push` is `off`, which is also what an unset `push`
/// means here. The reason names the key to change and also goes to stderr, so a command that kept
/// its work local says so.
pub fn kept_local(what: &str, kind: Kind, target: &str, push: Option<Mode>) -> Option<String> {
    if push.is_some_and(|push| push != Mode::Off) {
        return None;
    }
    let global = crate::core::config::path()
        .map_or_else(|_| "config.toml".into(), |path| path.display().to_string());
    let reason = format!(
        "{what} kept local: push is off for {target}; set `push = \"manual\"` under `[{}]`, \
         `[sync]` or the tracker in {global} or the repository's .config/agt.toml to send",
        kind.section()
    );
    eprintln!("agt: {reason}");
    Some(reason)
}

/// Refuses `what`, a write of `agt pr`, when the tracker's `push` was set to `off`. Unset allows
/// it.
pub fn pr_write_allowed(what: &str, target: &str, push: Option<Mode>) -> Result<()> {
    if push != Some(Mode::Off) {
        return Ok(());
    }
    fail(format!(
        "{what} refused: push is off for {target}; set `push = \"manual\"` under \
         `[pull_requests]`, `[sync]` or the tracker to allow it"
    ))
}

fn load_unless_offline(repo: &crate::git::Repo, offline: Offline) -> Option<Config> {
    if offline.is_set() {
        return None;
    }
    match Config::load_for_repo(repo) {
        Ok(config) => Some(config),
        Err(error) => {
            skipped(&error.0);
            None
        }
    }
}

/// The locks a sync holds: the repository's sync lock and, when a target is being published,
/// that target's publish lock. Released on drop.
pub struct SyncLocks {
    _sync: File,
    _other: Option<File>,
}

/// Takes the lock named `sync_key` and then `other` (a key and the message for when it is
/// held); `Err` is the reason another process holds one.
pub fn lock_for_sync(
    store: &Store,
    dir: &Path,
    sync_key: &str,
    other: Option<(&str, &'static str)>,
) -> Result<std::result::Result<SyncLocks, &'static str>> {
    let Some(_sync) = store.try_publish_lock(dir, sync_key)? else {
        return Ok(Err("another agt is syncing"));
    };
    let _other = match other {
        Some((key, busy)) => match store.try_publish_lock(dir, key)? {
            Some(lock) => Some(lock),
            None => return Ok(Err(busy)),
        },
        None => None,
    };
    Ok(Ok(SyncLocks { _sync, _other }))
}

/// The root `--offline` flag, which `AGT_OFFLINE` set to anything but empty, `0` or `false` also
/// turns on. `agt` records it once after parsing and every command reads it with
/// [`Offline::current`].
#[derive(Args, Clone, Copy)]
pub struct Offline {
    /// Disable automatic and explicit remote access (also `AGT_OFFLINE=1`).
    #[arg(long = "offline", global = true)]
    given: bool,
}

static GIVEN: AtomicBool = AtomicBool::new(false);

impl Offline {
    /// Records the parsed flag for [`Offline::current`].
    pub fn install(self) {
        GIVEN.store(self.given, Ordering::Relaxed);
    }

    /// The flag the command line carried (off until [`Offline::install`] runs).
    pub fn current() -> Self {
        Self {
            given: GIVEN.load(Ordering::Relaxed),
        }
    }

    pub fn is_set(self) -> bool {
        self.given || env::flag("AGT_OFFLINE")
    }

    /// Refuses `what` when offline.
    pub fn forbid(self, what: &str) -> Result<()> {
        if self.is_set() {
            return fail(format!("offline mode forbids {what}"));
        }
        Ok(())
    }
}

fn time_cap() -> Duration {
    env::number(CAP_OVERRIDE).map_or(TIME_CAP, Duration::from_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max_age_secs: u64) -> Policy {
        Policy {
            config: Config::default(),
            kind: Kind::Issues,
            trackers: Vec::new(),
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
