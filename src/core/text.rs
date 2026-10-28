//! Checks and reads shared by the text arguments agt persists.

use super::error::{Result, fail};
use super::local::read_stdin;

/// `text` unchanged, or all of stdin when it is `-`; `what` names it in errors.
pub fn argument_or_stdin(text: &str, what: &str) -> Result<String> {
    if text == "-" {
        read_stdin(what)
    } else {
        Ok(text.to_owned())
    }
}

/// Fails when `text` is longer than `max` bytes.
pub fn within(text: &str, name: &str, max: usize) -> Result<()> {
    if text.len() > max {
        return fail(format!("{name} exceeds {max} bytes"));
    }
    Ok(())
}

/// The first `len` bytes of `text`, or all of it when it is shorter.
pub fn prefix(text: &str, len: usize) -> &str {
    text.get(..len).unwrap_or(text)
}

/// The short form of a commit hash.
pub fn short(hash: &str) -> &str {
    prefix(hash, 7)
}

/// `text` unchanged while it has at most `limit` characters, else its first `keep` characters
/// and `…`.
pub fn cut(text: &str, limit: usize, keep: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(keep).collect();
    cut.push('…');
    cut
}

/// The longest free text, in bytes, that a review or an issue stores.
pub const MAX_TEXT: usize = 65_536;

/// Fails when `text` is empty (or only whitespace when `trimmed`) or longer than `max` bytes.
pub fn bounded(text: &str, name: &str, max: usize, trimmed: bool) -> Result<()> {
    let empty = if trimmed {
        text.trim().is_empty()
    } else {
        text.is_empty()
    };
    if empty {
        return fail(format!("{name} must not be empty"));
    }
    within(text, name, max)
}
