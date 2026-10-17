use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result, ctx, fail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::model::{Comment, Forge, Target, Thread, VERSION};

const MAX_KEY: usize = 200;
const ACCOUNT_FILE: &str = ".account.json";

/// The forge login `sync` authenticated as, cached per repository for author defaults.
#[derive(Serialize, Deserialize)]
pub struct Account {
    pub forge: Forge,
    pub host: String,
    pub login: String,
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open() -> Result<Self> {
        let base = match std::env::var_os("XDG_STATE_HOME").map(PathBuf::from) {
            Some(path) if path.is_absolute() => path,
            _ => {
                let home = std::env::var_os("HOME")
                    .filter(|home| !home.is_empty())
                    .ok_or_else(|| Error("HOME is not set".into()))?;
                PathBuf::from(home).join(".local").join("state")
            }
        };
        Ok(Self {
            root: base.join("agents").join("reviews"),
        })
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
        read_target(&dir.join(format!("{key}.json")))
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
            if let Some(target) = read_target(&entry.path())? {
                targets.push(target);
            }
        }
        targets.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(targets)
    }

    /// Every directory under the store root that holds at least one target file.
    pub fn repo_dirs(&self) -> Result<Vec<PathBuf>> {
        let mut found = Vec::new();
        let mut pending = vec![self.root.clone()];
        while let Some(dir) = pending.pop() {
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return fail(format!("read {}: {error}", dir.display())),
            };
            let mut has_target = false;
            for entry in entries {
                let entry = entry.map_err(ctx(format!("read {}", dir.display())))?;
                let kind = entry
                    .file_type()
                    .map_err(ctx(format!("stat {}", entry.path().display())))?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if kind.is_dir() {
                    pending.push(entry.path());
                } else if kind.is_file() && !name.starts_with('.') && name.ends_with(".json") {
                    has_target = true;
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
        let current = read_target(&path)?;
        let expected = current.as_ref().map(|target| target.revision);
        let mut target = current.unwrap_or_else(init);
        let result = change(&mut target)?;
        target.revision = expected.unwrap_or(0) + 1;
        target.updated_at = now.to_owned();
        write_target(dir, &path, &target, expected)?;
        Ok(result)
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
        let old = read_target(&from_path)?;
        let current = read_target(&to_path)?;
        let expected = current.as_ref().map(|target| target.revision);
        let had_old = old.is_some();
        let base_revision = expected.or(old.as_ref().map(|target| target.revision));
        let mut target = match (current, old) {
            (Some(mut current), Some(old)) => {
                merge_targets(&mut current, old);
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
        write_target(dir, &to_path, &target, expected)?;
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

    /// Deletes the target when `remove` approves it, re-reading it under its lock first.
    pub fn remove_if(
        &self,
        dir: &Path,
        key: &str,
        remove: impl FnOnce(&Target) -> Result<bool>,
    ) -> Result<bool> {
        validate_key(key)?;
        let lock_path = dir.join(format!(".{key}.lock"));
        let _lock = lock(&lock_path)?;
        let path = dir.join(format!("{key}.json"));
        let Some(target) = read_target(&path)? else {
            return Ok(false);
        };
        if !remove(&target)? {
            return Ok(false);
        }
        fs::remove_file(&path).map_err(ctx(format!("remove {}", path.display())))?;
        // Waiters on the old lock inode notice the unlink and retry on a fresh file.
        fs::remove_file(&lock_path).map_err(ctx(format!("remove {}", lock_path.display())))?;
        Ok(true)
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
        if let Ok(meta) = fs::symlink_metadata(path)
            && !meta.file_type().is_file()
        {
            return fail(format!("{} must be a regular file", path.display()));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .map_err(ctx(format!("open {}", path.display())))?;
        file.lock()
            .map_err(ctx(format!("lock {}", path.display())))?;
        let held = file
            .metadata()
            .map_err(ctx(format!("stat {}", path.display())))?;
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.ino() == held.ino() && meta.dev() == held.dev() => return Ok(file),
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return fail(format!("stat {}: {error}", path.display())),
        }
    }
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
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
    let mut file = File::open(path).map_err(ctx(format!("open {}", path.display())))?;
    let opened = file
        .metadata()
        .map_err(ctx(format!("stat {}", path.display())))?;
    if opened.ino() != meta.ino() || opened.dev() != meta.dev() {
        return fail(format!("{} changed while opening; retry", path.display()));
    }
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(ctx(format!("read {}", path.display())))?;
    serde_json::from_str(&raw).map_err(ctx(format!("parse {}", path.display())))
}

fn read_target(path: &Path) -> Result<Option<Target>> {
    let target: Option<Target> = read_json(path)?;
    if let Some(target) = &target
        && target.version != VERSION
    {
        return fail(format!(
            "{} has unsupported version {}",
            path.display(),
            target.version
        ));
    }
    Ok(target)
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
    File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(ctx(format!("sync {}", dir.display())))
}

fn write_target(dir: &Path, path: &Path, target: &Target, expected: Option<u64>) -> Result<()> {
    persist_json(dir, path, target, || {
        let on_disk = read_target(path)?.map(|current| current.revision);
        if on_disk != expected {
            return fail(format!(
                "target {} changed during update; retry",
                target.key
            ));
        }
        Ok(())
    })
}

fn create_dir(dir: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(ctx(format!("create {}", dir.display())))
}

/// Moves runs, threads and records that `into` lacks over from `from`.
/// Adds `thread` without losing anything: the same remote thread (or the same finding) gains
/// the comments it lacks; a different thread under a taken id (human `h<n>` ids restart per
/// target) gets the next free id.
fn merge_thread(into: &mut Target, mut thread: Thread) {
    let Some(index) = into.threads.iter().position(|known| known.id == thread.id) else {
        into.threads.push(thread);
        return;
    };
    let known = &mut into.threads[index];
    let same_remote = match (&known.remote, &thread.remote) {
        (Some(a), Some(b)) => a.id == b.id,
        (None, None) => thread.finding.is_some(),
        _ => false,
    };
    if same_remote {
        for comment in thread.comments {
            merge_comment(known, comment);
        }
        return;
    }
    let ids = into.threads.iter().map(|thread| thread.id.as_str());
    let next = ids
        .filter_map(|id| id.strip_prefix('h')?.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    let (old, new) = (format!("{}/", thread.id), format!("h{next}/"));
    for comment in &mut thread.comments {
        if let Some(rest) = comment.id.strip_prefix(&old) {
            comment.id = format!("{new}{rest}");
        }
    }
    thread.id = format!("h{next}");
    into.threads.push(thread);
}

/// Keeps a comment the thread lacks; a local comment id taken by different text is renumbered.
fn merge_comment(thread: &mut Thread, mut comment: Comment) {
    let same = |known: &Comment| match (&known.remote, &comment.remote) {
        (Some(a), Some(b)) => a.id == b.id,
        (None, None) => known.id == comment.id && known.body == comment.body,
        _ => false,
    };
    if thread.comments.iter().any(same) {
        return;
    }
    if thread.comments.iter().any(|known| known.id == comment.id) {
        let prefix = format!("{}/c", thread.id);
        let next = thread
            .comments
            .iter()
            .filter_map(|known| known.id.strip_prefix(&prefix)?.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
            + 1;
        comment.id = format!("{prefix}{next}");
    }
    thread.comments.push(comment);
}

fn merge_targets(into: &mut Target, from: Target) {
    for run in from.runs {
        if !into.runs.iter().any(|known| known.id == run.id) {
            into.runs.push(run);
        }
    }
    for thread in from.threads {
        merge_thread(into, thread);
    }
    for record in from.records {
        if !into
            .records
            .iter()
            .any(|known| known.comment_id == record.comment_id)
        {
            into.records.push(record);
        }
    }
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
        merge_targets(&mut into, from);
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
        merge_targets(&mut into, from);
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
}
