mod docs;
pub mod issues;
pub mod model;
pub mod prune;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions, TryLockError};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::core::error::{Error, Result, ctx, fail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::clock;
use crate::store::model::{Forge, Target, VERSION};

const MAX_KEY: usize = 200;
const _: () = assert!(REVIEW_MIGRATIONS.len() + 1 == VERSION as usize);
const TARGET_KINDS: [&str; 3] = ["branch", "range", "commit"];
const ACCOUNT_FILE: &str = ".account.json";
const REPO_FILE: &str = ".repo.json";

/// Files left out of a read, each as its key and the warning that says why.
pub type LeftOut = Vec<(String, String)>;

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

/// A store area with its own directory and schema version, so a change to one cannot break
/// another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Namespace {
    Reviews,
    Issues,
    Tasks,
}

/// The current schema version of a namespace and the steps that bring older documents to it.
#[derive(Clone, Copy)]
struct Schema {
    version: u32,
    migrations: &'static [fn(&mut Value)],
}

impl Namespace {
    pub const ALL: [Self; 3] = [Self::Reviews, Self::Issues, Self::Tasks];

    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Reviews => "reviews",
            Self::Issues => "issues",
            Self::Tasks => "tasks",
        }
    }

    fn schema(self) -> Schema {
        match self {
            Self::Reviews => Schema {
                version: VERSION,
                migrations: REVIEW_MIGRATIONS,
            },
            Self::Issues | Self::Tasks => Schema {
                version: 1,
                migrations: &[],
            },
        }
    }
}

pub struct Store {
    root: PathBuf,
    backups: PathBuf,
    namespace: Namespace,
    schema: Schema,
    backed_up: AtomicBool,
}

impl Store {
    pub fn open(namespace: Namespace) -> Result<Self> {
        let base = match std::env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
            Some(path) if path.is_absolute() => path,
            _ => {
                let home = std::env::var_os("HOME")
                    .filter(|home| !home.is_empty())
                    .ok_or_else(|| Error("HOME is not set".into()))?;
                PathBuf::from(home).join(".local").join("state")
            }
        };
        Ok(Self::at(&base, namespace))
    }

    /// The namespace directory, for domains whose documents are not review targets.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Opens `namespace` under the state directory `base` (the parent of `agents`).
    fn at(base: &Path, namespace: Namespace) -> Self {
        let agents = base.join("agents");
        Self {
            root: agents.join(namespace.dir_name()),
            backups: agents.join("backups"),
            namespace,
            schema: namespace.schema(),
            backed_up: AtomicBool::new(false),
        }
    }

    /// Copies the namespace to `backups/<namespace>-v<old version>-<timestamp>/` before the first
    /// overwrite of a document written under an older schema version. One backup per namespace
    /// and old version, across processes: a later run that finds one does not copy again.
    fn backup_before_migration(&self, path: &Path) -> Result<()> {
        if self.backed_up.load(Ordering::Acquire) {
            return Ok(());
        }
        let stored = read_document(path)?
            .and_then(|document| document.get("version").and_then(Value::as_u64));
        let Some(stored) = stored.filter(|version| *version < u64::from(self.schema.version))
        else {
            return Ok(());
        };
        let name = self.namespace.dir_name();
        let prefix = format!("{name}-v{stored}-");
        create_dir(&self.backups)?;
        // Beside the backups, not in them, so the backups directory holds only backups.
        let lock_path = self.backups.with_file_name(format!(".{name}-backup.lock"));
        // One backup at a time per namespace, across processes: the check, the copy and the
        // rename are one step, so no run deletes or renames another's copy.
        let _guard = lock(&lock_path)?;
        let done = fs::read_dir(&self.backups).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        });
        if done {
            self.backed_up.store(true, Ordering::Release);
            return Ok(());
        }
        // Copies that failed part way are no backup; under the lock none is in progress.
        let leftover = format!(".{prefix}");
        for entry in fs::read_dir(&self.backups)
            .map_err(ctx(format!("read {}", self.backups.display())))?
            .flatten()
        {
            let file = entry.file_name().to_string_lossy().into_owned();
            if file.starts_with(&leftover) && file.ends_with(".partial") {
                fs::remove_dir_all(entry.path())
                    .map_err(ctx(format!("remove {}", entry.path().display())))?;
            }
        }
        let stamp = crate::core::clock::now()?.replace(':', "-");
        let mut target = self.backups.join(format!("{prefix}{stamp}"));
        let mut attempt = 1;
        while target.exists() {
            attempt += 1;
            target = self.backups.join(format!("{prefix}{stamp}-{attempt}"));
        }
        // Copy under a hidden name and rename when complete, so a failed copy never looks
        // like a finished backup to the next run.
        let partial = self.backups.join(format!(
            ".{}.partial",
            target.file_name().unwrap_or_default().to_string_lossy()
        ));
        copy_tree(&self.root, &partial)?;
        fs::rename(&partial, &target).map_err(ctx(format!(
            "rename {} to {}",
            partial.display(),
            target.display()
        )))?;
        sync_dir(&self.backups)?;
        self.backed_up.store(true, Ordering::Release);
        Ok(())
    }

    fn write(&self, dir: &Path, path: &Path, target: &Target, expected: Option<u64>) -> Result<()> {
        self.backup_before_migration(path)?;
        write_target(dir, path, target, expected, self.schema)
    }

    pub fn repo_dir(&self, repo_key: &str) -> Result<PathBuf> {
        let mut dir = self.root.clone();
        for part in repo_key.split('/') {
            let part: String = part
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            if part.is_empty() || part.chars().all(|c| c == '.') {
                return fail(format!("invalid repository key: {repo_key}"));
            }
            dir.push(part);
        }
        Ok(dir)
    }

    pub fn relative(&self, dir: &Path) -> String {
        dir.strip_prefix(&self.root)
            .unwrap_or(dir)
            .to_string_lossy()
            .into_owned()
    }

    pub fn load(&self, dir: &Path, key: &str) -> Result<Option<Target>> {
        validate_key(key)?;
        read_target(&dir.join(format!("{key}.json")), self.schema)
    }

    pub fn targets(&self, dir: &Path) -> Result<Vec<Target>> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return fail(format!("read {}: {error}", dir.display())),
        };
        let mut targets = Vec::new();
        for entry in entries {
            let entry = entry.map_err(ctx(format!("read {}", dir.display())))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            let is_dir = entry
                .file_type()
                .map_err(ctx(format!("stat {}", entry.path().display())))?
                .is_dir();
            if is_dir {
                continue;
            }
            let path = entry.path();
            let Some(document) = read_document(&path)? else {
                continue;
            };
            if is_target_document(&document) {
                targets.push(parse_target(&path, document, self.schema)?.0);
            }
        }
        targets.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(targets)
    }

    /// The targets in `dir` that can be rewritten without losing anything, plus a warning for
    /// each file left out: one that cannot be read or parsed, or that holds fields this agt does
    /// not know. One bad file does not hide the others.
    pub fn targets_intact(&self, dir: &Path) -> Result<(Vec<Target>, Vec<String>)> {
        let (targets, left_out) = self.targets_intact_by_key(dir)?;
        Ok((
            targets,
            left_out.into_iter().map(|(_, warning)| warning).collect(),
        ))
    }

    /// Like [`Store::targets_intact`], with each file left out as its key and warning.
    pub fn targets_intact_by_key(&self, dir: &Path) -> Result<(Vec<Target>, LeftOut)> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok((Vec::new(), Vec::new()));
            }
            Err(error) => return fail(format!("read {}: {error}", dir.display())),
        };
        let (mut targets, mut warnings) = (Vec::new(), Vec::new());
        for entry in entries {
            let entry = entry.map_err(ctx(format!("read {}", dir.display())))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if name.starts_with('.') || !name.ends_with(".json") || is_dir {
                continue;
            }
            let path = entry.path();
            let key = name.trim_end_matches(".json").to_owned();
            match read_target_intact(&path, self.schema) {
                Ok(Some(Ok(target))) => targets.push(target),
                Ok(Some(Err(field))) => warnings.push((
                    key,
                    format!(
                        "{} has field {field} that this agt does not know; kept",
                        path.display()
                    ),
                )),
                Ok(None) => {}
                Err(error) => warnings.push((key, format!("{error}; kept"))),
            }
        }
        targets.sort_by(|a, b| a.key.cmp(&b.key));
        Ok((targets, warnings))
    }

    /// Every directory under the store root that holds at least one target file. A directory or file that cannot be read is skipped with a
    /// warning instead of ending the search.
    pub fn repo_dirs_lenient(&self) -> (Vec<PathBuf>, Vec<String>) {
        let mut warnings = Vec::new();
        let found = self.find_repo_dirs(Some(&mut warnings)).unwrap_or_default();
        (found, warnings)
    }

    /// With `warnings`, a problem is recorded and the search goes on; without, it ends the search.
    fn find_repo_dirs(&self, mut warnings: Option<&mut Vec<String>>) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        let mut pending = vec![self.root.clone()];
        let note = |warnings: &mut Option<&mut Vec<String>>, message: String| match warnings
            .as_deref_mut()
        {
            Some(warnings) => {
                warnings.push(message);
                Ok(())
            }
            None => fail(message),
        };
        while let Some(dir) = pending.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    note(&mut warnings, format!("read {}: {error}", dir.display()))?;
                    continue;
                }
            };
            let mut has_target = false;
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        note(&mut warnings, format!("read {}: {error}", dir.display()))?;
                        continue;
                    }
                };
                let path = entry.path();
                let kind = match entry.file_type() {
                    Ok(kind) => kind,
                    Err(error) => {
                        note(&mut warnings, format!("stat {}: {error}", path.display()))?;
                        continue;
                    }
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                if kind.is_dir() {
                    pending.push(path);
                } else if kind.is_file() && !name.starts_with('.') && name.ends_with(".json") {
                    match is_target_file(&path) {
                        Ok(is_target) => has_target |= is_target,
                        // Lenient: the caller reads the directory and warns about this file once.
                        Err(_) if warnings.is_some() => has_target = true,
                        Err(error) => return Err(error),
                    }
                }
            }
            if has_target {
                found.push(dir);
            }
        }
        found.sort();
        Ok(found)
    }

    /// Uses a separate lock so publishing can still take the target's store-update lock.
    pub fn publish_lock(&self, dir: &Path, key: &str) -> Result<File> {
        validate_key(key)?;
        create_dir(dir)?;
        lock(&dir.join(format!(".{key}.publish.lock")))
    }

    /// Runs `change` on the target under its lock and writes the result atomically. A missing
    /// target starts from `init`.
    pub fn update<R>(
        &self,
        dir: &Path,
        key: &str,
        now: &str,
        init: impl FnOnce() -> Target,
        change: impl FnOnce(&mut Target) -> Result<R>,
    ) -> Result<R> {
        validate_key(key)?;
        create_dir(dir)?;
        let _lock = lock(&dir.join(format!(".{key}.lock")))?;
        let path = dir.join(format!("{key}.json"));
        let current = read_target_for_write(&path, self.schema)?;
        let expected = current.as_ref().map(|target| target.revision);
        let mut target = current.unwrap_or_else(init);
        let result = change(&mut target)?;
        target.revision = expected.unwrap_or(0) + 1;
        target.updated_at = now.to_owned();
        self.write(dir, &path, &target, expected)?;
        Ok(result)
    }

    /// Like [`Store::update`] for the target `earlier` was read as, but fails when it is gone, so
    /// a change planned from an earlier read never recreates a target deleted since.
    pub fn update_existing<R>(
        &self,
        dir: &Path,
        earlier: &Target,
        now: &str,
        change: impl FnOnce(&mut Target) -> Result<R>,
    ) -> Result<R> {
        let key = earlier.key.as_str();
        let missing = std::cell::Cell::new(false);
        let init = || {
            missing.set(true);
            earlier.clone()
        };
        self.update(dir, key, now, init, |target| {
            if missing.get() {
                return fail(format!("target {key} vanished"));
            }
            change(target)
        })
    }

    /// Like [`Store::update`], but stores the result under `to` and deletes `from`. Both locks are
    /// held, taken in key order so two renames cannot deadlock. When both files exist, `from`'s
    /// runs, threads and records that `to` lacks are merged in.
    pub fn update_renamed<R>(
        &self,
        dir: &Path,
        from: &str,
        to: &str,
        now: &str,
        init: impl FnOnce() -> Target,
        change: impl FnOnce(&mut Target) -> Result<R>,
    ) -> Result<R> {
        if from == to {
            return self.update(dir, to, now, init, change);
        }
        validate_key(from)?;
        validate_key(to)?;
        create_dir(dir)?;
        let from_lock = dir.join(format!(".{from}.lock"));
        let to_lock = dir.join(format!(".{to}.lock"));
        let (first, second) = if from < to {
            (&from_lock, &to_lock)
        } else {
            (&to_lock, &from_lock)
        };
        let _first = lock(first)?;
        let _second = lock(second)?;
        let from_path = dir.join(format!("{from}.json"));
        let to_path = dir.join(format!("{to}.json"));
        let old = read_target_for_write(&from_path, self.schema)?;
        let current = read_target_for_write(&to_path, self.schema)?;
        let expected = current.as_ref().map(|target| target.revision);
        let had_old = old.is_some();
        let base_revision = expected.or(old.as_ref().map(|target| target.revision));
        let mut target = match (current, old) {
            (Some(mut current), Some(old)) => {
                current.absorb(old);
                current
            }
            (Some(current), None) => current,
            (None, Some(old)) => old,
            (None, None) => init(),
        };
        target.key = to.to_owned();
        let result = change(&mut target)?;
        target.revision = base_revision.unwrap_or(0) + 1;
        target.updated_at = now.to_owned();
        self.write(dir, &to_path, &target, expected)?;
        if had_old {
            fs::remove_file(&from_path).map_err(ctx(format!("remove {}", from_path.display())))?;
            fs::remove_file(&from_lock).map_err(ctx(format!("remove {}", from_lock.display())))?;
        }
        Ok(result)
    }

    pub fn load_account(&self, dir: &Path) -> Result<Option<Account>> {
        read_json(&dir.join(ACCOUNT_FILE))
    }

    pub fn save_account(&self, dir: &Path, account: &Account) -> Result<()> {
        create_dir(dir)?;
        persist_json(dir, &dir.join(ACCOUNT_FILE), account, || Ok(()))
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
        persist_json(dir, &path, &file, || Ok(()))
    }

    /// The git common dir recorded by [`Store::record_repo`], if any.
    pub fn recorded_repo(&self, dir: &Path) -> Result<Option<RepoFile>> {
        read_json(&dir.join(REPO_FILE))
    }
}

fn validate_key(key: &str) -> Result<()> {
    let valid = !key.is_empty()
        && key.len() <= MAX_KEY
        && !key.starts_with('.')
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '%'));
    if valid {
        Ok(())
    } else {
        fail(format!("invalid target key: {key}"))
    }
}

/// Takes the exclusive lock on `path`, retrying when the file was replaced while waiting so two
/// processes never hold locks on different inodes for the same target.
fn lock(path: &Path) -> Result<File> {
    loop {
        let file = open_lock(path)?;
        file.lock()
            .map_err(ctx(format!("lock {}", path.display())))?;
        if is_current(&file, path)? {
            return Ok(file);
        }
    }
}

/// Like [`lock`], but gives up with `None` when another process holds the lock.
fn try_lock(path: &Path) -> Result<Option<File>> {
    loop {
        let file = open_lock(path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(error)) => {
                return fail(format!("lock {}: {error}", path.display()));
            }
        }
        if is_current(&file, path)? {
            return Ok(Some(file));
        }
    }
}

fn open_lock(path: &Path) -> Result<File> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && !meta.file_type().is_file()
    {
        return fail(format!("{} must be a regular file", path.display()));
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(ctx(format!("open {}", path.display())))
}

/// Whether `path` still names the file `held`, so a lock on it excludes every other holder.
fn is_current(held: &File, path: &Path) -> Result<bool> {
    let held = held
        .metadata()
        .map_err(ctx(format!("stat {}", path.display())))?;
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.ino() == held.ino() && meta.dev() == held.dev()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => fail(format!("stat {}: {error}", path.display())),
    }
}

/// Reads a JSON file that must be a regular file. Writers replace files by rename, so a file
/// swapped between the check and the open is read again; only a path that keeps changing fails.
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    const ATTEMPTS: usize = 100;
    for _ in 0..ATTEMPTS {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return fail(format!("stat {}: {error}", path.display())),
        };
        if !meta.file_type().is_file() {
            return fail(format!(
                "{} must be a regular file, not a symbolic link",
                path.display()
            ));
        }
        let mut file = match File::open(path) {
            Ok(file) => file,
            // Replaced by a rename of a file that is gone again, or removed.
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return fail(format!("open {}: {error}", path.display())),
        };
        let opened = file
            .metadata()
            .map_err(ctx(format!("stat {}", path.display())))?;
        if opened.ino() != meta.ino() || opened.dev() != meta.dev() {
            continue;
        }
        let mut raw = String::new();
        file.read_to_string(&mut raw)
            .map_err(ctx(format!("read {}", path.display())))?;
        return serde_json::from_str(&raw).map_err(ctx(format!("parse {}", path.display())));
    }
    fail(format!("{} changed while opening; retry", path.display()))
}

/// Whether the store file holds a target. Other documents may share a repository directory; a
/// file whose `kind` is not a target kind belongs to them and is left alone.
fn is_target_document(document: &Value) -> bool {
    match document.get("kind").and_then(Value::as_str) {
        Some(kind) => TARGET_KINDS.contains(&kind),
        None => true,
    }
}

fn is_target_file(path: &Path) -> Result<bool> {
    Ok(read_document(path)?.is_some_and(|document| is_target_document(&document)))
}

/// Reads the target a caller asked for by key. A file of another document kind is an error so
/// that a write never replaces it.
fn read_target(path: &Path, schema: Schema) -> Result<Option<Target>> {
    match read_document(path)? {
        Some(document) => parse_target(path, document, schema).map(|(target, _)| Some(target)),
        None => Ok(None),
    }
}

/// Like [`read_target`], but the inner `Err` names the first field this build does not know, for
/// a caller that must keep such a file as it is.
fn read_target_intact(
    path: &Path,
    schema: Schema,
) -> Result<Option<std::result::Result<Target, String>>> {
    let Some(document) = read_document(path)? else {
        return Ok(None);
    };
    if !is_target_document(&document) {
        return Ok(None);
    }
    let (target, document) = parse_target(path, document, schema)?;
    let written =
        serde_json::to_value(&target).map_err(ctx(format!("encode {}", path.display())))?;
    Ok(Some(match lost_field(&document, &written, "") {
        Some(field) => Err(field),
        None => Ok(target),
    }))
}

/// Like [`read_target`], for a caller that will write the target back. Refuses a file holding
/// fields this build does not know, because writing it would silently drop them.
fn read_target_for_write(path: &Path, schema: Schema) -> Result<Option<Target>> {
    let Some(document) = read_document(path)? else {
        return Ok(None);
    };
    let (target, document) = parse_target(path, document, schema)?;
    let written =
        serde_json::to_value(&target).map_err(ctx(format!("encode {}", path.display())))?;
    if let Some(field) = lost_field(&document, &written, "") {
        return fail(format!(
            "{} has field {field} that this agt does not know; it was probably written by a newer agt, so upgrade agt before changing it",
            path.display()
        ));
    }
    Ok(Some(target))
}

fn read_document(path: &Path) -> Result<Option<Value>> {
    read_json(path)
}

/// The first field of `original` that `written` lacks, as a path.
fn lost_field(original: &Value, written: &Value, at: &str) -> Option<String> {
    match (original, written) {
        (Value::Object(original), Value::Object(written)) => {
            original.iter().find_map(|(name, value)| {
                let path = if at.is_empty() {
                    name.clone()
                } else {
                    format!("{at}.{name}")
                };
                match written.get(name) {
                    Some(written) => lost_field(value, written, &path),
                    None => Some(path),
                }
            })
        }
        (Value::Array(original), Value::Array(written)) => original
            .iter()
            .zip(written)
            .enumerate()
            .find_map(|(index, (original, written))| {
                lost_field(original, written, &format!("{at}[{index}]"))
            }),
        _ => None,
    }
}

/// Parses a stored target, migrating older schema versions first. Returns the migrated
/// document alongside the target.
fn parse_target(path: &Path, mut document: Value, schema: Schema) -> Result<(Target, Value)> {
    if !is_target_document(&document) {
        return fail(format!("{} is not a target document", path.display()));
    }
    let version = document.get("version").and_then(Value::as_u64);
    let version = match version.map(u32::try_from) {
        Some(Ok(version)) => version,
        _ => return fail(format!("{} has no valid schema version", path.display())),
    };
    if version > schema.version {
        return fail(format!(
            "{} uses schema version {version}, newer than the {} this agt (v{}) supports; upgrade agt to a release that supports schema version {version}",
            path.display(),
            schema.version,
            env!("CARGO_PKG_VERSION"),
        ));
    }
    if version == 0 {
        return fail(format!("{} has unsupported version 0", path.display()));
    }
    migrate_with(&mut document, version, schema.migrations);
    let target: Target = serde_json::from_value(document.clone())
        .map_err(ctx(format!("parse {}", path.display())))?;
    Ok((target, document))
}

/// One step per schema version: `MIGRATIONS[n - 1]` rewrites a version `n` document into
/// version `n + 1`'s shape. Bumping [`VERSION`] means adding a step here and a frozen-JSON test.
const REVIEW_MIGRATIONS: &[fn(&mut Value)] = &[];

fn migrate_with(document: &mut Value, from: u32, steps: &[fn(&mut Value)]) {
    let from = from as usize;
    for step in steps.iter().skip(from - 1) {
        step(document);
    }
    let to = steps.len() as u32 + 1;
    if let Some(version) = document.get_mut("version") {
        *version = Value::from(to);
    }
}

/// Writes `value` to a temporary file in `dir`, fsyncs it, runs `check` and renames it over
/// `path`.
fn persist_json(
    dir: &Path,
    path: &Path,
    value: &impl Serialize,
    check: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let mut temp = tempfile::Builder::new()
        .prefix(".tmp-")
        .suffix(".json")
        .tempfile_in(dir)
        .map_err(ctx(format!("create temporary file in {}", dir.display())))?;
    serde_json::to_writer_pretty(&mut temp, value)
        .map_err(ctx(format!("serialize {}", path.display())))?;
    temp.write_all(b"\n")
        .map_err(ctx(format!("write {}", temp.path().display())))?;
    temp.as_file()
        .set_permissions(Permissions::from_mode(0o600))
        .map_err(ctx(format!("chmod {}", temp.path().display())))?;
    temp.as_file()
        .sync_all()
        .map_err(ctx(format!("sync {}", temp.path().display())))?;
    check()?;
    temp.persist(path)
        .map_err(|error| Error(format!("replace {}: {}", path.display(), error.error)))?;
    sync_dir(dir)
}

fn write_target(
    dir: &Path,
    path: &Path,
    target: &Target,
    expected: Option<u64>,
    schema: Schema,
) -> Result<()> {
    persist_json(dir, path, target, || {
        let on_disk = read_target(path, schema)?.map(|current| current.revision);
        if on_disk != expected {
            return fail(format!(
                "target {} changed during update; retry",
                target.key
            ));
        }
        Ok(())
    })
}

/// Copies regular files and directories from `from` into the new directory `to`, skipping lock
/// files and anything that is not a plain file or directory.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    create_dir(to)?;
    let entries = match fs::read_dir(from) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return fail(format!("read {}: {error}", from.display())),
    };
    for entry in entries {
        let entry = entry.map_err(ctx(format!("read {}", from.display())))?;
        let kind = entry
            .file_type()
            .map_err(ctx(format!("stat {}", entry.path().display())))?;
        let destination = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &destination)?;
        } else if kind.is_file() && !entry.file_name().to_string_lossy().ends_with(".lock") {
            fs::copy(entry.path(), &destination)
                .map_err(ctx(format!("copy {}", entry.path().display())))?;
            File::open(&destination)
                .and_then(|copied| copied.sync_all())
                .map_err(ctx(format!("sync {}", destination.display())))?;
        }
    }
    sync_dir(to)
}

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(ctx(format!("sync {}", dir.display())))
}

fn create_dir(dir: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(ctx(format!("create {}", dir.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn comment(id: &str, body: &str, remote: Option<&str>) -> Value {
        json!({
            "id": id, "author": { "kind": "human", "login": "me" }, "body": body,
            "created_at": "2026-10-16T12:00:00Z",
            "post": if remote.is_some() { "posted" } else { "pending" },
            "remote": remote.map(|id| json!({
                "id": id, "url": null, "login": "alice", "is_me": false, "editor": null,
                "review_id": null,
            })),
            "draft_id": null, "attempt": null, "deleted_remote": false, "deleted_seen_at": null,
        })
    }

    fn thread(id: &str, remote: Option<&str>, comments: Vec<Value>) -> Value {
        json!({
            "id": id, "type": "comment", "severity": null, "anchor": null, "resolved": false,
            "resolution": null, "triage": null, "held": false, "finding": null,
            "comments": comments,
            "remote": remote.map(|id| json!({
                "kind": "general", "id": id, "url": null, "resolvable": false, "outdated": null,
                "path": null, "old_path": null, "line": null, "old_line": null,
                "original_line": null, "start_line": null, "original_start_line": null,
                "side": null, "commit": null, "review_state": null,
            })),
            "resolve_pending": null, "deleted_remote": false, "deleted_seen_at": null,
            "created_at": "2026-10-16T12:00:00Z", "updated_at": "2026-10-16T12:00:00Z",
        })
    }

    fn target(threads: Vec<Value>) -> Target {
        serde_json::from_value(json!({
            "version": VERSION, "revision": 1, "key": "k", "repo": "r", "kind": "branch",
            "branch": "feat", "branch_birth": null, "base": null, "head": null,
            "created_at": "2026-10-16T12:00:00Z", "updated_at": "2026-10-16T12:00:00Z",
            "review": null, "runs": [], "records": [], "threads": threads,
        }))
        .unwrap()
    }

    fn bodies(target: &Target) -> Vec<(String, Vec<String>)> {
        target
            .threads
            .iter()
            .map(|thread| {
                (
                    thread.id.clone(),
                    thread
                        .comments
                        .iter()
                        .map(|comment| comment.body.clone())
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn update_existing_refuses_a_deleted_target() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path(), Namespace::Reviews);
        let kept = target(Vec::new());
        let result = store.update_existing(dir.path(), &kept, "2026-10-16T12:00:00Z", |_| Ok(()));
        assert!(result.unwrap_err().to_string().contains("vanished"));
        assert!(!dir.path().join("k.json").exists());
        store
            .update(
                dir.path(),
                "k",
                "2026-10-16T12:00:00Z",
                || kept.clone(),
                |_| Ok(()),
            )
            .unwrap();
        store
            .update_existing(dir.path(), &kept, "2026-10-16T12:00:01Z", |target| {
                target.branch = Some("other".into());
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store
                .load(dir.path(), "k")
                .unwrap()
                .unwrap()
                .branch
                .as_deref(),
            Some("other")
        );
    }

    #[test]
    fn merge_renumbers_colliding_human_threads() {
        let mut into = target(vec![thread(
            "h1",
            None,
            vec![comment("h1/c1", "Into note", None)],
        )]);
        let from = target(vec![thread(
            "h1",
            None,
            vec![comment("h1/c1", "From note", None)],
        )]);
        into.absorb(from);
        assert_eq!(
            bodies(&into),
            [
                ("h1".to_owned(), vec!["Into note".to_owned()]),
                ("h2".to_owned(), vec!["From note".to_owned()]),
            ]
        );
        assert_eq!(into.threads[1].comments[0].id, "h2/c1");
    }

    #[test]
    fn merge_keeps_pending_replies_on_the_same_remote_thread() {
        let imported = || comment("remote-900", "Alice text", Some("900"));
        let mut into = target(vec![thread(
            "gh-c900",
            Some("900"),
            vec![imported(), comment("gh-c900/c1", "Into reply", None)],
        )]);
        let from = target(vec![thread(
            "gh-c900",
            Some("900"),
            vec![imported(), comment("gh-c900/c1", "From reply", None)],
        )]);
        into.absorb(from);
        assert_eq!(
            bodies(&into),
            [(
                "gh-c900".to_owned(),
                vec![
                    "Alice text".to_owned(),
                    "Into reply".to_owned(),
                    "From reply".to_owned()
                ]
            )]
        );
        assert_eq!(into.threads[0].comments[2].id, "gh-c900/c2");
    }

    fn stored(version: u64) -> Value {
        json!({
            "version": version, "revision": 1, "key": "k", "repo": "r", "kind": "branch",
            "branch": null, "branch_birth": null, "base": null, "head": null,
            "created_at": "t", "updated_at": "t", "review": null,
            "runs": [], "records": [], "threads": [],
        })
    }

    fn file(name: &str, document: &Value) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        fs::write(&path, document.to_string()).unwrap();
        (dir, path)
    }

    #[test]
    fn current_version_loads() {
        let (_dir, path) = file("k.json", &stored(VERSION.into()));
        assert!(
            read_target(&path, Namespace::Reviews.schema())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn newer_version_is_refused_with_an_upgrade_hint() {
        let (_dir, path) = file("k.json", &stored(u64::from(VERSION) + 1));
        let error = read_target(&path, Namespace::Reviews.schema())
            .err()
            .unwrap()
            .0;
        assert!(error.contains("newer"), "{error}");
        assert!(error.contains("upgrade agt"), "{error}");
        assert!(
            error.contains(&format!("version {}", VERSION + 1)),
            "{error}"
        );
    }

    #[test]
    fn version_zero_is_refused() {
        let (_dir, path) = file("k.json", &stored(0));
        assert!(read_target(&path, Namespace::Reviews.schema()).is_err());
    }

    #[test]
    fn migrations_run_from_the_stored_version() {
        fn rename(document: &mut Value) {
            document["renamed"] = json!(true);
        }
        fn twice(document: &mut Value) {
            document["twice"] = json!(true);
        }
        let steps: &[fn(&mut Value)] = &[rename, twice];
        let mut document = json!({"version": 1});
        migrate_with(&mut document, 1, steps);
        assert_eq!(
            document,
            json!({"version": 3, "renamed": true, "twice": true})
        );
        let mut document = json!({"version": 2});
        migrate_with(&mut document, 2, steps);
        assert_eq!(document, json!({"version": 3, "twice": true}));
    }

    #[test]
    fn writing_refuses_to_drop_unknown_fields() {
        let mut document = stored(VERSION.into());
        document["threads"] = json!([]);
        document["from_the_future"] = json!(1);
        let (_dir, path) = file("k.json", &document);
        assert!(
            read_target(&path, Namespace::Reviews.schema())
                .unwrap()
                .is_some()
        );
        let error = read_target_for_write(&path, Namespace::Reviews.schema())
            .err()
            .unwrap()
            .0;
        assert!(error.contains("from_the_future"), "{error}");
        let mut document = stored(VERSION.into());
        document["runs"] = json!([]);
        let (_dir, path) = file("k.json", &document);
        assert!(
            read_target_for_write(&path, Namespace::Reviews.schema())
                .unwrap()
                .is_some()
        );
    }

    const NOW: &str = "2026-10-16T12:00:00Z";

    fn write_target_named(store: &Store, dir: &Path) {
        store
            .update(dir, "k", NOW, || target(Vec::new()), |_| Ok(()))
            .unwrap();
    }

    #[test]
    fn namespaces_use_separate_directories_and_do_not_see_each_other() {
        let base = tempfile::tempdir().unwrap();
        let reviews = Store::at(base.path(), Namespace::Reviews);
        let issues = Store::at(base.path(), Namespace::Issues);
        let tasks = Store::at(base.path(), Namespace::Tasks);
        let dir = reviews.repo_dir("github.com/o/r").unwrap();
        write_target_named(&reviews, &dir);
        assert!(dir.starts_with(base.path().join("agents").join("reviews")));
        assert_eq!(reviews.repo_dirs_lenient().0, vec![dir.clone()]);
        assert!(issues.repo_dirs_lenient().0.is_empty());
        assert!(tasks.repo_dirs_lenient().0.is_empty());
        let issue_dir = issues.repo_dir("github.com/o/r").unwrap();
        assert!(issue_dir.starts_with(base.path().join("agents").join("issues")));
        assert!(issues.load(&issue_dir, "k").unwrap().is_none());
        assert!(
            tasks
                .repo_dir("x")
                .unwrap()
                .starts_with(base.path().join("agents").join("tasks"))
        );
    }

    #[test]
    fn each_namespace_refuses_a_newer_schema_version() {
        for namespace in [Namespace::Reviews, Namespace::Issues, Namespace::Tasks] {
            let base = tempfile::tempdir().unwrap();
            let store = Store::at(base.path(), namespace);
            let dir = store.repo_dir("o/r").unwrap();
            let newer = stored(u64::from(namespace.schema().version) + 1);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("k.json"), newer.to_string()).unwrap();
            let error = store.load(&dir, "k").err().unwrap().0;
            assert!(error.contains("upgrade agt"), "{namespace:?}: {error}");
        }
    }

    #[test]
    fn schema_versions_are_independent_per_namespace() {
        let base = tempfile::tempdir().unwrap();
        let mut reviews = Store::at(base.path(), Namespace::Reviews);
        let issues = Store::at(base.path(), Namespace::Issues);
        reviews.schema.version = 2;
        let dir = issues.repo_dir("o/r").unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("k.json"), stored(2).to_string()).unwrap();
        assert!(issues.load(&dir, "k").is_err());
        let dir = reviews.repo_dir("o/r").unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("k.json"), stored(2).to_string()).unwrap();
        reviews.schema.migrations = &[|_| {}];
        assert!(reviews.load(&dir, "k").unwrap().is_some());
    }

    #[test]
    fn migrating_write_backs_up_the_namespace_first_and_only_once() {
        let base = tempfile::tempdir().unwrap();
        let mut store = Store::at(base.path(), Namespace::Reviews);
        let dir = store.repo_dir("o/r").unwrap();
        write_target_named(&store, &dir);
        let backups = base.path().join("agents").join("backups");
        assert!(!backups.exists(), "a current-version write needs no backup");
        let before = fs::read_to_string(dir.join("k.json")).unwrap();

        store.schema = Schema {
            version: 2,
            migrations: &[|_| {}],
        };
        write_target_named(&store, &dir);
        let made: Vec<_> = fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(made.len(), 1);
        let name = made[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("reviews-"), "{name}");
        let copy = made[0].join("o").join("r").join("k.json");
        assert_eq!(fs::read_to_string(copy).unwrap(), before);
        let after = fs::read_to_string(dir.join("k.json")).unwrap();
        assert_ne!(
            after, before,
            "the live file was rewritten after the backup"
        );

        write_target_named(&store, &dir);
        assert_eq!(fs::read_dir(&backups).unwrap().count(), 1);
    }

    #[test]
    fn a_later_process_finds_the_backup_of_that_version_and_skips_it() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = store.repo_dir("o/r").unwrap();
        write_target_named(&store, &dir);
        fs::write(
            dir.join("old.json"),
            fs::read_to_string(dir.join("k.json")).unwrap(),
        )
        .unwrap();
        let backups = base.path().join("agents").join("backups");
        let v2 = Schema {
            version: 2,
            migrations: &[|_| {}],
        };
        // Two processes, each with its own store, migrate different files from version 1.
        for key in ["k", "old"] {
            let mut next = Store::at(base.path(), Namespace::Reviews);
            next.schema = v2;
            next.update(&dir, key, NOW, || target(Vec::new()), |_| Ok(()))
                .unwrap();
        }
        let made: Vec<String> = fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(made.len(), 1, "{made:?}");
        assert!(made[0].starts_with("reviews-v1-"), "{made:?}");
    }

    #[test]
    fn a_partial_backup_is_not_taken_for_a_finished_one() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = store.repo_dir("o/r").unwrap();
        write_target_named(&store, &dir);
        let backups = base.path().join("agents").join("backups");
        // What a copy that failed part way leaves behind.
        fs::create_dir_all(backups.join(".reviews-v1-2026-01-01T00-00-00Z.partial")).unwrap();
        let mut next = Store::at(base.path(), Namespace::Reviews);
        next.schema = Schema {
            version: 2,
            migrations: &[|_| {}],
        };
        write_target_named(&next, &dir);
        let finished: Vec<String> = fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("reviews-v1-"))
            .collect();
        assert_eq!(finished.len(), 1, "{finished:?}");
        assert!(
            !backups
                .join(".reviews-v1-2026-01-01T00-00-00Z.partial")
                .exists(),
            "the stale partial copy is removed"
        );
    }

    #[test]
    fn a_migration_waiting_on_the_backup_lock_rechecks_for_the_backup_made_meanwhile() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let dir = store.repo_dir("o/r").unwrap();
        write_target_named(&store, &dir);
        let agents = base.path().join("agents");
        let backups = agents.join("backups");
        fs::create_dir_all(&backups).unwrap();
        let guard = lock(&agents.join(".reviews-backup.lock")).unwrap();
        std::thread::scope(|scope| {
            let migrating = scope.spawn(|| {
                let mut next = Store::at(base.path(), Namespace::Reviews);
                next.schema = Schema {
                    version: 2,
                    migrations: &[|_| {}],
                };
                write_target_named(&next, &dir);
            });
            // The other process finishes its backup while this one waits on the lock.
            std::thread::sleep(std::time::Duration::from_millis(200));
            fs::create_dir(backups.join("reviews-v1-x")).unwrap();
            drop(guard);
            migrating.join().unwrap();
        });
        let names: Vec<String> = fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["reviews-v1-x".to_string()], "{names:?}");
    }

    #[test]
    fn concurrent_migrations_leave_one_whole_backup() {
        let base = tempfile::tempdir().unwrap();
        let store = Store::at(base.path(), Namespace::Reviews);
        let keys: Vec<String> = (0..6).map(|i| format!("k{i}")).collect();
        let dir = store.repo_dir("o/r").unwrap();
        for key in &keys {
            store
                .update(&dir, key, NOW, || target(Vec::new()), |_| Ok(()))
                .unwrap();
        }
        let barrier = std::sync::Barrier::new(keys.len());
        std::thread::scope(|scope| {
            for key in &keys {
                let (barrier, dir, root) = (&barrier, &dir, base.path());
                scope.spawn(move || {
                    let mut next = Store::at(root, Namespace::Reviews);
                    next.schema = Schema {
                        version: 2,
                        migrations: &[|_| {}],
                    };
                    barrier.wait();
                    next.update(dir, key, NOW, || target(Vec::new()), |_| Ok(()))
                        .unwrap();
                });
            }
        });
        let names: Vec<String> = fs::read_dir(base.path().join("agents").join("backups"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].starts_with("reviews-v1-"), "{names:?}");
        assert!(!names[0].ends_with(".partial"), "{names:?}");
    }
}

#[cfg(test)]
mod read_race_tests {
    use super::*;

    #[test]
    fn a_reader_survives_the_file_being_replaced_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.json");
        persist_json(dir.path(), &path, &Value::from(0), || Ok(())).unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer = {
            let (dir, path, stop) = (dir.path().to_owned(), path.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut n = 0;
                while !stop.load(Ordering::Relaxed) {
                    n += 1;
                    persist_json(&dir, &path, &Value::from(n), || Ok(())).unwrap();
                }
            })
        };
        for _ in 0..3000 {
            read_json::<Value>(&path).unwrap().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }
}
