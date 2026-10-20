//! Incremental scanning of one transcript file into the turns the index stores.

use std::fs::File;
use std::os::unix::fs::{FileExt, MetadataExt};

use crate::core::hash::sha256_hex;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::error::{Result, ctx};
use crate::core::transcripts::{ScanState, Turn, parse_line};

pub const READ_CHUNK: usize = 64 * 1024;
pub const MAX_LINE_BYTES: usize = 1024 * 1024;
const PREFIX_BYTES: u64 = 4_096;

/// What the index remembers about one transcript file, enough to continue after the bytes it
/// has already read.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FileState {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub offset: u64,
    pub session_id: Option<String>,
    pub forked_from: Option<String>,
    pub history_start: Option<i64>,
    pub malformed: u64,
    pub oversized: u64,
    pub skip_line: bool,
    pub partial: Option<String>,
    pub prefix: String,
    pub tail_hash: String,
    pub modified: f64,
    pub changed: f64,
    pub turns: Vec<Turn>,
}

/// A file's identity and times as the index records them (milliseconds as a float).
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub modified: f64,
    pub changed: f64,
}

impl Stat {
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        let millis =
            |seconds: i64, nanos: i64| seconds as f64 * 1000.0 + nanos as f64 / 1_000_000.0;
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.len(),
            modified: millis(metadata.mtime(), metadata.mtime_nsec()),
            changed: millis(metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

/// Reads up to `length` bytes at `position`, fewer only at the end of the file.
fn read_at(file: &File, position: u64, length: usize) -> Result<Vec<u8>> {
    let mut buffer = vec![0; length];
    let mut filled = 0;
    while filled < length {
        let count = file
            .read_at(&mut buffer[filled..], position + filled as u64)
            .map_err(ctx("read transcript"))?;
        if count == 0 {
            break;
        }
        filled += count;
    }
    buffer.truncate(filled);
    Ok(buffer)
}

fn file_prefix(file: &File, size: u64) -> Result<String> {
    let length = usize::try_from(size.min(PREFIX_BYTES)).unwrap_or_default();
    Ok(sha256_hex(&read_at(file, 0, length)?))
}

fn tail_hash(file: &File, end: u64) -> Result<String> {
    let length = end.min(READ_CHUNK as u64);
    Ok(sha256_hex(&read_at(
        file,
        end - length,
        usize::try_from(length).unwrap_or_default(),
    )?))
}

/// Whether `previous` describes the start of `stat`'s file, so scanning can continue from its
/// offset instead of starting over.
fn can_append(file: &File, stat: &Stat, previous: Option<&FileState>) -> Result<bool> {
    let Some(previous) = previous else {
        return Ok(false);
    };
    Ok(previous.dev == stat.dev
        && previous.ino == stat.ino
        && stat.size >= previous.size
        && previous.prefix == file_prefix(file, previous.size)?
        && previous.tail_hash == tail_hash(file, previous.offset)?
        && (stat.size > previous.size
            || (stat.modified == previous.modified && stat.changed == previous.changed)))
}

pub struct Scanned {
    pub state: FileState,
    pub bytes: u64,
}

/// Scans `path` from where `previous` stopped, reading at most `budget` bytes.
pub fn scan_file(
    path: &str,
    stat: &Stat,
    previous: Option<&FileState>,
    budget: u64,
) -> Result<Scanned> {
    let file = File::open(path).map_err(ctx(format!("open {path}")))?;
    let prefix = file_prefix(&file, stat.size)?;
    let append = can_append(&file, stat, previous)?;
    let (mut state, start_offset, mut pending, mut skipping) = match previous.filter(|_| append) {
        Some(previous) => (
            ScanState {
                turns: previous.turns.clone(),
                session_id: previous.session_id.clone(),
                forked_from: previous.forked_from.clone(),
                history_start: previous.history_start,
                malformed: previous.malformed,
                oversized: previous.oversized,
            },
            previous.offset,
            previous
                .partial
                .as_deref()
                .and_then(|partial| STANDARD.decode(partial).ok())
                .unwrap_or_default(),
            previous.skip_line,
        ),
        None => (ScanState::default(), 0, Vec::new(), false),
    };
    let mut read_bytes: u64 = 0;
    while start_offset + read_bytes < stat.size && read_bytes < budget {
        let length = (READ_CHUNK as u64)
            .min(stat.size - start_offset - read_bytes)
            .min(budget - read_bytes);
        let chunk = read_at(
            &file,
            start_offset + read_bytes,
            usize::try_from(length).unwrap_or_default(),
        )?;
        if chunk.is_empty() {
            break;
        }
        read_bytes += chunk.len() as u64;
        let mut start = 0;
        for position in 0..chunk.len() {
            if chunk[position] != b'\n' {
                continue;
            }
            let fragment = &chunk[start..position];
            let line_offset = start_offset + read_bytes - chunk.len() as u64 + start as u64
                - pending.len() as u64;
            if skipping {
                skipping = false;
            } else if pending.is_empty() {
                parse_line(fragment, &mut state, path, line_offset);
            } else {
                pending.extend_from_slice(fragment);
                parse_line(&pending, &mut state, path, line_offset);
            }
            pending.clear();
            start = position + 1;
        }
        if start < chunk.len() && !skipping {
            let fragment = &chunk[start..];
            if pending.len() + fragment.len() > MAX_LINE_BYTES {
                state.oversized += 1;
                pending.clear();
                skipping = true;
            } else {
                pending.extend_from_slice(fragment);
            }
        }
    }
    let offset = start_offset + read_bytes;
    let result = FileState {
        dev: stat.dev,
        ino: stat.ino,
        size: stat.size,
        offset,
        session_id: state.session_id,
        forked_from: state.forked_from,
        history_start: state.history_start,
        malformed: state.malformed,
        oversized: state.oversized,
        skip_line: skipping,
        partial: (!pending.is_empty()).then(|| STANDARD.encode(&pending)),
        prefix,
        tail_hash: tail_hash(&file, offset)?,
        modified: stat.modified,
        changed: stat.changed,
        turns: state.turns,
    };
    Ok(Scanned {
        state: result,
        bytes: read_bytes,
    })
}

/// The turns of `state` plus the final line when it is complete JSON without a newline.
pub fn visible_turns(file: &str, state: &FileState) -> Vec<Turn> {
    let mut turns = state.turns.clone();
    let Some(partial) = state.partial.as_deref() else {
        return turns;
    };
    if state.skip_line || state.offset != state.size {
        return turns;
    }
    let Ok(pending) = STANDARD.decode(partial) else {
        return turns;
    };
    if serde_json::from_str::<Value>(&String::from_utf8_lossy(&pending)).is_err() {
        return turns;
    }
    let mut view = ScanState {
        turns: Vec::new(),
        session_id: state.session_id.clone(),
        forked_from: state.forked_from.clone(),
        history_start: state.history_start,
        malformed: state.malformed,
        oversized: state.oversized,
    };
    parse_line(
        &pending,
        &mut view,
        file,
        state.offset - pending.len() as u64,
    );
    for mut turn in view.turns {
        turn.line_bytes = pending.len() as u64;
        turns.push(turn);
    }
    turns
}

#[cfg(test)]
mod tests {
    /// Index timestamps must survive a JSON round trip exactly, or every unchanged session
    /// file looks changed and is scanned again (serde_json's `float_roundtrip`).
    #[test]
    fn index_timestamps_round_trip_exactly() {
        let ms = 1_791_315_986_406.988_5_f64;
        let text = serde_json::to_string(&ms).unwrap();
        assert_eq!(serde_json::from_str::<f64>(&text).unwrap(), ms);
        assert_eq!(
            serde_json::from_str::<f64>("1791315986406.9885").unwrap(),
            ms
        );
    }
}
