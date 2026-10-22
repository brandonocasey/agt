//! The local machine and the private files under the user's state directory.

use std::ffi::CString;
use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::core::error::{Error, Result, ctx, fail};

/// Creates `path` and the parents it lacks with `mode`; a directory that exists keeps its own.
pub fn create_dirs(path: &Path, mode: u32) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
        .map_err(ctx(format!("create {}", path.display())))
}

/// Creates `path` and its parents with mode 0700 and makes `path` itself 0700.
pub fn private_dir(path: &Path) -> Result<()> {
    create_dirs(path, 0o700)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(ctx(format!("chmod {}", path.display())))
}

/// Replaces `path` with `data` through a private temporary file in the same directory, so a
/// reader sees the old or the new document and never a partial one. `check` runs just before
/// the rename. `durable` also syncs the data and the directory, so the write survives a crash.
pub fn write_atomic(
    path: &Path,
    data: &[u8],
    durable: bool,
    check: impl FnOnce() -> Result<()>,
) -> Result<()> {
    stage(path, data, durable, check, false)
}

/// Durably creates `path` with `data` the same way, failing when `path` already exists.
pub fn write_new(path: &Path, data: &[u8]) -> Result<()> {
    stage(path, data, true, || Ok(()), true)
}

fn stage(
    path: &Path,
    data: &[u8],
    durable: bool,
    check: impl FnOnce() -> Result<()>,
    noclobber: bool,
) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut temp = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempfile_in(dir)
        .map_err(ctx(format!("create temporary file in {}", dir.display())))?;
    temp.write_all(data)
        .map_err(ctx(format!("write {}", temp.path().display())))?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(ctx(format!("chmod {}", temp.path().display())))?;
    if durable {
        temp.as_file()
            .sync_all()
            .map_err(ctx(format!("sync {}", temp.path().display())))?;
    }
    check()?;
    let persisted = if noclobber {
        temp.persist_noclobber(path)
    } else {
        temp.persist(path)
    };
    let verb = if noclobber { "create" } else { "replace" };
    persisted.map_err(|error| Error(format!("{verb} {}: {}", path.display(), error.error)))?;
    if durable {
        sync_dir(dir)?;
    }
    Ok(())
}

/// Flushes the entries of `dir` to disk, so a rename or a new file in it survives a crash.
pub fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|handle| handle.sync_all())
        .map_err(ctx(format!("sync {}", dir.display())))
}

/// The JSON document at `path`, `None` when the file does not exist. It must be a regular
/// file. Writers replace files by rename, so a file
/// swapped between the check and the open is read again; only a path that keeps changing fails.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
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

/// Writes `value` as indented JSON with a final newline, privately and atomically; see
/// [`write_atomic`] for `durable`.
pub fn write_json(path: &Path, value: &impl Serialize, durable: bool) -> Result<()> {
    let mut text =
        serde_json::to_vec_pretty(value).map_err(ctx(format!("serialize {}", path.display())))?;
    text.push(b'\n');
    write_atomic(path, &text, durable, || Ok(()))
}

/// Applies `change` to the JSON document at `path` (the default when missing) under the flock on
/// the sibling `.flock` file, and writes it when `change` returns true; returns that flag.
pub fn update_json<T: DeserializeOwned + Default + Serialize>(
    path: &Path,
    change: impl FnOnce(&mut T) -> Result<bool>,
) -> Result<bool> {
    let _lock = lock_file(&path.with_extension("flock"))?;
    let mut document = read_json(path)?.unwrap_or_default();
    let changed = change(&mut document)?;
    if changed {
        write_json(path, &document, false)?;
    }
    Ok(changed)
}

/// Whether this process may create and delete entries in the directory `path`.
pub fn writable(path: &Path) -> bool {
    CString::new(path.as_os_str().as_encoded_bytes()).is_ok_and(|path| {
        // SAFETY: `access` reads the NUL-terminated path, which `path` keeps alive for the call.
        unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
    })
}

/// The errno name of a failed read, such as `EACCES`, or the error text for other failures.
pub fn error_code(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(libc::EACCES) => "EACCES".into(),
        Some(libc::EPERM) => "EPERM".into(),
        Some(libc::ENOENT) => "ENOENT".into(),
        Some(libc::ENOTDIR) => "ENOTDIR".into(),
        Some(libc::EISDIR) => "EISDIR".into(),
        Some(libc::ELOOP) => "ELOOP".into(),
        _ => error.to_string(),
    }
}

pub fn hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: the pointer and length describe `buffer`, which outlives the call.
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len() - 1) };
    if status != 0 {
        return String::new();
    }
    let length = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..length]).into_owned()
}

pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Error("HOME is not set".into()))
}

/// Takes the exclusive lock on `path`, retrying when the file was replaced while waiting so two
/// processes never hold locks on different inodes for the same target.
pub fn lock_file(path: &Path) -> Result<File> {
    loop {
        let file = open_lock_file(path)?;
        file.lock()
            .map_err(ctx(format!("lock {}", path.display())))?;
        if is_current(&file, path)? {
            return Ok(file);
        }
    }
}

/// Tries the exclusive lock on the open `file` without waiting; `false` when another process
/// holds it. `path` names the file in the error.
pub fn try_lock(file: &File, path: &Path) -> Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => fail(format!("lock {}: {error}", path.display())),
    }
}

/// Like [`lock_file`], but gives up with `None` when another process holds the lock.
pub fn try_lock_file(path: &Path) -> Result<Option<File>> {
    lock_file_within(path, None)
}

/// Like [`try_lock_file`], but polls for `wait` while another process holds the lock, and
/// fails with `busy` when it still does. The lock is released when the file is dropped, also
/// when the process dies.
pub fn lock_file_or_busy(path: &Path, wait: Option<Duration>, busy: &str) -> Result<File> {
    lock_file_within(path, wait)?.ok_or_else(|| Error(busy.to_owned()))
}

/// [`try_lock_file`] polling for `wait` (`None`: one try) while another process holds the lock.
pub fn lock_file_within(path: &Path, wait: Option<Duration>) -> Result<Option<File>> {
    let deadline = wait.map(|wait| Instant::now() + wait);
    loop {
        let file = open_lock_file(path)?;
        if try_lock(&file, path)? {
            if is_current(&file, path)? {
                return Ok(Some(file));
            }
            continue;
        }
        if deadline.is_none_or(|deadline| Instant::now() >= deadline) {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn open_lock_file(path: &Path) -> Result<File> {
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
pub fn is_current(held: &File, path: &Path) -> Result<bool> {
    let held = held
        .metadata()
        .map_err(ctx(format!("stat {}", path.display())))?;
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.ino() == held.ino() && meta.dev() == held.dev()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => fail(format!("stat {}: {error}", path.display())),
    }
}

/// All of stdin as text; `what` names it in the error.
pub fn read_stdin(what: &str) -> Result<String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(ctx(format!("read {what} from stdin")))?;
    Ok(text)
}

/// The text of the file `source`, or of stdin when it is `-`.
pub fn read_source(source: &Path, what: &str) -> Result<String> {
    if source == Path::new("-") {
        read_stdin(what)
    } else {
        fs::read_to_string(source).map_err(ctx(format!("read {}", source.display())))
    }
}

/// `$var` when it is an absolute path, else `~/<fallback>`.
fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf> {
    match std::env::var_os(var).map(PathBuf::from) {
        Some(path) if path.is_absolute() => Ok(path),
        _ => Ok(home()?.join(fallback)),
    }
}

/// `$XDG_STATE_HOME`, or `~/.local/state` when it is unset or relative.
pub fn state_home() -> Result<PathBuf> {
    xdg_dir("XDG_STATE_HOME", ".local/state")
}

/// `$XDG_CONFIG_HOME`, or `~/.config` when it is unset or relative.
pub fn config_home() -> Result<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

/// `name` in agt's config directory, `agt` under [`config_home`].
pub fn agt_config_file(name: &str) -> Result<PathBuf> {
    Ok(config_home()?.join("agt").join(name))
}

/// `$XDG_CACHE_HOME`, or `~/.cache` when it is unset or relative.
pub fn cache_home() -> Result<PathBuf> {
    xdg_dir("XDG_CACHE_HOME", ".cache")
}

/// `agents` under the state directory, which holds every namespace and the domains' state files.
pub fn agents_state() -> Result<PathBuf> {
    Ok(state_home()?.join("agents"))
}

/// `agents/tasks` under the state directory, where task records and the session index live.
pub fn tasks_dir() -> Result<PathBuf> {
    Ok(agents_state()?.join("tasks"))
}

/// `agents/backups` under the state directory, where migrations and worktree removal save copies.
pub fn backups_dir() -> Result<PathBuf> {
    Ok(agents_state()?.join("backups"))
}

/// `agents` under the cache directory, which holds command output and scratch files.
pub fn agents_cache() -> Result<PathBuf> {
    Ok(cache_home()?.join("agents"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    type Map = BTreeMap<String, u16>;

    #[test]
    fn a_held_lock_is_polled_for_then_reported_busy_and_follows_its_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".x.lock");
        let held = try_lock_file(&path).unwrap().unwrap();
        assert!(try_lock_file(&path).unwrap().is_none());
        let started = Instant::now();
        let error = lock_file_or_busy(&path, Some(Duration::from_millis(100)), "busy now");
        assert_eq!(error.unwrap_err().0, "busy now");
        assert!(started.elapsed() >= Duration::from_millis(100));
        drop(held);
        let again = lock_file_within(&path, Some(Duration::from_millis(100))).unwrap();
        assert!(again.is_some());
    }

    #[test]
    fn read_json_refuses_a_symbolic_link_and_reads_through_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.json");
        std::fs::write(&real, "{}").unwrap();
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = read_json::<Map>(&link).unwrap_err();
        assert!(error.0.contains("must be a regular file"), "{}", error.0);
        assert!(
            read_json::<Map>(&dir.path().join("none.json"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn update_json_writes_a_private_file_only_when_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let unchanged = update_json::<Map>(&path, |_| Ok(false)).unwrap();
        assert!(!unchanged && !path.exists());
        let changed = update_json::<Map>(&path, |map| {
            map.insert("a".into(), 1);
            Ok(true)
        })
        .unwrap();
        assert!(changed);
        assert_eq!(read_json::<Map>(&path).unwrap().unwrap()["a"], 1);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(fs::read_to_string(&path).unwrap().ends_with("}\n"));
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(names.len(), 2, "state file and lock file only");
    }

    #[test]
    fn a_failed_check_leaves_the_old_document() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.json");
        write_atomic(&path, b"old", true, || Ok(())).unwrap();
        assert!(write_atomic(&path, b"new", true, || fail("no")).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn write_new_names_the_failure_a_create_and_write_atomic_a_replace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity");
        write_new(&path, b"one").unwrap();
        let error = write_new(&path, b"two").unwrap_err();
        assert!(error.0.starts_with("create "), "{}", error.0);
        assert_eq!(fs::read(&path).unwrap(), b"one");
    }

    #[test]
    fn the_flock_excludes_a_second_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let held = lock_file(&path).unwrap();
        assert!(try_lock_file(&path).unwrap().is_none());
        drop(held);
        assert!(try_lock_file(&path).unwrap().is_some());
    }
}
