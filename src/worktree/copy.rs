use std::fs;
use std::io;
use std::path::Path;

pub(super) fn tree(from: &Path, to: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(from)?;
    let kind = metadata.file_type();
    if kind.is_symlink() {
        return std::os::unix::fs::symlink(fs::read_link(from)?, to);
    }
    if kind.is_dir() {
        fs::create_dir(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        return fs::set_permissions(to, metadata.permissions());
    }
    if !kind.is_file() {
        return Err(io::Error::other(format!(
            "cannot copy special file {}",
            from.display()
        )));
    }
    file(from, to)
}

#[cfg(target_os = "linux")]
fn file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let source = fs::File::open(from)?;
    let destination = fs::File::create_new(to)?;
    // FICLONE shares extents, while writes to either file MUST remain independent.
    // https://docs.kernel.org/filesystems/iomap/operations.html#reflink
    let cloned = unsafe { libc::ioctl(destination.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) };
    if cloned == 0 {
        return destination.set_permissions(source.metadata()?.permissions());
    }
    drop(destination);
    fs::copy(from, to).map(|_| ())
}

#[cfg(not(target_os = "linux"))]
fn file(from: &Path, to: &Path) -> io::Result<()> {
    // On macOS std uses fclonefileat, falling back to fcopyfile when cloning is unavailable.
    // https://doc.rust-lang.org/std/fs/fn.copy.html#platform-specific-behavior
    fs::copy(from, to).map(|_| ())
}
