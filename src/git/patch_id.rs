//! `git patch-id --stable` over the patch text libgit2 prints. libgit2's own `Diff::patchid`
//! drops the header of a file that only changes mode, where git hashes it together with the
//! next file's header.

use sha1_smol::Sha1;

use crate::core::error::Result;

const HASH_BYTES: usize = 20;

/// The stable patch-id of `diff`, as `git patch-id --stable` prints it for `git diff`'s output.
pub(super) fn stable(diff: &git2::Diff<'_>) -> Result<String> {
    Ok(sum_of_file_hashes(&patch_text(diff)?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The text `git diff` prints for `diff`.
pub(super) fn patch_text(diff: &git2::Diff<'_>) -> Result<Vec<u8>> {
    let mut text = Vec::new();
    diff.print(git2::DiffFormat::Patch, |_, _, line| {
        if matches!(line.origin(), '+' | '-' | ' ') {
            text.push(line.origin() as u8);
        }
        text.extend_from_slice(line.content());
        true
    })?;
    Ok(text)
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

/// What to do with a line of the patch.
enum Step {
    /// The line is not hashed.
    Skip,
    /// The patch ends here.
    Stop,
    Hash,
}

/// The state of git's `builtin/patch-id.c` while it reads a patch. `before` and `after` count the
/// lines a hunk still has to show, and are -1 while the header of a file is read.
struct Reading<'a> {
    sum: Sum,
    before: i64,
    after: i64,
    in_binary: bool,
    pre_id: &'a [u8],
    post_id: &'a [u8],
}

impl<'a> Reading<'a> {
    fn step(&mut self, line: &'a [u8]) -> Step {
        // The marker after a line without a final newline is not part of the patch.
        if line.starts_with(b"\\ ") && line.len() > 12 {
            return Step::Skip;
        }
        if self.sum.fed == 0 && !line.starts_with(b"diff ") {
            return Step::Skip;
        }
        if self.before == -1
            && let Some(step) = self.in_header(line)
        {
            return step;
        }
        if self.in_binary {
            if line.starts_with(b"diff ") {
                self.in_binary = false;
                self.before = -1;
            }
            return Step::Skip;
        }
        if self.before == 0
            && self.after == 0
            && let Some(step) = self.between_hunks(line)
        {
            return step;
        }
        if matches!(line[0], b'-' | b' ') {
            self.before -= 1;
        }
        if matches!(line[0], b'+' | b' ') {
            self.after -= 1;
        }
        Step::Hash
    }

    /// A line of a file header; `Some` when it is not the start of a hunk's text.
    fn in_header(&mut self, line: &'a [u8]) -> Option<Step> {
        if line.starts_with(b"GIT binary patch") || line.starts_with(b"Binary files") {
            self.in_binary = true;
            self.before = 0;
            self.sum.feed(self.pre_id);
            self.sum.feed(self.post_id);
            self.sum.flush();
            return Some(Step::Skip);
        }
        if let Some(ids) = line.strip_prefix(b"index ") {
            (self.pre_id, self.post_id) = index_ids(ids);
            return Some(Step::Skip);
        }
        if line.starts_with(b"--- ") {
            self.before = 1;
            self.after = 1;
            return None;
        }
        (!line[0].is_ascii_alphabetic()).then_some(Step::Stop)
    }

    /// A line after a hunk has shown all its lines: the next hunk, or the next file.
    fn between_hunks(&mut self, line: &[u8]) -> Option<Step> {
        if line.starts_with(b"@@ -") {
            (self.before, self.after) = hunk_lengths(&line[4..]);
            return Some(Step::Skip);
        }
        if !line.starts_with(b"diff ") {
            return Some(Step::Stop);
        }
        self.sum.flush();
        self.before = -1;
        self.after = -1;
        None
    }
}

/// Ports the line loop of git's `builtin/patch-id.c`.
fn sum_of_file_hashes(text: &[u8]) -> [u8; HASH_BYTES] {
    let mut reading = Reading {
        sum: Sum {
            total: [0; HASH_BYTES],
            current: Sha1::new(),
            fed: 0,
        },
        before: -1,
        after: -1,
        in_binary: false,
        pre_id: b"",
        post_id: b"",
    };
    for line in text.split_inclusive(|byte| *byte == b'\n') {
        match reading.step(line) {
            Step::Skip => {}
            Step::Stop => break,
            Step::Hash => {
                let squeezed: Vec<u8> = line
                    .iter()
                    .copied()
                    .filter(|byte| !byte.is_ascii_whitespace())
                    .collect();
                reading.sum.feed(&squeezed);
            }
        }
    }
    reading.sum.flush();
    reading.sum.total
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
