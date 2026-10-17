//! What `agt prune` may delete from the store, and the locked, non-blocking ways to delete it, so
//! a prune never waits for or corrupts a concurrent agt run.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use super::{
    ACCOUNT_FILE, Namespace, REPO_FILE, Store, ctx, fail, is_current, read_target_intact, try_lock,
    validate_key,
};
use crate::core::clock;
use crate::core::error::Result;
use crate::store::model::Target;

/// How long a finished thing is kept before `prune` drops it.
pub const PRUNE_AFTER_SECS: i64 = 90 * 24 * 60 * 60;

/// How long a recorded repository must have gone unseen, with its path missing, to be gone.
pub const GONE_AFTER_SECS: i64 = 30 * 24 * 60 * 60;

/// Directories under which removable or network volumes appear. A path missing below one may
/// only be unmounted, so it never marks a repository gone.
const MOUNT_ROOTS: [&str; 4] = ["/Volumes", "/media", "/run/media", "/mnt"];

/// What `.repo.json` says about the repository behind a `local/` directory.
#[derive(Debug, PartialEq, Eq)]
pub enum Presence {
    NoRecord,
    Unreadable(String),
    /// The recorded path exists.
    Alive(String),
    /// The path is missing but agt wrote there recently: the repository was probably moved.
    Missing {
        common_dir: String,
        unseen_secs: i64,
    },
    /// The path is missing below a mount root: the volume may only be unmounted.
    Unmounted(String),
    /// The path is missing and agt has not run there for [`GONE_AFTER_SECS`].
    Gone {
        common_dir: String,
        unseen_secs: i64,
    },
}

impl Presence {
    /// Whether the repository's targets may go at any age.
    pub fn is_gone(&self) -> bool {
        matches!(self, Self::Gone { .. })
    }

    /// Why the repository is, or is not, treated as gone, naming the rule that decided.
    pub fn describe(&self) -> String {
        let days = |secs: &i64| (secs / 86_400).max(0);
        match self {
            Self::NoRecord => "no recorded path, so its existence cannot be checked".into(),
            Self::Unreadable(why) => format!("its path record is unreadable ({why})"),
            Self::Alive(path) => format!("its repository still exists at {path}"),
            Self::Missing {
                common_dir,
                unseen_secs,
            } => format!(
                "{common_dir} is missing but agt last wrote there {} day(s) ago, under the {} days that mark a repository gone, so it may have moved",
                days(unseen_secs),
                GONE_AFTER_SECS / 86_400
            ),
            Self::Unmounted(path) => {
                format!("{path} is missing below a mount root, so its volume may only be unmounted")
            }
            Self::Gone {
                common_dir,
                unseen_secs,
            } => format!(
                "local repository is gone: {common_dir} no longer exists and agt last wrote there {} day(s) ago (path missing and unseen for {}+ days)",
                days(unseen_secs),
                GONE_AFTER_SECS / 86_400
            ),
        }
    }
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

/// A backup directory made by `agt` itself: `<namespace>-v<version>-<stamp>[-<n>]`, or the
/// hidden `.<name>.partial` copy of one that did not finish.
#[derive(Debug, Clone)]
pub struct Backup {
    pub path: PathBuf,
    pub namespace: Namespace,
    pub partial: bool,
    /// When it was taken, Unix seconds, and the `-<n>` suffix that separates two in one second.
    pub taken: (i64, u32),
    pub bytes: u64,
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
        let lock_path = dir.join(format!(".{key}.lock"));
        let Some(_lock) = try_lock(&lock_path)? else {
            return Ok(Removal::Busy);
        };
        // A publish in flight has the target's comments in hand; deleting under it would lose them.
        let publish_path = dir.join(format!(".{key}.publish.lock"));
        let created = !publish_path.exists();
        let Some(file) = try_lock(&publish_path)? else {
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
        let bytes = remove_with_lock(&path, &lock_path)?;
        Ok(Removal::Removed(bytes))
    }

    /// Lock files (`.<key>.lock`, `.<key>.publish.lock`) under the namespace whose document is
    /// gone, so nothing will ever lock them again.
    pub fn stray_locks(&self) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        let mut pending = vec![self.root.clone()];
        while let Some(dir) = pending.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return fail(format!("read {}: {error}", dir.display())),
            };
            for entry in entries {
                let entry = entry.map_err(ctx(format!("read {}", dir.display())))?;
                let kind = entry
                    .file_type()
                    .map_err(ctx(format!("stat {}", entry.path().display())))?;
                if kind.is_dir() {
                    pending.push(entry.path());
                } else if kind.is_file()
                    && lock_target_gone(&dir, &entry.file_name().to_string_lossy())
                {
                    found.push(entry.path());
                }
            }
        }
        found.sort();
        Ok(found)
    }

    /// Deletes a lock file from [`Store::stray_locks`] unless it is held or its document came
    /// back.
    pub fn remove_stray_lock(&self, path: &Path) -> Result<Removal> {
        match self.lock_stray(path)? {
            StrayLock::Held(file) => delete_stray(&file, path),
            StrayLock::Busy => Ok(Removal::Busy),
            StrayLock::Kept => Ok(Removal::Kept),
        }
    }

    /// Takes the lock of a stray lock file without waiting. Opening without `create`, so a lock
    /// that vanished stays vanished.
    pub(super) fn lock_stray(&self, path: &Path) -> Result<StrayLock> {
        match fs::OpenOptions::new().read(true).write(true).open(path) {
            Ok(file) => lock_stray_file(file, path),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(StrayLock::Kept),
            Err(error) => fail(format!("open {}: {error}", path.display())),
        }
    }

    /// What a `local/` repository directory holds, or `None` when it holds anything `prune` may
    /// not delete: a subdirectory, a link, or a file that is not a target, cache or lock.
    pub fn scan_local_repo(&self, dir: &Path, now: i64) -> Result<Option<LocalRepo>> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return fail(format!("read {}: {error}", dir.display())),
        };
        let (mut extras, mut stray_locks, mut documents) = (Vec::new(), Vec::new(), 0);
        for entry in entries {
            let entry = entry.map_err(ctx(format!("read {}", dir.display())))?;
            let kind = entry
                .file_type()
                .map_err(ctx(format!("stat {}", entry.path().display())))?;
            if !kind.is_file() {
                return Ok(None);
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == ACCOUNT_FILE || name == REPO_FILE {
                extras.push(entry.path());
            } else if lock_target_gone(dir, &name) {
                stray_locks.push(entry.path());
            } else if name.starts_with('.') && name.ends_with(".lock") {
                // A target's lock, deleted with the target.
            } else if !name.starts_with('.') && name.ends_with(".json") {
                documents += 1;
            } else {
                return Ok(None);
            }
        }
        let (targets, _) = self.targets_intact(dir)?;
        if targets.len() != documents {
            return Ok(None);
        }
        extras.sort();
        stray_locks.sort();
        Ok(Some(LocalRepo {
            targets,
            extras,
            stray_locks,
            presence: self.presence(dir, now),
        }))
    }

    /// Whether the repository recorded in `dir` still exists, as of `now`.
    pub fn presence(&self, dir: &Path, now: i64) -> Presence {
        let repo = match self.recorded_repo(dir) {
            Ok(Some(repo)) => repo,
            Ok(None) => return Presence::NoRecord,
            Err(error) => return Presence::Unreadable(error.to_string()),
        };
        let common = Path::new(&repo.common_dir);
        if !common.is_absolute() {
            return Presence::Unreadable(format!("{} is not an absolute path", repo.common_dir));
        }
        match common.try_exists() {
            Ok(true) => return Presence::Alive(repo.common_dir),
            Ok(false) => {}
            Err(error) => return Presence::Unreadable(format!("{}: {error}", repo.common_dir)),
        }
        if MOUNT_ROOTS.iter().any(|root| {
            common
                .strip_prefix(root)
                .is_ok_and(|rest| rest.components().next().is_some())
        }) {
            return Presence::Unmounted(repo.common_dir);
        }
        // A record from before `last_seen` existed was last written when it was made.
        let seen = match repo.last_seen.as_deref() {
            Some(seen) => clock::parse(seen).ok(),
            None => seen_by_mtime(&dir.join(REPO_FILE)),
        };
        let Some(seen) = seen else {
            return Presence::Unreadable("no usable last-seen time".into());
        };
        let unseen_secs = now - seen;
        if unseen_secs >= GONE_AFTER_SECS {
            Presence::Gone {
                common_dir: repo.common_dir,
                unseen_secs,
            }
        } else {
            Presence::Missing {
                common_dir: repo.common_dir,
                unseen_secs,
            }
        }
    }

    /// Deletes a `local/` repository directory: its targets when the repository is gone (else
    /// none may be left), then its cache files and stray locks. Every lock is taken before the
    /// first file goes, so a repository that is partly busy loses nothing.
    pub fn remove_local_repo(&self, dir: &Path, now: i64) -> Result<Removal> {
        let Some(repo) = self.scan_local_repo(dir, now)? else {
            return Ok(Removal::Kept);
        };
        if !repo.targets.is_empty() && !repo.presence.is_gone() {
            return Ok(Removal::Kept);
        }
        let mut held = Vec::new();
        let (mut documents, mut locks) = (Vec::new(), Vec::new());
        for target in &repo.targets {
            validate_key(&target.key)?;
            for suffix in ["lock", "publish.lock"] {
                let lock_path = dir.join(format!(".{}.{suffix}", target.key));
                let Some(file) = try_lock(&lock_path)? else {
                    return Ok(Removal::Busy);
                };
                held.push(file);
                locks.push(lock_path);
            }
            documents.push(dir.join(format!("{}.json", target.key)));
        }
        for path in &repo.stray_locks {
            match self.lock_stray(path)? {
                StrayLock::Held(file) => held.push(file),
                StrayLock::Busy => return Ok(Removal::Busy),
                StrayLock::Kept => return Ok(Removal::Kept),
            }
            locks.push(path.clone());
        }
        // Documents first, then locks, then cache files: the order `remove_with_lock` uses, so a
        // reader never finds a lock-less document.
        let doomed: Vec<PathBuf> = documents
            .into_iter()
            .chain(locks)
            .chain(repo.extras)
            .collect();
        // A writer that started after the scan has no lock of ours to wait on; if it left
        // anything we do not know, the repository is in use and nothing is deleted.
        if !only_holds(dir, &doomed)? {
            return Ok(Removal::Busy);
        }
        let mut freed = 0;
        for path in &doomed {
            freed += fs::symlink_metadata(path).map_or(0, |meta| meta.len());
            remove_if_present(path)?;
        }
        match fs::remove_dir(dir) {
            Ok(()) => Ok(Removal::Removed(freed)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Removal::Kept),
            Err(error) if error.kind() == ErrorKind::DirectoryNotEmpty => Ok(Removal::Busy),
            Err(error) => fail(format!("remove {}: {error}", dir.display())),
        }
    }

    /// The backups `agt` made, finished and partial, oldest first. Anything else in the backups
    /// directory belongs to another tool and is not listed.
    pub fn agt_backups(&self) -> Result<Vec<Backup>> {
        let entries = match fs::read_dir(&self.backups) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return fail(format!("read {}: {error}", self.backups.display())),
        };
        let mut found = Vec::new();
        for entry in entries {
            let entry = entry.map_err(ctx(format!("read {}", self.backups.display())))?;
            let kind = entry
                .file_type()
                .map_err(ctx(format!("stat {}", entry.path().display())))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !kind.is_dir() {
                continue;
            }
            let (partial, name) = match name
                .strip_prefix('.')
                .and_then(|name| name.strip_suffix(".partial"))
            {
                Some(inner) => (true, inner),
                None => (false, name.as_str()),
            };
            if let Some((namespace, taken)) = parse_backup_name(name) {
                found.push(Backup {
                    bytes: tree_size(&entry.path()),
                    path: entry.path(),
                    namespace,
                    partial,
                    taken,
                });
            }
        }
        found.sort_by(|a, b| (a.taken, &a.path).cmp(&(b.taken, &b.path)));
        Ok(found)
    }

    /// Deletes a backup unless a backup of its namespace is being made.
    pub fn remove_backup(&self, backup: &Backup) -> Result<Removal> {
        let name = backup.namespace.dir_name();
        let lock_path = self.backups.with_file_name(format!(".{name}-backup.lock"));
        let Some(_lock) = try_lock(&lock_path)? else {
            return Ok(Removal::Busy);
        };
        match fs::remove_dir_all(&backup.path) {
            Ok(()) => Ok(Removal::Removed(backup.bytes)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Removal::Kept),
            Err(error) => fail(format!("remove {}: {error}", backup.path.display())),
        }
    }
}

/// What a `local/` repository directory holds; see [`Store::scan_local_repo`].
pub struct LocalRepo {
    pub targets: Vec<Target>,
    /// Cache files (`.account.json`, `.repo.json`) deleted with the repository.
    pub extras: Vec<PathBuf>,
    /// Lock files whose document is gone.
    pub stray_locks: Vec<PathBuf>,
    pub presence: Presence,
}

/// A stray lock file after trying to take its lock.
pub(super) enum StrayLock {
    Held(fs::File),
    Busy,
    Kept,
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

fn seen_by_mtime(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_secs()).ok()
}

/// Whether every entry of `dir` is one of `paths`.
fn only_holds(dir: &Path, paths: &[PathBuf]) -> Result<bool> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(true),
        Err(error) => return fail(format!("read {}: {error}", dir.display())),
    };
    for entry in entries {
        let entry = entry.map_err(ctx(format!("read {}", dir.display())))?;
        if !paths.contains(&entry.path()) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Locks an opened lock file and checks that it is still the one at `path` and still stray. A
/// lock file replaced since the open is a different inode, and deleting it would break the
/// lock its holder believes it has.
fn lock_stray_file(file: fs::File, path: &Path) -> Result<StrayLock> {
    match file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(StrayLock::Busy),
        Err(fs::TryLockError::Error(error)) => {
            return fail(format!("lock {}: {error}", path.display()));
        }
    }
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Ok(StrayLock::Kept);
    };
    if !is_current(&file, path)? || !lock_target_gone(dir, &name.to_string_lossy()) {
        return Ok(StrayLock::Kept);
    }
    Ok(StrayLock::Held(file))
}

fn delete_stray(file: &fs::File, path: &Path) -> Result<Removal> {
    let bytes = file.metadata().map_or(0, |meta| meta.len());
    fs::remove_file(path).map_err(ctx(format!("remove {}", path.display())))?;
    Ok(Removal::Removed(bytes))
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => fail(format!("remove {}: {error}", path.display())),
    }
}

/// Whether `name` is a lock file in `dir` whose document does not exist.
fn lock_target_gone(dir: &Path, name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix('.')
        .and_then(|name| name.strip_suffix(".lock"))
        .filter(|stem| !stem.is_empty())
    else {
        return false;
    };
    let exists = |stem: &str| dir.join(format!("{stem}.json")).exists();
    // `agt issue push` holds `.push.publish.lock` for the whole tracker, which has no `push`
    // document.
    if stem == "push.publish" && exists("tracker") {
        return false;
    }
    // A target key may itself end in `.publish`, so either reading keeps the lock.
    !exists(stem) && !stem.strip_suffix(".publish").is_some_and(exists)
}

/// `reviews-v1-2026-07-01T10-20-30Z` or the same with `-2`: the namespace and when it was made.
fn parse_backup_name(name: &str) -> Option<(Namespace, (i64, u32))> {
    let namespace = Namespace::ALL
        .into_iter()
        .find(|namespace| name.starts_with(&format!("{}-v", namespace.dir_name())))?;
    let rest = name.strip_prefix(&format!("{}-v", namespace.dir_name()))?;
    let (version, stamp) = rest.split_once('-')?;
    if version.is_empty() || !version.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let time = stamp.get(..20)?;
    let attempt = match stamp.get(20..)? {
        "" => 1,
        suffix => suffix.strip_prefix('-')?.parse().ok().filter(|n| *n >= 2)?,
    };
    if time.as_bytes().get(10) != Some(&b'T') || !time.ends_with('Z') {
        return None;
    }
    // The stamp is the timestamp with its time colons turned into dashes.
    let iso = format!("{}{}", time.get(..11)?, time.get(11..)?.replace('-', ":"));
    Some((namespace, (clock::parse(&iso).ok()?, attempt)))
}

fn tree_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    fs::read_dir(path).map_or(0, |entries| {
        entries
            .flatten()
            .map(|entry| tree_size(&entry.path()))
            .sum()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(store.remove_local_repo(&dir, NOW).unwrap(), Removal::Kept);
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
        assert_eq!(store.remove_local_repo(&dir, NOW).unwrap(), Removal::Busy);
        assert!(dir.join(".account.json").exists() && dir.join(".a.lock").exists());
        drop(held);
        assert!(matches!(
            settled(|| store.remove_local_repo(&dir, NOW).unwrap()),
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
        assert!(presence.describe().contains(" 0 day(s) ago"), "{presence:?}");
        store.record_repo(&dir, "/nowhere/future/.git", NOW).unwrap();
        let seen = store.recorded_repo(&dir).unwrap().unwrap().last_seen;
        assert_eq!(seen.as_deref(), Some(clock::format(NOW).as_str()));
    }
}
