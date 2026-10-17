//! Value checks and string slicing that follow the JavaScript semantics of `agent-task`.

use crate::core::error::{Result, fail};

pub const MAX_TEXT: usize = 16_384;

/// A required, non-empty value of at most [`MAX_TEXT`] bytes.
pub fn limited(value: Option<&str>, name: &str) -> Result<String> {
    match value {
        None | Some("") => fail(format!("{name} must not be empty")),
        Some(value) if value.len() > MAX_TEXT => fail(format!("{name} exceeds {MAX_TEXT} bytes")),
        Some(value) => Ok(value.to_owned()),
    }
}

/// `Number(text)` for the forms an option can plausibly take.
pub fn js_number(text: &str) -> f64 {
    let text = text.trim();
    if text.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = text.strip_prefix(prefix) {
            return u64::from_str_radix(digits, radix).map_or(f64::NAN, |number| number as f64);
        }
    }
    match text {
        "Infinity" | "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ if text
            .chars()
            .any(|c| c.is_ascii_alphabetic() && !matches!(c, 'e' | 'E')) =>
        {
            f64::NAN
        }
        _ => text.parse().unwrap_or(f64::NAN),
    }
}

/// `Number.isSafeInteger`, as an integer.
pub fn safe_integer(number: f64) -> Option<i64> {
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
    (number.is_finite() && number.fract() == 0.0 && number.abs() <= MAX_SAFE)
        .then_some(number as i64)
}

/// The text between UTF-16 code unit offsets `start` and `end`, which is what `String.slice`
/// counts. A surrogate pair cut by the range is dropped rather than emitted as a lone half.
pub fn utf16_slice(text: &str, start: usize, end: usize) -> String {
    let mut position = 0;
    let mut out = String::new();
    for character in text.chars() {
        let next = position + character.len_utf16();
        if position >= start && next <= end {
            out.push(character);
        }
        position = next;
        if position >= end {
            break;
        }
    }
    out
}

/// The offset in UTF-16 code units of byte offset `byte` in `text`.
pub fn utf16_offset(text: &str, byte: usize) -> usize {
    text[..byte].chars().map(char::len_utf16).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_follow_javascript() {
        assert_eq!(js_number(" 12 "), 12.0);
        assert_eq!(js_number("1e1"), 10.0);
        assert_eq!(js_number("0x10"), 16.0);
        assert!(js_number("abc").is_nan());
        assert!(js_number("inf").is_nan());
        assert_eq!(safe_integer(3.0), Some(3));
        assert_eq!(safe_integer(3.5), None);
        assert_eq!(safe_integer(f64::NAN), None);
    }

    #[test]
    fn slices_count_utf16_units() {
        assert_eq!(utf16_slice("a\u{1F600}b", 0, 2), "a");
        assert_eq!(utf16_slice("a\u{1F600}b", 0, 3), "a\u{1F600}");
        assert_eq!(utf16_slice("a\u{1F600}b", 3, 10), "b");
        assert_eq!(utf16_offset("a\u{1F600}b", 5), 3);
    }
}
