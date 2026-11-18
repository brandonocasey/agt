//! Output lines flush as written. A reader that closes the pipe ends the output quietly.

use std::cell::Cell;

use crate::core::output::write_stdout;

#[derive(Default)]
pub struct Out {
    closed: Cell<bool>,
}

impl Out {
    /// Writes `text` and a newline. After the reader closes stdout, writes nothing more.
    pub fn line(&self, text: &str) {
        if self.closed.get() {
            return;
        }
        if matches!(write_stdout(format!("{text}\n").as_bytes()), Ok(false)) {
            self.closed.set(true);
        }
    }

    /// Whether no write has found stdout closed.
    pub fn is_open(&self) -> bool {
        !self.closed.get()
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
