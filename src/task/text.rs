//! Value checks for fields that are persisted.

use crate::core::error::{Result, fail};

const MAX_TEXT: usize = 16_384;

/// A required, non-empty value of at most [`MAX_TEXT`] bytes.
pub fn limited(value: Option<&str>, name: &str) -> Result<String> {
    match value {
        None | Some("") => fail(format!("{name} must not be empty")),
        Some(value) if value.len() > MAX_TEXT => fail(format!("{name} exceeds {MAX_TEXT} bytes")),
        Some(value) => Ok(value.to_owned()),
    }
}
