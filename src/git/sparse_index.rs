//! Reading a sparse index, which libgit2 refuses (`unsupported mandatory extension: 'sdir'`):
//! each collapsed directory becomes the files of its tree, marked skip-worktree. Readers get the
//! expanded index in memory; a change libgit2 makes to the index first rewrites the file as the
//! full index.

use sha1_smol::Sha1;

use super::index::{SKIP_WORKTREE, STAGE_MASK};
use crate::core::error::{Error, Result, ctx};

/// The index entry mode of a collapsed directory.
const TREE_MODE: u32 = 0o040_000;
/// Size of the fixed part of an index entry: nine 32-bit stat fields, the mode, the id and the
/// flags.
const ENTRY_FIXED: usize = 62;
const EXTENDED: u16 = 0x4000;
const NAME_MASK: u16 = 0x0fff;
const CHECKSUM: usize = 20;

struct Entry {
    stat: [u8; 36],
    mode: u32,
    id: [u8; 20],
    flags: u16,
    extended: u16,
    path: Vec<u8>,
}

impl Entry {
    fn to_index_entry(&self) -> Result<git2::IndexEntry> {
        let field = |at: usize| be32(&self.stat, at);
        let time =
            |at: usize| Ok::<_, Error>(git2::IndexTime::new(field(at)? as i32, field(at + 4)?));
        Ok(git2::IndexEntry {
            ctime: time(0)?,
            mtime: time(8)?,
            dev: field(16)?,
            ino: field(20)?,
            mode: self.mode,
            uid: field(24)?,
            gid: field(28)?,
            file_size: field(32)?,
            id: git2::Oid::from_bytes(&self.id)?,
            flags: self.flags | if self.extended != 0 { EXTENDED } else { 0 },
            flags_extended: self.extended,
            path: self.path.clone(),
        })
    }
}

/// Whether the index file of `repo` is a sparse index that libgit2 cannot read. A repository
/// without `index.sparse` is not asked, so its index is not read here.
fn is_unreadable_sparse(repo: &git2::Repository) -> Result<bool> {
    let sparse = repo.config()?.get_bool("index.sparse").unwrap_or(false);
    Ok(sparse && repo.index().is_err())
}

/// Gives `repo` the expanded index in memory when its index file is a sparse index. The file is
/// left alone, so libgit2 cannot write the index until [`expand_in_place`] has run.
pub(super) fn expand_if_sparse(repo: &git2::Repository) -> Result<()> {
    if is_unreadable_sparse(repo)? {
        repo.set_index(&mut open_index_file(repo)?)?;
    }
    Ok(())
}

/// The index file of `repo` as a copy that is never written back: a sparse index is expanded in
/// memory, any other index is read as it is.
pub(super) fn open_index_file(repo: &git2::Repository) -> Result<git2::Index> {
    let real = repo.path().join("index");
    let error = match git2::Index::open(&real) {
        Ok(index) => return Ok(index),
        Err(error) => error,
    };
    if !repo.config()?.get_bool("index.sparse").unwrap_or(false) {
        return Err(error.into());
    }
    let bytes = std::fs::read(&real).map_err(ctx(format!("read {}", real.display())))?;
    let mut index = git2::Index::new()?;
    for entry in expanded_entries(repo, &bytes)? {
        index.add(&entry.to_index_entry()?)?;
    }
    Ok(index)
}

/// Rewrites the index file of `repo` as the full index when it is a sparse one, so libgit2 can
/// read it and write changes to it. Git reads the full index as it does a sparse one.
pub(super) fn expand_in_place(repo: &git2::Repository) -> Result<()> {
    if !is_unreadable_sparse(repo)? {
        return Ok(());
    }
    let real = repo.path().join("index");
    let bytes = std::fs::read(&real).map_err(ctx(format!("read {}", real.display())))?;
    let expanded = serialize(&expanded_entries(repo, &bytes)?);
    let next = repo.path().join("index.agt-expanded");
    std::fs::write(&next, expanded).map_err(ctx(format!("write {}", next.display())))?;
    std::fs::rename(&next, &real).map_err(ctx(format!("replace {}", real.display())))?;
    Ok(())
}

/// The entries of the sparse index `bytes`, sorted by path, with each collapsed directory
/// replaced by the files of its tree.
fn expanded_entries(repo: &git2::Repository, bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut entries = parse(bytes)?;
    let mut expanded = Vec::new();
    for entry in entries.drain(..) {
        if entry.mode != TREE_MODE {
            expanded.push(entry);
            continue;
        }
        let tree = repo.find_tree(git2::Oid::from_bytes(&entry.id)?)?;
        let mut files = Vec::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, item| {
            if item.kind() != Some(git2::ObjectType::Tree) {
                let path = [entry.path.as_slice(), dir.as_bytes(), item.name_bytes()].concat();
                files.push((path, item.filemode() as u32, item.id()));
            }
            git2::TreeWalkResult::Ok
        })?;
        for (path, mode, id) in files {
            let mut raw = [0; 20];
            raw.copy_from_slice(id.as_bytes());
            expanded.push(Entry {
                stat: [0; 36],
                mode,
                id: raw,
                flags: path.len().min(usize::from(NAME_MASK)) as u16,
                extended: SKIP_WORKTREE,
                path,
            });
        }
    }
    expanded.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(expanded)
}

fn be32(bytes: &[u8], at: usize) -> Result<u32> {
    let field = bytes
        .get(at..at + 4)
        .ok_or_else(|| Error("the index is cut short".into()))?;
    Ok(u32::from_be_bytes([field[0], field[1], field[2], field[3]]))
}

fn parse(bytes: &[u8]) -> Result<Vec<Entry>> {
    if bytes.get(..4) != Some(b"DIRC") {
        return Err(Error("the index has no DIRC header".into()));
    }
    let version = be32(bytes, 4)?;
    let count = be32(bytes, 8)? as usize;
    let mut at = 12;
    let mut previous: Vec<u8> = Vec::new();
    let mut entries = Vec::new();
    for _ in 0..count {
        let start = at;
        let cut = || Error("the index is cut short".into());
        let fixed = bytes.get(at..at + ENTRY_FIXED).ok_or_else(cut)?;
        let mut stat = [0; 36];
        stat[..24].copy_from_slice(&fixed[..24]);
        stat[24..].copy_from_slice(&fixed[28..40]);
        let mode = be32(fixed, 24)?;
        let mut id = [0; 20];
        id.copy_from_slice(&fixed[40..60]);
        let flags = u16::from_be_bytes([fixed[60], fixed[61]]);
        at += ENTRY_FIXED;
        let mut extended = 0;
        if version >= 3 && flags & EXTENDED != 0 {
            let field = bytes.get(at..at + 2).ok_or_else(cut)?;
            extended = u16::from_be_bytes([field[0], field[1]]);
            at += 2;
        }
        let path = if version >= 4 {
            let (strip, used) = varint(&bytes[at..])?;
            at += used;
            let end = bytes[at..]
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(cut)?;
            let keep = previous.len().checked_sub(strip).ok_or_else(cut)?;
            let path = [&previous[..keep], &bytes[at..at + end]].concat();
            at += end + 1;
            path
        } else {
            let end = bytes[at..]
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(cut)?;
            let path = bytes[at..at + end].to_vec();
            at += end + 1;
            at = start + (at - start).div_ceil(8) * 8;
            path
        };
        previous.clone_from(&path);
        entries.push(Entry {
            stat,
            mode,
            id,
            flags,
            extended,
            path,
        });
    }
    Ok(entries)
}

/// A git offset-encoded integer and the bytes it took.
fn varint(bytes: &[u8]) -> Result<(usize, usize)> {
    let mut value = 0usize;
    for (used, byte) in bytes.iter().enumerate() {
        value = (value << 7) | usize::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Ok((value, used + 1));
        }
        value += 1;
    }
    Err(Error("the index is cut short".into()))
}

/// A version 3 index of `entries`, which are sorted by path.
fn serialize(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"DIRC");
    out.extend_from_slice(&3u32.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for entry in entries {
        let start = out.len();
        out.extend_from_slice(&entry.stat[..24]);
        out.extend_from_slice(&entry.mode.to_be_bytes());
        out.extend_from_slice(&entry.stat[24..]);
        out.extend_from_slice(&entry.id);
        let extended = entry.extended != 0;
        let name = entry.flags & NAME_MASK;
        let stage = entry.flags & STAGE_MASK;
        let flags = name | stage | if extended { EXTENDED } else { 0 };
        out.extend_from_slice(&flags.to_be_bytes());
        if extended {
            out.extend_from_slice(&entry.extended.to_be_bytes());
        }
        out.extend_from_slice(&entry.path);
        out.push(0);
        while (out.len() - start) % 8 != 0 {
            out.push(0);
        }
    }
    let checksum = Sha1::from(&out).digest().bytes();
    debug_assert_eq!(checksum.len(), CHECKSUM);
    out.extend_from_slice(&checksum);
    out
}
