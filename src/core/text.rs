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
