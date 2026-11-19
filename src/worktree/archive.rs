//! A worktree backup as one gzip-compressed tar file: `backup.json` first, so listing reads a
//! single entry, then the per-worktree git files under `worktree-git/`, then each saved file
//! under `files/`. Tar keeps modes, symlinks and byte-exact names.

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
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let partial = path.with_file_name(format!(".{name}.partial"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&partial)
            .map_err(ctx(format!("create {}", partial.display())))?;
        let mut tar = Builder::new(GzEncoder::new(BufWriter::new(file), Compression::best()));
        tar.follow_symlinks(false);
        let mut writer = Self {
            tar,
            partial,
            path: path.to_owned(),
        };
        let json = serde_json::to_vec_pretty(record).map_err(ctx("encode backup record"))?;
        let mut header = Header::new_gnu();
        header.set_size(json.len() as u64);
        header.set_mode(0o600);
        if let Err(error) = writer.tar.append_data(&mut header, RECORD, json.as_slice()) {
            writer.discard();
            return Err(ctx(format!("write {}", path.display()))(error));
        }
        Ok(writer)
    }

    /// Adds the per-worktree git file `name` (one of `SAVED_GIT_FILES`) from `source`.
    pub fn git_file(&mut self, name: &str, source: &Path) -> Result<()> {
        self.tar
            .append_path_with_name(source, Path::new(GIT).join(name))
            .map_err(ctx(format!("archive {}", source.display())))
    }

    /// Adds the file or symlink at `source` as the worktree path `relative`.
    pub fn file(&mut self, relative: &[u8], source: &Path) -> Result<()> {
        self.tar
            .append_path_with_name(source, entry_name(relative))
            .map_err(ctx(format!("archive {}", source.display())))
    }

    /// Adds a file the worktree does not hold on disk: `contents` with `mode`, or a symlink to
    /// `contents` when `link`.
    pub fn blob(&mut self, relative: &[u8], mode: u32, link: bool, contents: &[u8]) -> Result<()> {
        let mut header = Header::new_gnu();
        header.set_mode(mode);
        let name = entry_name(relative);
        let added = if link {
            header.set_entry_type(EntryType::Symlink);
            header.set_size(0);
            self.tar
                .append_link(&mut header, &name, Path::new(OsStr::from_bytes(contents)))
        } else {
            header.set_size(contents.len() as u64);
            self.tar.append_data(&mut header, &name, contents)
        };
        added.map_err(ctx(format!("archive {}", name.display())))
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

/// Writes `entry` at `relative` below `root`, creating missing parent directories and replacing
/// a file or symlink already there.
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
        Ok(found) if found.is_dir() => return fail(format!("{} is a directory", path.display())),
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

/// `relative` below `root`, when it names something inside it and no existing parent is a
/// symlink or file.
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
