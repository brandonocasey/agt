use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result, ctx, fail};
use crate::model::{Target, VERSION};

const MAX_KEY: usize = 200;

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
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(ctx(format!("create {}", dir.display())))?;
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

fn read_target(path: &Path) -> Result<Option<Target>> {
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
    let target: Target =
        serde_json::from_str(&raw).map_err(ctx(format!("parse {}", path.display())))?;
    if target.version != VERSION {
        return fail(format!(
            "{} has unsupported version {}",
            path.display(),
            target.version
        ));
    }
    Ok(Some(target))
}

fn write_target(dir: &Path, path: &Path, target: &Target, expected: Option<u64>) -> Result<()> {
    let mut temp = tempfile::Builder::new()
        .prefix(".tmp-")
        .suffix(".json")
        .tempfile_in(dir)
        .map_err(ctx(format!("create temporary file in {}", dir.display())))?;
    serde_json::to_writer_pretty(&mut temp, target).map_err(ctx("serialize target"))?;
    temp.write_all(b"\n")
        .map_err(ctx(format!("write {}", temp.path().display())))?;
    temp.as_file()
        .set_permissions(Permissions::from_mode(0o600))
        .map_err(ctx(format!("chmod {}", temp.path().display())))?;
    temp.as_file()
        .sync_all()
        .map_err(ctx(format!("sync {}", temp.path().display())))?;
    let on_disk = read_target(path)?.map(|current| current.revision);
    if on_disk != expected {
        return fail(format!(
            "target {} changed during update; retry",
            target.key
        ));
    }
    temp.persist(path)
        .map_err(|error| Error(format!("replace {}: {}", path.display(), error.error)))?;
    File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(ctx(format!("sync {}", dir.display())))
}
