use crate::store::model::{Anchor, Side};

pub struct FileDiff {
    old: Option<String>,
    new: Option<String>,
    hunks: Vec<Hunk>,
}

impl FileDiff {
    /// Whether the diff changes this path, on either side of a rename.
    pub fn touches(&self, path: &str) -> bool {
        self.old.as_deref() == Some(path) || self.new.as_deref() == Some(path)
    }

    /// Whether the diff changes this path and leaves a file there.
    pub fn changes_into(&self, path: &str) -> bool {
        self.new.as_deref() == Some(path)
    }
}

struct Hunk {
    old_start: u32,
    old_len: u32,
    new_start: u32,
    new_len: u32,
}

pub struct Location {
    pub path: String,
    pub line: Option<u32>,
    pub outdated: bool,
}

/// Parses `git diff -U0 -M` output into per-file hunks; `None` paths mean `/dev/null`.
pub fn parse(diff: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut in_hunk = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = header_path(rest);
            files.push(FileDiff {
                old: path.clone(),
                new: path,
                hunks: Vec::new(),
            });
            in_hunk = false;
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if let Some(rest) = line.strip_prefix("@@ ") {
            file.hunks.extend(parse_hunk(rest));
            in_hunk = true;
            continue;
        }
        // Content lines such as a removed "-- a/x" would look like headers.
        if in_hunk {
            continue;
        }
        if let Some(path) = line.strip_prefix("rename from ") {
            file.old = Some(path.to_owned());
        } else if let Some(path) = line.strip_prefix("rename to ") {
            file.new = Some(path.to_owned());
        } else if line == "--- /dev/null" || line.starts_with("new file mode") {
            file.old = None;
        } else if line == "+++ /dev/null" || line.starts_with("deleted file mode") {
            file.new = None;
        } else if let Some(path) = line.strip_prefix("--- a/") {
            file.old = Some(header_name(path));
        } else if let Some(path) = line.strip_prefix("+++ b/") {
            file.new = Some(header_name(path));
        }
    }
    files
}

/// Git ends a `---`/`+++` header with a tab when the path contains a space.
fn header_name(path: &str) -> String {
    path.strip_suffix('\t').unwrap_or(path).to_owned()
}

fn header_path(rest: &str) -> Option<String> {
    let half = rest.len().checked_sub(5)? / 2;
    let old = rest.strip_prefix("a/")?.get(..half)?;
    let new = rest.get(2 + half..)?.strip_prefix(" b/")?;
    (old == new).then(|| old.to_owned())
}

fn parse_hunk(rest: &str) -> Option<Hunk> {
    let mut parts = rest.split_whitespace();
    let (old_start, old_len) = range(parts.next()?.strip_prefix('-')?)?;
    let (new_start, new_len) = range(parts.next()?.strip_prefix('+')?)?;
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
    })
}

fn range(text: &str) -> Option<(u32, u32)> {
    match text.split_once(',') {
        Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
        None => Some((text.parse().ok()?, 1)),
    }
}

/// Maps an anchor through a diff from its commit or tree to the current head. A new-side line
/// inside a changed hunk is outdated; an old-side anchor is outdated once its file changes.
pub fn locate(files: &[FileDiff], anchor: &Anchor) -> Location {
    let Some(file) = files
        .iter()
        .find(|file| file.old.as_deref() == Some(anchor.path.as_str()))
    else {
        return unchanged(anchor);
    };
    let Some(path) = file.new.clone() else {
        return Location {
            path: anchor.path.clone(),
            line: None,
            outdated: true,
        };
    };
    let line = match (anchor.side, anchor.line) {
        (Side::Old, _) => {
            return Location {
                path,
                line: anchor.old_line,
                outdated: !file.hunks.is_empty(),
            };
        }
        (Side::New, None) => {
            return Location {
                path,
                line: None,
                outdated: false,
            };
        }
        (Side::New, Some(line)) => line,
    };
    let start = anchor.start_line.unwrap_or(line).min(line);
    let mut shift = 0_i64;
    let mut outdated = false;
    for hunk in &file.hunks {
        if hunk.old_len == 0 {
            // Pure insertion after line `old_start`.
            if hunk.old_start < start {
                shift += i64::from(hunk.new_len);
            } else if hunk.old_start < line {
                outdated = true;
            }
            continue;
        }
        let end = hunk.old_start + hunk.old_len - 1;
        if end < start {
            shift += i64::from(hunk.new_len) - i64::from(hunk.old_len);
        } else if hunk.old_start <= line {
            outdated = true;
        }
    }
    Location {
        path,
        line: u32::try_from(i64::from(line) + shift).ok(),
        outdated,
    }
}

/// The old path and line of the line `line` of `path` at the head of a `-U0` diff, when the
/// diff changes the file but leaves that line as it was; `None` when the line is added or the
/// file is new, renamed from nothing or deleted.
pub fn old_of_unchanged(files: &[FileDiff], path: &str, line: u32) -> Option<(String, u32)> {
    let file = files
        .iter()
        .find(|file| file.new.as_deref() == Some(path))?;
    let old = file.old.clone()?;
    let mut shift = 0_i64;
    for hunk in &file.hunks {
        if hunk.new_len == 0 {
            // A pure deletion sits after line `new_start`.
            if line <= hunk.new_start {
                break;
            }
        } else if line < hunk.new_start {
            break;
        } else if line < hunk.new_start + hunk.new_len {
            return None;
        }
        shift += i64::from(hunk.old_len) - i64::from(hunk.new_len);
    }
    Some((old, u32::try_from(i64::from(line) + shift).ok()?))
}

/// A line of a unified diff: its old and new numbers (one is `None` for a removed or added
/// line) and the file's old and new paths.
pub struct DiffLine {
    pub old_path: String,
    pub new_path: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    /// Index of the hunk in the diff: a multi-line comment must start and end in one hunk.
    pub hunk: u32,
    /// Old and new line counters at this line, also for a line only on one side; GitLab's
    /// line codes use them.
    pub old_pos: u32,
    pub new_pos: u32,
}

/// Finds `line` on `side` of `path` inside a hunk of a `git diff -U3 -M` text, so a review
/// comment can be placed on it.
pub fn diff_line(diff: &str, path: &str, side: Side, line: u32) -> Option<DiffLine> {
    let mut paths: Option<(String, String)> = None;
    let mut numbers: Option<(u32, u32)> = None;
    let mut hunk = 0;
    for text in diff.lines() {
        if let Some(rest) = text.strip_prefix("diff --git ") {
            let path = header_path(rest).unwrap_or_default();
            paths = Some((path.clone(), path));
            numbers = None;
            continue;
        }
        let Some((old_path, new_path)) = &mut paths else {
            continue;
        };
        if let Some(rest) = text.strip_prefix("@@ ") {
            let mut parts = rest.split_whitespace();
            let old = parts.next().and_then(|part| range(part.strip_prefix('-')?));
            let new = parts.next().and_then(|part| range(part.strip_prefix('+')?));
            numbers = old.zip(new).map(|((old, _), (new, _))| (old, new));
            hunk += 1;
            continue;
        }
        let Some((old, new)) = &mut numbers else {
            if let Some(found) = text.strip_prefix("rename from ") {
                *old_path = found.to_owned();
            } else if let Some(found) = text.strip_prefix("rename to ") {
                *new_path = found.to_owned();
            } else if let Some(found) = text.strip_prefix("--- a/") {
                *old_path = header_name(found);
            } else if let Some(found) = text.strip_prefix("+++ b/") {
                *new_path = header_name(found);
            }
            continue;
        };
        let (old_line, new_line) = match text.as_bytes().first() {
            Some(b' ') => (Some(*old), Some(*new)),
            Some(b'-') => (Some(*old), None),
            Some(b'+') => (None, Some(*new)),
            _ => continue,
        };
        let hit = match side {
            Side::New => new_line == Some(line) && new_path == path,
            Side::Old => old_line == Some(line) && old_path == path,
        };
        if hit {
            return Some(DiffLine {
                old_path: old_path.clone(),
                new_path: new_path.clone(),
                old_line,
                new_line,
                hunk,
                old_pos: *old,
                new_pos: *new,
            });
        }
        if old_line.is_some() {
            *old += 1;
        }
        if new_line.is_some() {
            *new += 1;
        }
    }
    None
}

pub fn unchanged(anchor: &Anchor) -> Location {
    Location {
        path: anchor.path.clone(),
        line: match anchor.side {
            Side::New => anchor.line,
            Side::Old => anchor.old_line,
        },
        outdated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(path: &str, line: u32) -> Anchor {
        Anchor {
            path: path.into(),
            line: Some(line),
            old_line: None,
            side: Side::New,
            start_line: None,
            commit: None,
            tree: String::new(),
        }
    }

    const DIFF: &str = "diff --git a/a.txt b/b.txt\n\
similarity index 90%\n\
rename from a.txt\n\
rename to b.txt\n\
--- a/a.txt\n\
+++ b/b.txt\n\
@@ -2,0 +3,2 @@ ctx\n\
+new\n\
+--- a/fake\n\
@@ -5 +7 @@\n\
-old\n\
+changed\n";

    #[test]
    fn maps_an_unchanged_line_to_its_old_number() {
        let files = parse(DIFF);
        assert_eq!(
            old_of_unchanged(&files, "b.txt", 1),
            Some(("a.txt".into(), 1))
        );
        assert_eq!(
            old_of_unchanged(&files, "b.txt", 5),
            Some(("a.txt".into(), 3))
        );
        assert_eq!(
            old_of_unchanged(&files, "b.txt", 8),
            Some(("a.txt".into(), 6))
        );
        // Added or changed lines are not unchanged, and other files are not in the diff.
        assert_eq!(old_of_unchanged(&files, "b.txt", 4), None);
        assert_eq!(old_of_unchanged(&files, "b.txt", 7), None);
        assert_eq!(old_of_unchanged(&files, "c.txt", 1), None);
    }

    #[test]
    fn shifts_lines_across_rename() {
        let files = parse(DIFF);
        let moved = locate(&files, &anchor("a.txt", 4));
        assert_eq!(
            (moved.path.as_str(), moved.line, moved.outdated),
            ("b.txt", Some(6), false)
        );
        let first = locate(&files, &anchor("a.txt", 2));
        assert_eq!((first.line, first.outdated), (Some(2), false));
    }

    #[test]
    fn paths_with_spaces_drop_the_header_tab() {
        let diff = "diff --git a/my file.txt b/my file.txt\n--- a/my file.txt\t\n+++ b/my file.txt\t\n@@ -2 +2 @@\n-2\n+X\n";
        let anchor = |line| Anchor {
            path: "my file.txt".into(),
            line: Some(line),
            old_line: None,
            side: Side::New,
            start_line: None,
            commit: None,
            tree: String::new(),
        };
        assert!(locate(&parse(diff), &anchor(2)).outdated);
        assert!(!locate(&parse(diff), &anchor(3)).outdated);
        let found = diff_line(diff, "my file.txt", Side::New, 2).unwrap();
        assert_eq!(found.new_path, "my file.txt");
    }

    #[test]
    fn classifies_diff_lines() {
        let diff =
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
        let added = diff_line(diff, "x", Side::New, 2).unwrap();
        assert_eq!((added.old_line, added.new_line), (None, Some(2)));
        let context = diff_line(diff, "x", Side::New, 3).unwrap();
        assert_eq!((context.old_line, context.new_line), (Some(3), Some(3)));
        let removed = diff_line(diff, "x", Side::Old, 2).unwrap();
        assert_eq!((removed.old_line, removed.new_line), (Some(2), None));
        assert!(diff_line(diff, "x", Side::New, 9).is_none());
        assert!(diff_line(diff, "y", Side::New, 2).is_none());
    }

    #[test]
    fn changed_line_is_outdated() {
        let files = parse(DIFF);
        assert!(locate(&files, &anchor("a.txt", 5)).outdated);
        assert!(!locate(&files, &anchor("other.txt", 5)).outdated);
    }
}
