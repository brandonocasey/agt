use std::io::{IsTerminal, Write};

use serde_json::Value;

use super::error::{Error, Result, ctx};

/// Exit status of a command that failed on a usage or query error rather than a finding.
pub const FAILED: u8 = 2;

/// Exit status when a wait passes its time limit before the awaited thing happens.
pub const TIMED_OUT: u8 = 3;

/// Exit status of a wait that SIGINT ended: 128 plus the signal.
pub const INTERRUPTED: u8 = 130;

/// Exit status of a wait that SIGTERM ended: 128 plus the signal.
pub const TERMINATED: u8 = 143;

pub enum Output {
    Json(Value),
    Text(String),
    /// Everything is already printed; exit with this status.
    Exit(u8),
    /// Print the inner output, then exit with this status.
    Report(Box<Output>, u8),
    /// Print `agt: error: <message>` to stderr and exit with this status.
    Failure(Error, u8),
}

impl Output {
    /// Always-indented JSON text, even when piped.
    pub fn pretty(value: &Value) -> Result<Output> {
        pretty(value).map(Output::Text)
    }

    /// This output, then exit `code`; a zero code is a plain success.
    pub fn exiting(self, code: u8) -> Output {
        if code == 0 {
            self
        } else {
            Output::Report(Box::new(self), code)
        }
    }
}

/// `value` as indented JSON plus a newline, for output that is always indented.
pub fn pretty(value: &Value) -> Result<String> {
    let mut text = serde_json::to_string_pretty(value).map_err(ctx("serialize output"))?;
    text.push('\n');
    Ok(text)
}

pub fn print(output: Output) -> Result<()> {
    let text = match output {
        // Pretty for a person at a terminal; compact for agents and scripts, which pay per byte.
        Output::Json(value) => {
            let mut text = if std::io::stdout().is_terminal() {
                serde_json::to_string_pretty(&value)
            } else {
                serde_json::to_string(&value)
            }
            .map_err(ctx("serialize output"))?;
            text.push('\n');
            text
        }
        Output::Text(text) => text,
        Output::Report(inner, _) => return print(*inner),
        Output::Exit(_) | Output::Failure(..) => return Ok(()),
    };
    std::io::stdout()
        .lock()
        .write_all(text.as_bytes())
        .map_err(ctx("write stdout"))
}
