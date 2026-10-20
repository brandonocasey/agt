//! Number and table formatting shared by the reports.

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

pub use crate::core::table::table;

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
}
