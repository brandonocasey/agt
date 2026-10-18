//! The local machine and the private files under the user's state directory.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::core::error::{Error, Result, ctx, fail};

/// Creates `path` and its parents with mode 0700 and makes `path` itself 0700.
pub fn private_dir(path: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(ctx(format!("create {}", path.display())))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(ctx(format!("chmod {}", path.display())))
}

/// A lock held as a directory. Dropping it releases the lock.
pub struct Lock(PathBuf);

impl Lock {
    /// Takes the lock, retrying for `wait` when it is held and then failing with `busy`.
    pub fn acquire(path: PathBuf, wait: Option<Duration>, busy: &str) -> Result<Self> {
        let deadline = wait.map(|wait| Instant::now() + wait);
        loop {
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if deadline.is_none_or(|deadline| Instant::now() >= deadline) {
                        return fail(busy);
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(Error(format!("lock {}: {error}", path.display()))),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}

/// Writes `data` to `target` through a private temporary file in the same directory, so a
/// reader sees the old or the new document and never a partial one.
pub fn write_private(target: &Path, temporary: &Path, data: &[u8], sync: bool) -> Result<()> {
    let result = (|| {
        let mut file: File = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(temporary)
            .map_err(ctx(format!("create {}", temporary.display())))?;
        file.write_all(data)
            .map_err(ctx(format!("write {}", temporary.display())))?;
        if sync {
            file.sync_all()
                .map_err(ctx(format!("sync {}", temporary.display())))?;
        }
        drop(file);
        fs::rename(temporary, target).map_err(ctx(format!("rename {}", target.display())))?;
        fs::set_permissions(target, fs::Permissions::from_mode(0o600))
            .map_err(ctx(format!("chmod {}", target.display())))
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
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
