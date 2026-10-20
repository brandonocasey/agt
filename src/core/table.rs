//! The one aligned text table the reports and `agt ci failures` print.

use std::sync::LazyLock;

use regex::Regex;

static NUMERIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-$\d.,%kMG/]+$").expect("valid pattern"));

/// A table with a dash rule under the headers. The first column and text columns are left
/// aligned and numeric columns right aligned.
pub fn table(out: &mut String, headers: &[&str], rows: &[Vec<String>]) {
    render(out, headers, rows, true);
}

/// Like [`table`], with every column left aligned.
pub fn table_left(out: &mut String, headers: &[&str], rows: &[Vec<String>]) {
    render(out, headers, rows, false);
}

fn render(out: &mut String, headers: &[&str], rows: &[Vec<String>], numbers_right: bool) {
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
            numbers_right
                && index > 0
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
