//! Tolerant readers for transcript records, whose fields are often missing or of another type.

use serde_json::Value;

static NULL: Value = Value::Null;

/// `value[key]`, or `Null` for a missing key or a value that is not an object.
pub fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&NULL)
}

/// `value[key]` when it is an object, else `Null`.
pub fn object<'a>(value: &'a Value, key: &str) -> &'a Value {
    let found = get(value, key);
    if found.is_object() { found } else { &NULL }
}

/// Whether the value is set: not null, false, zero, or empty.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// `value[key]` as a token count: 0 when missing or not a number.
pub fn count(value: &Value, key: &str) -> i64 {
    match get(value, key) {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64))
            .unwrap_or_default(),
        Value::Bool(true) => 1,
        _ => 0,
    }
}

/// `value[key]` as text: empty when missing or not a string.
pub fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    get(value, key).as_str().unwrap_or_default()
}

/// A hashable stand-in for an id of any JSON type.
pub fn key(value: &Value) -> String {
    match value {
        Value::String(text) => format!("s{text}"),
        other => format!("v{other}"),
    }
}

/// The first `n` characters of `text`.
pub fn head(text: &str, n: usize) -> &str {
    text.char_indices()
        .nth(n)
        .map_or(text, |(at, _)| &text[..at])
}

/// How many characters of a session id name a session in reports.
pub const SHORT_ID: usize = 8;

/// The last `n` characters of `text`.
pub fn tail(text: &str, n: usize) -> &str {
    let total = text.chars().count();
    if total <= n {
        return text;
    }
    text.char_indices()
        .nth(total - n)
        .map_or(text, |(at, _)| &text[at..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_count_characters() {
        assert_eq!(head("héllo", 2), "hé");
        assert_eq!(tail("héllo", 3), "llo");
        assert_eq!(tail("ab", 5), "ab");
    }
}
