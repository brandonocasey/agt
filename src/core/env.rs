//! Reads of the `AGT_*` environment variables, so each kind of value is parsed one way.

use std::ffi::OsString;
use std::str::FromStr;

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

/// The value of `name`, else `default`; for overriding a program or a directory.
pub fn value_or(name: &str, default: &str) -> OsString {
    std::env::var_os(name).unwrap_or_else(|| default.into())
}
