//! A worktree backup is one gzip-compressed tar file. `backup.json` comes first, so listing
//! reads a single entry. Then come the per-worktree git files in `worktree-git/` and the saved
//! files in `files/`. Tar keeps modes, symlinks and byte-exact names.

use std::ffi::OsStr;
use std::fmt::Display;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use tar::{Archive, Builder, Entry, EntryType, Header};

use super::backup::Record;
use super::snapshot::SAVED_GIT_FILES;
use crate::core::error::{Error, Result, ctx, fail};

pub const EXT: &str = ".tar.gz";
const RECORD: &str = "backup.json";
const FILES: &str = "files";
const GIT: &str = "worktree-git";

/// An archive being written under a hidden `.partial` name, which [`Writer::finish`] renames
/// into place.
pub struct Writer {
    tar: Builder<GzEncoder<BufWriter<File>>>,
    partial: PathBuf,
    path: PathBuf,
}

impl Writer {
    /// Starts the archive at `path` with `record` as its first entry.
    pub fn create(path: &Path, record: &Record) -> Result<Self> {
        let json = serde_json::to_vec_pretty(record).map_err(ctx("encode backup record"))?;
        let partial = Self::partial_name(path);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial)
            .map_err(ctx(format!("create {}", partial.display())))?;
        let mut tar = Builder::new(GzEncoder::new(BufWriter::new(file), Compression::default()));
        tar.follow_symlinks(false);
        let mut writer = Self {
            tar,
            partial,
            path: path.to_owned(),
        };
        let mut header = Header::new_gnu();
        header.set_size(json.len() as u64);
        header.set_mode(0o600);
        if let Err(error) = writer.tar.append_data(&mut header, RECORD, json.as_slice()) {
            writer.discard();
            return Err(ctx(format!("write {}", path.display()))(error));
        }
        Ok(writer)
    }

    /// The hidden name an archive at `path` is written under until it is complete.
    pub fn partial_name(path: &Path) -> PathBuf {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        path.with_file_name(format!(".{name}.partial"))
    }

    /// Adds the per-worktree git file `name` (one of `SAVED_GIT_FILES`) from `source`.
    pub fn git_file(&mut self, name: &str, source: &Path) -> Result<()> {
        self.tar
            .append_path_with_name(source, Path::new(GIT).join(name))
            .map_err(ctx(format!("archive {}", source.display())))
    }

    /// Adds the file or symlink at `source` as the worktree path `relative`.
    pub fn file(&mut self, relative: &[u8], source: &Path) -> Result<()> {
        let metadata =
            fs::symlink_metadata(source).map_err(ctx(format!("stat {}", source.display())))?;
        if !metadata.file_type().is_symlink() {
            return self
                .tar
                .append_path_with_name(source, entry_name(relative))
                .map_err(ctx(format!("archive {}", source.display())));
        }
        let target = fs::read_link(source).map_err(ctx(format!("read {}", source.display())))?;
        self.symlink(
            relative,
            metadata.permissions().mode() & 0o7777,
            target.as_os_str().as_bytes(),
        )
    }

    /// Adds a file the worktree does not hold on disk, with `mode` and `contents`.
    pub fn blob(&mut self, relative: &[u8], mode: u32, contents: &[u8]) -> Result<()> {
        let mut header = Header::new_gnu();
        header.set_mode(mode);
        header.set_size(contents.len() as u64);
        let name = entry_name(relative);
        self.tar
            .append_data(&mut header, &name, contents)
            .map_err(ctx(format!("archive {}", name.display())))
    }

    /// Adds a symlink to `target`.
    pub fn symlink(&mut self, relative: &[u8], mode: u32, target: &[u8]) -> Result<()> {
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_mode(mode);
        header.set_size(0);
        let name = entry_name(relative);
        self.tar
            .append_link(&mut header, &name, Path::new(OsStr::from_bytes(target)))
            .map_err(ctx(format!("archive {}", name.display())))
    }

    /// Completes the archive and moves it to its name.
    pub fn finish(self) -> Result<()> {
        let Self { tar, partial, path } = self;
        let done = tar
            .into_inner()
            .and_then(GzEncoder::finish)
            .and_then(|mut out| out.flush())
            .map_err(ctx(format!("write {}", partial.display())))
            .and_then(|()| {
                fs::rename(&partial, &path).map_err(ctx(format!("move to {}", path.display())))
            });
        if done.is_err() {
            let _ = fs::remove_file(&partial);
        }
        done
    }

    /// Drops the unfinished archive.
    pub fn discard(self) {
        drop(self.tar);
        let _ = fs::remove_file(&self.partial);
    }
}

fn entry_name(relative: &[u8]) -> PathBuf {
    Path::new(FILES).join(OsStr::from_bytes(relative))
}

fn reading<E: Display>(path: &Path) -> impl FnOnce(E) -> Error {
    ctx(format!("read {}", path.display()))
}

fn open(path: &Path) -> Result<Archive<GzDecoder<BufReader<File>>>> {
    let file = File::open(path).map_err(reading(path))?;
    Ok(Archive::new(GzDecoder::new(BufReader::new(file))))
}

/// The record in the archive's first entry; the rest of the file is not read.
pub fn read_record(path: &Path) -> Result<Record> {
    let mut archive = open(path)?;
    let mut entries = archive.entries().map_err(reading(path))?;
    let Some(first) = entries.next() else {
        return fail(format!("{} is empty", path.display()));
    };
    let mut first = first.map_err(reading(path))?;
    if first.path_bytes().as_ref() != RECORD.as_bytes() {
        return fail(format!("{} does not start with {RECORD}", path.display()));
    }
    serde_json::from_reader(&mut first).map_err(reading(path))
}

/// Writes the archive's files under `target` and its per-worktree git files under `git_to`.
/// A path that leaves its root or runs through a symlink is an error.
pub fn unpack(path: &Path, target: &Path, git_to: &Path) -> Result<()> {
    let mut archive = open(path)?;
    for entry in archive.entries().map_err(reading(path))? {
        let mut entry = entry.map_err(reading(path))?;
        let name = entry.path_bytes().into_owned();
        if let Some(relative) = name.strip_prefix(format!("{FILES}/").as_bytes()) {
            place(&mut entry, target, relative)?;
        } else if let Some(relative) = name.strip_prefix(format!("{GIT}/").as_bytes())
            && SAVED_GIT_FILES
                .iter()
                .any(|saved| saved.as_bytes() == relative)
        {
            place(&mut entry, git_to, relative)?;
        }
    }
    Ok(())
}

/// Writes `entry` at `relative` below `root`. It creates missing parent directories and replaces
/// any file or symlink there.
fn place<R: Read>(entry: &mut Entry<R>, root: &Path, relative: &[u8]) -> Result<()> {
    let path = inside(root, relative)?;
    let kind = entry.header().entry_type();
    match kind {
        EntryType::Directory => return Ok(()),
        EntryType::Regular | EntryType::Continuous | EntryType::Symlink => {}
        _ => {
            return fail(format!(
                "backup holds an unsupported entry at {}",
                path.display()
            ));
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(ctx(format!("create {}", parent.display())))?;
    }
    match fs::symlink_metadata(&path) {
        // `worktree add` leaves an empty directory where HEAD has a submodule; a file may take its place.
        Ok(found) if found.is_dir() => fs::remove_dir(&path).map_err(ctx(format!(
            "cannot replace the directory {}",
            path.display()
        )))?,
        Ok(_) => fs::remove_file(&path).map_err(ctx(format!("replace {}", path.display())))?,
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(ctx(format!("stat {}", path.display()))(error)),
    }
    entry
        .unpack(&path)
        .map_err(ctx(format!("restore {}", path.display())))?;
    if kind != EntryType::Symlink {
        // Set after unpacking so the process umask cannot change a restored mode.
        let mode = entry.header().mode().map_err(ctx("read file mode"))? & 0o7777;
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))
            .map_err(ctx(format!("restore mode of {}", path.display())))?;
    }
    Ok(())
}

/// Returns `relative` below `root`. It fails when the path leaves `root`, is empty, or runs
/// through a symlink or file.
pub fn inside(root: &Path, relative: &[u8]) -> Result<PathBuf> {
    let relative = Path::new(OsStr::from_bytes(relative));
    let mut path = root.to_owned();
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(part) = part else {
            return fail(format!(
                "backup holds an unsafe path {}",
                relative.display()
            ));
        };
        path.push(part);
        if parts.peek().is_some() && fs::symlink_metadata(&path).is_ok_and(|found| !found.is_dir())
        {
            return fail(format!("cannot restore through {}", path.display()));
        }
    }
    if path == root {
        return fail("backup holds an empty path");
    }
    Ok(path)
}
