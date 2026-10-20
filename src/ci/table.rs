//! Output: lines flushed as they are written (a closed pipe is not an error).

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
    let mut text = String::new();
    crate::core::table::table_left(&mut text, &headers, rows);
    for line in text.lines() {
        out.line(line);
    }
}
