use std::io::{IsTerminal, Write};

use serde_json::Value;

use super::error::{Result, ctx};

pub enum Output {
    Json(Value),
    Text(String),
    /// Everything is already printed; exit with this status.
    Exit(u8),
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
        Output::Exit(_) => return Ok(()),
    };
    std::io::stdout()
        .lock()
        .write_all(text.as_bytes())
        .map_err(ctx("write stdout"))
}
