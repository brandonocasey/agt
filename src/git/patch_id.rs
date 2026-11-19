//! `git patch-id --stable` over the patch text libgit2 prints. libgit2's own `Diff::patchid`
//! drops the header of a file that only changes mode, where git hashes it together with the
//! next file's header.

use sha1_smol::Sha1;

use crate::core::error::Result;

const HASH_BYTES: usize = 20;

/// The stable patch-id of `diff`, as `git patch-id --stable` prints it for `git diff`'s output.
pub(super) fn stable(diff: &git2::Diff<'_>) -> Result<String> {
    let mut text = Vec::new();
    diff.print(git2::DiffFormat::Patch, |_, _, line| {
        if matches!(line.origin(), '+' | '-' | ' ') {
            text.push(line.origin() as u8);
        }
        text.extend_from_slice(line.content());
        true
    })?;
    Ok(sum_of_file_hashes(&text)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The hashes of the files seen so far, added byte by byte with carry, and the hash of the
/// file being read.
struct Sum {
    total: [u8; HASH_BYTES],
    current: Sha1,
    /// Bytes fed to `current` since the patch began; zero until the first `diff` line.
    fed: usize,
}

impl Sum {
    fn feed(&mut self, bytes: &[u8]) {
        self.current.update(bytes);
        self.fed += bytes.len();
    }

    fn flush(&mut self) {
        let hash = self.current.digest().bytes();
        self.current = Sha1::new();
        let mut carry = 0u16;
        for (sum, byte) in self.total.iter_mut().zip(hash) {
            carry += u16::from(*sum) + u16::from(byte);
            *sum = carry as u8;
            carry >>= 8;
        }
    }
}

/// Ports the line loop of git's `builtin/patch-id.c`. `before` and `after` count the lines a
/// hunk still has to show, and are -1 while the header of a file is read.
fn sum_of_file_hashes(text: &[u8]) -> [u8; HASH_BYTES] {
    let mut sum = Sum {
        total: [0; HASH_BYTES],
        current: Sha1::new(),
        fed: 0,
    };
    let (mut before, mut after) = (-1i64, -1i64);
    let mut in_binary = false;
    let (mut pre_id, mut post_id): (&[u8], &[u8]) = (b"", b"");
    for line in text.split_inclusive(|byte| *byte == b'\n') {
        // The marker after a line without a final newline is not part of the patch.
        if line.starts_with(b"\\ ") && line.len() > 12 {
            continue;
        }
        if sum.fed == 0 && !line.starts_with(b"diff ") {
            continue;
        }
        if before == -1 {
            if line.starts_with(b"GIT binary patch") || line.starts_with(b"Binary files") {
                in_binary = true;
                before = 0;
                sum.feed(pre_id);
                sum.feed(post_id);
                sum.flush();
                continue;
            } else if let Some(ids) = line.strip_prefix(b"index ") {
                (pre_id, post_id) = index_ids(ids);
                continue;
            } else if line.starts_with(b"--- ") {
                before = 1;
                after = 1;
            } else if !line[0].is_ascii_alphabetic() {
                break;
            }
        }
        if in_binary {
            if line.starts_with(b"diff ") {
                in_binary = false;
                before = -1;
            }
            continue;
        }
        if before == 0 && after == 0 {
            if line.starts_with(b"@@ -") {
                (before, after) = hunk_lengths(&line[4..]);
                continue;
            }
            if !line.starts_with(b"diff ") {
                break;
            }
            sum.flush();
            before = -1;
            after = -1;
        }
        if matches!(line[0], b'-' | b' ') {
            before -= 1;
        }
        if matches!(line[0], b'+' | b' ') {
            after -= 1;
        }
        let squeezed: Vec<u8> = line
            .iter()
            .copied()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        sum.feed(&squeezed);
    }
    sum.flush();
    sum.total
}

/// The two ids of an `index <old>..<new>[ <mode>]` line, without the word `index `.
fn index_ids(rest: &[u8]) -> (&[u8], &[u8]) {
    let rest = rest.strip_suffix(b"\n").unwrap_or(rest);
    let Some(split) = rest.windows(2).position(|pair| pair == b"..") else {
        return (b"", b"");
    };
    let new = &rest[split + 2..];
    let end = new
        .iter()
        .position(|byte| *byte == b' ')
        .unwrap_or(new.len());
    (&rest[..split], &new[..end])
}

/// The old and new line counts of the hunk header that follows `@@ -`; a missing count is 1.
fn hunk_lengths(header: &[u8]) -> (i64, i64) {
    let text = String::from_utf8_lossy(header);
    let mut sides = text.split(' ');
    let count = |side: Option<&str>, sign: &str| {
        side.and_then(|side| side.trim_start_matches(sign).split_once(','))
            .map_or(1, |(_, length)| length.parse().unwrap_or(0))
    };
    let old = count(sides.next(), "-");
    let new = count(sides.next(), "+");
    (old, new)
}
