//! Reads of the `AGT_*` environment variables, so each kind of value is parsed one way.

use std::ffi::OsString;
use std::str::FromStr;

use super::error::{Result, fail};

/// Whether `name` is set to anything but empty, `0` or `false`.
pub fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !matches!(value.trim(), "" | "0" | "false"))
}

/// `name` as a number; unset or malformed is `None`.
pub fn number<T: FromStr>(name: &str) -> Option<T> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
}

/// `name` as a positive integer; unset or empty is `None`, anything else is an error.
pub fn positive(name: &str) -> Result<Option<u64>> {
    match std::env::var(name).ok().filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => match value.trim().parse::<u64>() {
            Ok(number) if number >= 1 => Ok(Some(number)),
            _ => fail(format!("{name} must be a positive integer")),
        },
    }
}

/// The value of `name`, else `default`; for overriding a program or a directory.
pub fn value_or(name: &str, default: &str) -> OsString {
    std::env::var_os(name).unwrap_or_else(|| default.into())
}
