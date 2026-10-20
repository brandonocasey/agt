//! Number and table formatting shared by the reports.

use std::sync::LazyLock;

use regex::Regex;

/// `1234567.891` as `1,234,567.89`.
pub fn grouped(value: f64, places: usize) -> String {
    let plain = format!("{value:.places$}");
    let (sign, digits) = plain
        .strip_prefix('-')
        .map_or(("", plain.as_str()), |rest| ("-", rest));
    let (whole, fraction) = digits
        .split_once('.')
        .map_or((digits, None), |(whole, fraction)| (whole, Some(fraction)));
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

/// `value` rounded to `places` decimals.
pub fn round(value: f64, places: usize) -> f64 {
    format!("{value:.places$}").parse().unwrap_or(value)
}

/// A token count as `1.2k`, `3.4M` or `5.6G`, else the whole number.
pub fn tok(n: f64) -> String {
    for (div, unit) in [(1e9, 'G'), (1e6, 'M'), (1e3, 'k')] {
        if n.abs() >= div {
            return format!("{:.1}{unit}", n / div);
        }
    }
    format!("{}", n.trunc() as i64)
}

static NUMERIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-$\d.,%kMG/]+$").expect("valid pattern"));

/// A table with a dash rule under the headers. The first column and text columns are left
/// aligned and numeric columns right aligned.
pub fn table(out: &mut String, headers: &[&str], rows: &[Vec<String>]) {
    let width = |text: &str| text.chars().count();
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            rows.iter()
                .map(|row| width(&row[index]))
                .chain([width(header)])
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
            let pad = " ".repeat(widths[index].saturating_sub(width(cell)));
            if numeric[index] {
                line.push_str(&pad);
                line.push_str(cell);
            } else {
                line.push_str(cell);
                line.push_str(&pad);
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_format_with_groups_and_units() {
        assert_eq!(grouped(1234567.891, 2), "1,234,567.89");
        assert_eq!(grouped(-1234.5, 2), "-1,234.50");
        assert_eq!(grouped(999.999, 2), "1,000.00");
        assert_eq!(tok(1500.0), "1.5k");
        assert_eq!(tok(999.0), "999");
        assert_eq!(tok(2_500_000.0), "2.5M");
        assert_eq!(round(0.12345, 4), 0.1235);
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
