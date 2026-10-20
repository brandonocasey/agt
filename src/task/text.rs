//! Value checks for fields that are persisted.

use crate::core::error::Result;
use crate::core::text::bounded;

const MAX_TEXT: usize = 16_384;

/// A required, non-empty value of at most [`MAX_TEXT`] bytes.
pub fn limited(value: Option<&str>, name: &str) -> Result<String> {
    let value = value.unwrap_or_default();
    bounded(value, name, MAX_TEXT, false)?;
    Ok(value.to_owned())
}
