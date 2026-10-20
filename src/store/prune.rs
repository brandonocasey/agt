//! What `agt cleanup` may delete from the store, and the locked, non-blocking ways to delete it, so
//! a prune never waits for or corrupts a concurrent agt run.

pub mod backups;
mod local_repo;
mod presence;
mod stray;

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use super::schema::read_target_intact;
use super::{Store, ctx, fail, lock_path, publish_lock_path, validate_key};
use crate::core::error::Result;
use crate::core::local::{is_current, try_lock_file};
use crate::store::model::Target;

/// How long a finished thing is kept before `prune` drops it.
pub const PRUNE_AFTER_SECS: i64 = 90 * 24 * 60 * 60;

/// Whether something last changed at `last` has been kept for [`PRUNE_AFTER_SECS`] at `now`.
pub fn past_retention(now: i64, last: i64) -> bool {
    now - last >= PRUNE_AFTER_SECS
}

/// The outcome of one guarded deletion.
#[derive(Debug, PartialEq, Eq)]
pub enum Removal {
    /// Deleted; the bytes freed.
    Removed(u64),
    /// The thing is gone or no longer qualifies.
    Kept,
    /// Another process holds its lock.
    Busy,
}

pub(super) fn remove_with_lock(path: &Path, lock_path: &Path) -> Result<u64> {
    let bytes = fs::metadata(path).map_or(0, |meta| meta.len());
    fs::remove_file(path).map_err(ctx(format!("remove {}", path.display())))?;
    // Waiters on the old lock inode notice the unlink and retry on a fresh file.
    fs::remove_file(lock_path).map_err(ctx(format!("remove {}", lock_path.display())))?;
    Ok(bytes)
}

impl Store {
    /// Like [`Store::remove_if`], but gives up with [`Removal::Busy`] instead of waiting.
    pub fn try_remove_if(
        &self,
        dir: &Path,
        key: &str,
        remove: impl FnOnce(&Target) -> Result<bool>,
    ) -> Result<Removal> {
        validate_key(key)?;
        let lock = lock_path(dir, key);
        let Some(_lock) = try_lock_file(&lock)? else {
            return Ok(Removal::Busy);
        };
        // A publish in flight has the target's comments in hand; deleting under it would lose them.
        let publish_path = publish_lock_path(dir, key);
        let created = !publish_path.exists();
        let Some(file) = try_lock_file(&publish_path)? else {
            return Ok(Removal::Busy);
        };
        // Taking the lock may have made its file; it goes again on every way out unless it
        // belongs to a target that stays.
        let mut publish = PublishLock {
            file,
            path: &publish_path,
            unlink: created,
        };
        let path = dir.join(format!("{key}.json"));
        // A file with fields this agt does not know may be newer than it; it stays.
        let Some(Ok(target)) = read_target_intact(&path, self.schema)? else {
            return Ok(Removal::Kept);
        };
        if !remove(&target)? {
            return Ok(Removal::Kept);
        }
        publish.unlink = true;
        let bytes = remove_with_lock(&path, &lock)?;
        Ok(Removal::Removed(bytes))
    }
}

/// A held publish lock that deletes its file when dropped and `unlink` is set, provided the path
/// still names the file held, so a replacement made by someone else is never removed.
struct PublishLock<'a> {
    file: fs::File,
    path: &'a Path,
    unlink: bool,
}

impl Drop for PublishLock<'_> {
    fn drop(&mut self) {
        if self.unlink && is_current(&self.file, self.path).unwrap_or(false) {
            let _ = fs::remove_file(self.path);
        }
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => fail(format!("remove {}: {error}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::backups::parse_backup_name;
    use super::local_repo::only_holds;
    use super::presence::Presence;
    use super::stray::{StrayLock, lock_stray_file, lock_target_gone};
    use super::*;
    use crate::core::clock;
    use crate::store::Namespace;
    use crate::store::sidecars::REPO_FILE;

    /// Retries while `attempt` reports [`Removal::Busy`] for a few seconds. A lock is an open file
    /// description, and a thread that forks a child (other tests run `git`) hands it to the child
    /// until the child's `exec` closes it, so a lock released here can stay held for a moment.
    fn settled(mut attempt: impl FnMut() -> Removal) -> Removal {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let removal = attempt();
            if removal != Removal::Busy || std::time::Instant::now() > deadline {
                return removal;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Like [`settled`] for a removal of `k` that is expected to end in `Ok(Kept)` or an error.
    fn try_remove_settled(store: &Store, dir: &Path) -> Result<Removal> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let result = store.try_remove_if(dir, "k", |_| Ok(true));
            if !matches!(result, Ok(Removal::Busy)) || std::time::Instant::now() > deadline {
                return result;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn only_the_exact_backup_names_parse() {
        let (namespace, (taken, attempt)) =
            parse_backup_name("reviews-v1-1970-01-02T00-00-01Z").unwrap();
        assert_eq!((namespace, taken, attempt), (Namespace::Reviews, 86_401, 1));
        assert_eq!(
            parse_backup_name("issues-v12-1970-01-02T00-00-01Z-3")
                .unwrap()
                .1,
            (86_401, 3)
        );
        for foreign in [
            "reviews-v1",
            "reviews-vx-1970-01-02T00-00-01Z",
            "reviews-v1-1970-01-02T00-00-01Z-1",
            "reviews-v1-1970-01-02T00-00-01Zjunk",
            "notes-v1-1970-01-02T00-00-01Z",
            "repo",
        ] {
            assert!(parse_backup_name(foreign).is_none(), "{foreign}");
        }
    }

    #[test]
    fn a_lock_is_stray_only_when_its_document_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.json"), "{}").unwrap();
        assert!(!lock_target_gone(dir.path(), ".a.lock"));
        assert!(!lock_target_gone(dir.path(), ".a.publish.lock"));
        assert!(lock_target_gone(dir.path(), ".b.lock"));
        assert!(lock_target_gone(dir.path(), ".b.publish.lock"));
        assert!(!lock_target_gone(dir.path(), ".account.json"));
        assert!(!lock_target_gone(dir.path(), "b.lock"));
    }

    #[test]
    fn the_tracker_push_lock_stays_while_the_tracker_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert!(lock_target_gone(dir.path(), ".push.publish.lock"));
        fs::write(dir.path().join("tracker.json"), "{}").unwrap();
        assert!(!lock_target_gone(dir.path(), ".push.publish.lock"));
        assert!(lock_target_gone(dir.path(), ".other.publish.lock"));
    }

    #[test]
    fn a_lock_file_replaced_after_the_open_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".b.lock");
        fs::write(&path, "").unwrap();
        let open = || {
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap()
        };
        let stale = open();
        fs::remove_file(&path).unwrap();
        fs::write(&path, "").unwrap();
        // A child forked by another test can still hold the old file's lock for a moment, which
        // reads as `Busy`; wait for an answer about the replacement.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let kept = loop {
            let outcome = lock_stray_file(stale.try_clone().unwrap(), &path).unwrap();
            if !matches!(outcome, StrayLock::Busy) || std::time::Instant::now() > deadline {
                break outcome;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(matches!(kept, StrayLock::Kept));
        assert!(path.exists());
    }

    #[test]
    fn a_target_that_no_longer_qualifies_is_kept_and_leaves_no_lock_behind() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = base.path().join("repo");
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            store.try_remove_if(&dir, "missing", |_| Ok(true)).unwrap(),
            Removal::Kept
        );
        assert!(!dir.join(".missing.publish.lock").exists());
    }

    #[test]
    fn a_publish_lock_made_before_an_error_is_removed_with_it() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = base.path().join("repo");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("k.json"), "not json").unwrap();
        assert!(try_remove_settled(&store, &dir).is_err());
        assert!(!dir.join(".k.publish.lock").exists());
        // A lock file that was already there and whose target stays is not ours to delete.
        let live = base.path().join("live");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join(".k.publish.lock"), "").unwrap();
        assert!(try_remove_settled(&store, &live).is_ok());
        let present = live.join(".k.publish.lock");
        fs::write(live.join("k.json"), "not json").unwrap();
        assert!(try_remove_settled(&store, &live).is_err());
        assert!(present.exists());
    }

    #[test]
    fn a_held_publish_lock_blocks_removing_its_target() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = base.path().join("repo");
        fs::create_dir_all(&dir).unwrap();
        let target = serde_json::json!({
            "version": 1, "revision": 1, "key": "k", "repo": "r", "kind": "branch",
            "branch": "k", "branch_birth": null, "base": null, "head": null,
            "created_at": "2020-01-01T00:00:00Z", "updated_at": "2020-01-01T00:00:00Z",
            "review": null, "runs": [], "records": [], "threads": [],
        });
        fs::write(dir.join("k.json"), target.to_string()).unwrap();
        let publish = store.publish_lock(&dir, "k").unwrap();
        let busy = store.try_remove_if(&dir, "k", |_| Ok(true)).unwrap();
        assert_eq!(busy, Removal::Busy);
        assert!(dir.join("k.json").exists());
        drop(publish);
        let gone = settled(|| store.try_remove_if(&dir, "k", |_| Ok(true)).unwrap());
        assert!(matches!(gone, Removal::Removed(_)));
        assert!(!dir.join(".k.publish.lock").exists());
        assert!(!dir.join(".k.lock").exists());
    }

    #[test]
    fn a_local_repo_with_anything_else_is_not_scanned_for_removal() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = base.path().join("local/x-1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".account.json"), "{}").unwrap();
        fs::write(dir.join(".gone.lock"), "").unwrap();
        let scan = store.scan_local_repo(&dir, NOW).unwrap().unwrap();
        assert_eq!((scan.extras.len(), scan.stray_locks.len()), (1, 1));
        assert!(!scan.presence.is_gone());
        fs::write(dir.join("notes.txt"), "mine").unwrap();
        assert!(store.scan_local_repo(&dir, NOW).unwrap().is_none());
        assert_eq!(
            store.remove_local_repo(&dir, NOW, false).unwrap(),
            Removal::Kept
        );
        assert!(dir.join(".account.json").exists());
    }

    #[test]
    fn a_busy_lock_keeps_the_whole_local_repo() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = base.path().join("local/x-1");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".account.json"), "{}").unwrap();
        fs::write(dir.join(".a.lock"), "").unwrap();
        fs::write(dir.join(".b.lock"), "").unwrap();
        let held = fs::OpenOptions::new()
            .write(true)
            .open(dir.join(".b.lock"))
            .unwrap();
        held.lock().unwrap();
        assert_eq!(
            store.remove_local_repo(&dir, NOW, true).unwrap(),
            Removal::Busy
        );
        assert!(dir.join(".account.json").exists() && dir.join(".a.lock").exists());
        drop(held);
        assert!(matches!(
            settled(|| store.remove_local_repo(&dir, NOW, true).unwrap()),
            Removal::Removed(_)
        ));
        assert!(!dir.exists());
    }

    #[test]
    fn a_directory_holding_anything_unplanned_is_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let planned = vec![dir.path().join("a.json")];
        assert!(only_holds(dir.path(), &planned).unwrap());
        fs::write(dir.path().join("a.json"), "{}").unwrap();
        assert!(only_holds(dir.path(), &planned).unwrap());
        fs::write(dir.path().join("b.json"), "{}").unwrap();
        assert!(!only_holds(dir.path(), &planned).unwrap());
    }

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;

    fn record(store: &Store, dir: &Path, common_dir: &str, last_seen: Option<i64>) {
        let seen = last_seen.map_or(String::new(), |seen| {
            format!(r#","last_seen":"{}""#, clock::format(seen))
        });
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join(REPO_FILE),
            format!(r#"{{"common_dir":"{common_dir}"{seen}}}"#),
        )
        .unwrap();
        assert!(store.recorded_repo(dir).unwrap().is_some());
    }

    fn presence_of(common_dir: &str, last_seen: Option<i64>) -> Presence {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = base.path().join("local/x-1");
        record(&store, &dir, common_dir, last_seen);
        store.presence(&dir, NOW)
    }

    #[test]
    fn a_repo_that_moved_seconds_ago_is_not_gone() {
        let presence = presence_of("/nowhere/moved/.git", Some(NOW - 5));
        assert!(matches!(presence, Presence::Missing { .. }), "{presence:?}");
        assert!(!presence.is_gone());
        let almost = presence_of("/nowhere/moved/.git", Some(NOW - 30 * DAY + 1));
        assert!(!almost.is_gone());
    }

    #[test]
    fn a_missing_repo_unseen_for_a_month_is_gone() {
        let presence = presence_of("/nowhere/old/.git", Some(NOW - 31 * DAY));
        assert!(presence.is_gone(), "{presence:?}");
        assert!(presence.describe().contains("31 day(s) ago"));
        assert!(presence_of("/nowhere/old/.git", Some(NOW - 30 * DAY)).is_gone());
    }

    #[test]
    fn a_missing_path_under_a_mount_root_is_never_gone() {
        for path in [
            "/Volumes/Work/r/.git",
            "/media/me/Work/r/.git",
            "/run/media/me/Work/r/.git",
            "/mnt/work/r/.git",
        ] {
            let presence = presence_of(path, Some(NOW - 400 * DAY));
            assert!(matches!(presence, Presence::Unmounted(_)), "{path}");
        }
        // Only below the root: a sibling that merely shares the prefix is an ordinary path.
        assert!(presence_of("/Volumesx/r/.git", Some(NOW - 400 * DAY)).is_gone());
    }

    #[test]
    fn a_record_without_last_seen_counts_from_its_modification_time() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = base.path().join("local/x-1");
        record(&store, &dir, "/nowhere/old/.git", None);
        // Written just now.
        assert!(matches!(
            store.presence(&dir, clock::now_secs().unwrap()),
            Presence::Missing { .. }
        ));
        let file = fs::File::options()
            .write(true)
            .open(dir.join(REPO_FILE))
            .unwrap();
        let then = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86_400);
        file.set_modified(then).unwrap();
        assert!(store.presence(&dir, clock::now_secs().unwrap()).is_gone());
    }

    #[test]
    fn an_existing_unrecorded_or_unreadable_record_says_which() {
        let here = tempfile::tempdir().unwrap();
        let alive = presence_of(here.path().to_str().unwrap(), Some(NOW - 400 * DAY));
        assert!(matches!(alive, Presence::Alive(_)));
        assert!(!alive.is_gone());
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = base.path().join("local/x-1");
        assert_eq!(store.presence(&dir, NOW), Presence::NoRecord);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(REPO_FILE), "{ nope").unwrap();
        assert!(matches!(store.presence(&dir, NOW), Presence::Unreadable(_)));
        assert_ne!(alive.describe(), Presence::NoRecord.describe());
    }

    #[test]
    fn last_seen_is_refreshed_at_most_once_a_day() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = base.path().join("local/x-1");
        let seen = || store.recorded_repo(&dir).unwrap().unwrap().last_seen;
        store.record_repo(&dir, "/r/.git", NOW).unwrap();
        assert_eq!(seen().as_deref(), Some(clock::format(NOW).as_str()));
        store.record_repo(&dir, "/r/.git", NOW + DAY - 1).unwrap();
        assert_eq!(seen().as_deref(), Some(clock::format(NOW).as_str()));
        store.record_repo(&dir, "/r/.git", NOW + DAY).unwrap();
        assert_eq!(seen().as_deref(), Some(clock::format(NOW + DAY).as_str()));
        // A record without `last_seen` is upgraded at once, and a new path replaces the old.
        record(&store, &dir, "/r/.git", None);
        store.record_repo(&dir, "/r/.git", NOW).unwrap();
        assert!(seen().is_some());
        store.record_repo(&dir, "/s/.git", NOW).unwrap();
        assert_eq!(
            store.recorded_repo(&dir).unwrap().unwrap().common_dir,
            "/s/.git"
        );
    }

    #[test]
    fn a_future_last_seen_is_refreshed_and_never_described_as_negative_days() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = base.path().join("local/x-1");
        record(&store, &dir, "/nowhere/future/.git", Some(NOW + 40 * DAY));
        let presence = store.presence(&dir, NOW);
        assert!(!presence.is_gone());
        assert!(
            presence.describe().contains(" 0 day(s) ago"),
            "{presence:?}"
        );
        store
            .record_repo(&dir, "/nowhere/future/.git", NOW)
            .unwrap();
        let seen = store.recorded_repo(&dir).unwrap().unwrap().last_seen;
        assert_eq!(seen.as_deref(), Some(clock::format(NOW).as_str()));
    }

    #[test]
    fn a_local_repo_with_no_targets_goes_only_once_idle_for_the_retention() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = base.path().join("local/x-1");
        let alive = base.path().join("alive");
        fs::create_dir_all(&alive).unwrap();
        let common = alive.to_string_lossy().into_owned();
        record(&store, &dir, &common, Some(NOW - 89 * DAY));
        assert_eq!(
            store.remove_local_repo(&dir, NOW, false).unwrap(),
            Removal::Kept
        );
        assert!(dir.exists());
        record(&store, &dir, &common, Some(NOW - 90 * DAY));
        assert!(matches!(
            store.remove_local_repo(&dir, NOW, false).unwrap(),
            Removal::Removed(_)
        ));
        assert!(!dir.exists());
    }
}
