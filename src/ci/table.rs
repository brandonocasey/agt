//! Output: lines flushed as they are written (a closed pipe is not an error) and the aligned
//! tables.

use std::io::Write;

pub struct Out;

impl Out {
    pub fn line(&self, text: &str) {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{text}");
        let _ = stdout.flush();
    }
}

pub fn table<S: AsRef<str>>(out: &Out, headers: &[S], rows: &[Vec<String>]) {
    let headers: Vec<&str> = headers.iter().map(AsRef::as_ref).collect();
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            rows.iter()
                .map(|row| row[index].chars().count())
                .chain([header.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let format = |cells: &[&str]| {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        padded.join("  ").trim_end().to_owned()
    };
    out.line(&format(&headers));
    let dashes: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();
    out.line(&format(
        &dashes.iter().map(String::as_str).collect::<Vec<_>>(),
    ));
    for row in rows {
        out.line(&format(&row.iter().map(String::as_str).collect::<Vec<_>>()));
    }
}
