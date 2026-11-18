//! Key spelling for JSON that agt prints from data stored with other spellings.

use serde_json::{Map, Value};

/// `value` with every object key, at every depth, in snake_case, such as `updated_at` for
/// `updatedAt`. Keys already in snake_case, and all other values, are unchanged.
pub fn snake_case_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, item)| (snake_case(&key), snake_case_keys(item)))
                .collect::<Map<String, Value>>(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(snake_case_keys).collect()),
        other => other,
    }
}

fn snake_case(key: &str) -> String {
    let mut text = String::with_capacity(key.len() + 2);
    let mut previous_is_lower = false;
    for letter in key.chars() {
        if letter.is_ascii_uppercase() && previous_is_lower {
            text.push('_');
        }
        text.push(letter.to_ascii_lowercase());
        previous_is_lower = letter.is_ascii_lowercase() || letter.is_ascii_digit();
    }
    text
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn keys_are_renamed_at_every_depth() {
        let value = json!({
            "updatedAt": 1,
            "plain": [{"parentPr": null, "id": "x"}],
            "already_snake": {"sourceType": "a", "kept_as_is": 2},
            "values": "stayCamel",
        });
        assert_eq!(
            snake_case_keys(value),
            json!({
                "updated_at": 1,
                "plain": [{"parent_pr": null, "id": "x"}],
                "already_snake": {"source_type": "a", "kept_as_is": 2},
                "values": "stayCamel",
            })
        );
    }
}
