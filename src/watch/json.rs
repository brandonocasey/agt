//! JSON values that keep the order keys were added in, which is the order agent-watch's
//! JavaScript objects print in; `serde_json::Value` sorts its keys.

use std::cmp::Ordering;

use serde_json::Value;

#[derive(Clone, Debug)]
pub enum J {
    Raw(Value),
    Obj(Fields),
    /// An object parsed from input, with its own keys.
    Parsed(Vec<(String, J)>),
    Arr(Vec<J>),
}

pub type Fields = Vec<(&'static str, J)>;

impl J {
    pub fn null() -> Self {
        Self::Raw(Value::Null)
    }

    pub fn str(text: &str) -> Self {
        Self::Raw(Value::String(text.to_owned()))
    }

    pub fn render(&self, out: &mut String) {
        match self {
            Self::Raw(value) => out.push_str(&value.to_string()),
            Self::Obj(fields) => render_fields(fields, out),
            Self::Parsed(fields) => {
                out.push('{');
                for (index, (key, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String(key.clone()).to_string());
                    out.push(':');
                    value.render(out);
                }
                out.push('}');
            }
            Self::Arr(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.render(out);
                }
                out.push(']');
            }
        }
    }

    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.render(&mut out);
        out
    }
}

/// Parses JSON text keeping the order of object keys.
pub fn parse_ordered(text: &str) -> Option<J> {
    serde_json::from_str::<Ordered>(text)
        .ok()
        .map(|ordered| ordered.0)
}

struct Ordered(J);

impl<'de> serde::Deserialize<'de> for Ordered {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = Ordered;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("any JSON value")
            }

            fn visit_unit<E>(self) -> Result<Ordered, E> {
                Ok(Ordered(J::null()))
            }

            fn visit_bool<E>(self, value: bool) -> Result<Ordered, E> {
                Ok(Ordered(J::Raw(Value::Bool(value))))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Ordered, E> {
                Ok(Ordered(J::Raw(Value::from(value))))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Ordered, E> {
                Ok(Ordered(J::Raw(Value::from(value))))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Ordered, E> {
                Ok(Ordered(J::Raw(Value::from(value))))
            }

            fn visit_str<E>(self, value: &str) -> Result<Ordered, E> {
                Ok(Ordered(J::str(value)))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Ordered, A::Error> {
                let mut items = Vec::new();
                while let Some(Ordered(item)) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Ordered(J::Arr(items)))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Ordered, A::Error> {
                let mut fields: Vec<(String, J)> = Vec::new();
                while let Some((key, Ordered(value))) = map.next_entry::<String, Ordered>()? {
                    // A repeated key keeps its first position and the last value.
                    match fields.iter_mut().find(|(name, _)| *name == key) {
                        Some(slot) => slot.1 = value,
                        None => fields.push((key, value)),
                    }
                }
                Ok(Ordered(J::Parsed(fields)))
            }
        }
        deserializer.deserialize_any(Visit)
    }
}

pub fn render_fields(fields: &Fields, out: &mut String) {
    out.push('{');
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&Value::String((*key).to_owned()).to_string());
        out.push(':');
        value.render(out);
    }
    out.push('}');
}

/// JavaScript truthiness of a parsed value.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `object[key]`, `null` when the key or the object is missing (`undefined` in JavaScript, which
/// every use here treats as `null`).
pub fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
    static NULL: Value = Value::Null;
    value.get(key).unwrap_or(&NULL)
}

pub fn is_str(value: &Value, expected: &str) -> bool {
    value.as_str() == Some(expected)
}

/// `a.localeCompare(b)` for the ICU root collation as far as ASCII goes: whitespace, then
/// punctuation in the CLDR order, then digits, then letters ignoring case; a lowercase letter
/// sorts before its uppercase form only when everything else is equal.
pub fn locale_compare(left: &str, right: &str) -> Ordering {
    let primary = |text: &str| -> Vec<(u32, char)> {
        text.chars()
            .map(|c| (primary_weight(c), c.to_lowercase().next().unwrap_or(c)))
            .collect()
    };
    primary(left).cmp(&primary(right)).then_with(|| {
        for (a, b) in left.chars().zip(right.chars()) {
            if a != b {
                // Lowercase first.
                return b.is_lowercase().cmp(&a.is_lowercase());
            }
        }
        Ordering::Equal
    })
}

const PUNCTUATION: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

fn primary_weight(c: char) -> u32 {
    if c.is_whitespace() {
        return 1;
    }
    if let Some(at) = PUNCTUATION.find(c) {
        return 100 + at as u32;
    }
    if c.is_ascii_digit() {
        return 1000 + (c as u32 - '0' as u32);
    }
    if c.is_alphabetic() {
        return 2000;
    }
    // Other symbols sort among the punctuation, after the ASCII ones.
    500
}
