//! Python's value and text rules, so reports print byte for byte what the `agent-sessions`
//! script printed: truthiness, `json.dumps`, `repr`, float formatting and the table layout.

use std::fmt::Write as _;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

pub use crate::core::transcripts::json::{count, get, head, key, object, tail, text, truthy};

/// Python's `str.isspace()` set: Unicode white space plus the four separator controls.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

pub fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

pub fn split(text: &str) -> impl Iterator<Item = &str> {
    text.split(is_space).filter(|part| !part.is_empty())
}

pub fn chars(text: &str) -> usize {
    text.chars().count()
}

/// `f"{value:.{places}f}"`.
pub fn fixed(value: f64, places: usize) -> String {
    format!("{value:.places$}")
}

/// `f"{value:,.{places}f}"`.
pub fn grouped(value: f64, places: usize) -> String {
    let plain = fixed(value, places);
    let (sign, digits) = plain
        .strip_prefix('-')
        .map_or(("", plain.as_str()), |rest| ("-", rest));
    let (whole, fraction) = digits
        .split_once('.')
        .map_or((digits, None), |(w, f)| (w, Some(f)));
    let mut out = String::from(sign);
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    if let Some(fraction) = fraction {
        out.push('.');
        out.push_str(fraction);
    }
    out
}

/// `round(value, places)` for a float: the correctly rounded decimal, read back.
pub fn round(value: f64, places: usize) -> f64 {
    fixed(value, places).parse().unwrap_or(value)
}

/// `str(int(value))`: truncated toward zero.
pub fn int(value: f64) -> String {
    format!("{}", value.trunc() as i64)
}

/// The script's token count: `1.2k`, `3.4M`, `5.6G`, else the whole number.
pub fn tok(n: f64) -> String {
    for (div, unit) in [(1e9, 'G'), (1e6, 'M'), (1e3, 'k')] {
        if n.abs() >= div {
            return format!("{}{unit}", fixed(n / div, 1));
        }
    }
    int(n)
}

/// `statistics.median`.
pub fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// `repr(float)`: the shortest round-trip digits, in exponent form below 1e-4 or from 1e16.
pub fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.into();
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let exp = format!("{:e}", value.abs());
    let (mantissa, power) = exp.split_once('e').unwrap_or((&exp, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let power: i64 = power.parse().unwrap_or_default();
    let point = power + 1;
    let length = digits.len() as i64;
    let body = if (-4 < point) && (point <= 16) {
        if point <= 0 {
            format!("0.{}{digits}", "0".repeat((-point) as usize))
        } else if point >= length {
            format!("{digits}{}.0", "0".repeat((point - length) as usize))
        } else {
            let (whole, fraction) = digits.split_at(point as usize);
            format!("{whole}.{fraction}")
        }
    } else {
        let (first, rest) = digits.split_at(1);
        let mantissa = if rest.is_empty() {
            first.to_owned()
        } else {
            format!("{first}.{rest}")
        };
        let exponent = point - 1;
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exponent.abs())
    };
    format!("{sign}{body}")
}

fn number(number: &serde_json::Number) -> String {
    if number.is_f64() {
        let value = number.as_f64().unwrap_or_default();
        if value.is_nan() {
            return "NaN".into();
        }
        if value.is_infinite() {
            return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
        }
        float_repr(value)
    } else {
        number.to_string()
    }
}

fn json_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

/// `json.dumps(value)`: ASCII only, `", "` and `": "` separators.
pub fn dumps(value: &Value) -> String {
    let mut out = String::new();
    write_json(&mut out, value, None, 0);
    out
}

/// `json.dump(value, indent=1)`.
pub fn dumps_indented(value: &Value) -> String {
    let mut out = String::new();
    write_json(&mut out, value, Some(1), 1);
    out
}

fn write_json(out: &mut String, value: &Value, indent: Option<usize>, level: usize) {
    let (separator, open, close) = match indent {
        Some(width) => (
            format!(",\n{}", " ".repeat(width * level)),
            format!("\n{}", " ".repeat(width * level)),
            format!("\n{}", " ".repeat(width * (level - 1))),
        ),
        None => (", ".into(), String::new(), String::new()),
    };
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number(n)),
        Value::String(text) => json_string(out, text),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Array(items) => {
            out.push('[');
            out.push_str(&open);
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(&separator);
                }
                write_json(out, item, indent, level + 1);
            }
            out.push_str(&close);
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            out.push_str(&open);
            for (index, (name, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push_str(&separator);
                }
                json_string(out, name);
                out.push_str(": ");
                write_json(out, item, indent, level + 1);
            }
            out.push_str(&close);
            out.push('}');
        }
    }
}

/// `repr(str)`.
pub fn str_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => {
                let code = c as u32;
                let _ = if code < 0x100 {
                    write!(out, "\\x{code:02x}")
                } else {
                    write!(out, "\\u{code:04x}")
                };
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `str(dict)` for a dict of string keys and integer values.
pub fn counts_repr(items: &[(String, i64)]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|(name, count)| format!("{}: {count}", str_repr(name)))
        .collect();
    format!("{{{}}}", parts.join(", "))
}

static NUMERIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-$\d.,%kMG/]+$").expect("valid pattern"));

/// The script's `table`: first and text columns left-aligned, numeric columns right-aligned,
/// a dash rule under the headers, trailing space trimmed.
pub fn table(out: &mut String, headers: &[&str], rows: &[Vec<String>]) {
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            rows.iter()
                .map(|row| chars(&row[index]))
                .chain([chars(header)])
                .max()
                .unwrap_or_default()
        })
        .collect();
    let numeric: Vec<bool> = (0..headers.len())
        .map(|index| {
            index > 0
                && rows.iter().all(|row| {
                    let cell = &row[index];
                    NUMERIC.is_match(if cell.is_empty() { "0" } else { cell })
                })
        })
        .collect();
    let rule: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();
    let header: Vec<String> = headers.iter().map(|header| (*header).to_owned()).collect();
    for row in [&header, &rule].into_iter().chain(rows) {
        let mut line = String::new();
        for (index, cell) in row.iter().enumerate() {
            if index > 0 {
                line.push_str("  ");
            }
            let pad = " ".repeat(widths[index].saturating_sub(chars(cell)));
            if numeric[index] {
                line.push_str(&pad);
                line.push_str(cell);
            } else {
                line.push_str(cell);
                line.push_str(&pad);
            }
        }
        out.push_str(line.trim_end_matches(is_space));
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats_print_as_python_repr() {
        assert_eq!(float_repr(0.0), "0.0");
        assert_eq!(float_repr(1.5), "1.5");
        assert_eq!(float_repr(0.0001), "0.0001");
        assert_eq!(float_repr(0.00005), "5e-05");
        assert_eq!(float_repr(12.0), "12.0");
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(1234567890123456.0), "1234567890123456.0");
        assert_eq!(float_repr(-2.25e-7), "-2.25e-07");
        assert_eq!(float_repr(0.1 + 0.2), "0.30000000000000004");
    }

    #[test]
    fn numbers_format_as_python() {
        assert_eq!(grouped(1234567.891, 2), "1,234,567.89");
        assert_eq!(grouped(-1234.5, 2), "-1,234.50");
        assert_eq!(grouped(999.999, 2), "1,000.00");
        assert_eq!(fixed(0.125, 2), "0.12");
        assert_eq!(fixed(2.5, 0), "2");
        assert_eq!(tok(1500.0), "1.5k");
        assert_eq!(tok(999.0), "999");
        assert_eq!(tok(2_500_000.0), "2.5M");
        assert_eq!(round(0.12345, 4), 0.1235);
        assert_eq!(median(vec![3.0, 1.0, 2.0, 10.0]), 2.5);
    }

    #[test]
    fn json_matches_python_dumps() {
        let value = json!({"a": [1, 2.5, "é😀"], "b": {}, "c": null});
        assert_eq!(
            dumps(&value),
            r#"{"a": [1, 2.5, "\u00e9\ud83d\ude00"], "b": {}, "c": null}"#
        );
        assert_eq!(
            dumps_indented(&json!({"rows": [{"x": 1}], "u": {}})),
            "{\n \"rows\": [\n  {\n   \"x\": 1\n  }\n ],\n \"u\": {}\n}"
        );
    }

    #[test]
    fn strings_repr_as_python() {
        assert_eq!(str_repr("plain"), "'plain'");
        assert_eq!(str_repr("it's"), "\"it's\"");
        assert_eq!(str_repr("a'b\"c"), "'a\\'b\"c'");
        assert_eq!(counts_repr(&[("m".into(), 2)]), "{'m': 2}");
    }

    #[test]
    fn tables_align_numbers_right() {
        let mut out = String::new();
        table(
            &mut out,
            &["name", "n"],
            &[vec!["a".into(), "10".into()], vec!["bb".into(), "2".into()]],
        );
        assert_eq!(out, "name   n\n----  --\na     10\nbb     2\n");
    }
}
