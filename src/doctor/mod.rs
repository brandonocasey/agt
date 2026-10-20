//! `agt doctor`: a read-only report of whether this host and repository are set up for agt.
//! It never writes, never reads credentials (forge logins are only `auth status` exit codes and
//! the account name they print), and bounds every subprocess with one overall deadline.

mod checks;

use std::time::Duration;

use clap::Args;
use serde_json::json;

use crate::core::error::Result;
use crate::core::output::Output;
use crate::forge::transport::{clear_deadline, set_deadline};

/// All checks together may take this long; later subprocesses then report as unavailable.
const BUDGET: Duration = Duration::from_secs(30);

/// Check that this host and repository are set up for agt-driven work.
///
/// Checks tools (git >= 2.38, gh, glab, mise), forge logins, the agt version pinned in mise,
/// the global git ignore for `.worktrees/` and `/.cache/agents/scratch/`, agt state and config,
/// and, inside a repository, origin, the default branch and stray agt locks. Each failed check
/// prints a fix hint. Exit status: 0 all ok, 1 any check failed, 2 only warnings.
#[derive(Args)]
pub struct Cli {
    /// Print the rows as JSON instead of a table.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

pub struct Row {
    pub name: String,
    pub status: Status,
    pub detail: String,
    pub fix: Option<String>,
}

impl Row {
    pub fn ok(name: &str, detail: impl Into<String>) -> Self {
        Self::new(name, Status::Ok, detail, None)
    }

    pub fn warn(name: &str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self::new(name, Status::Warn, detail, Some(fix.into()))
    }

    pub fn fail(name: &str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self::new(name, Status::Fail, detail, Some(fix.into()))
    }

    fn new(name: &str, status: Status, detail: impl Into<String>, fix: Option<String>) -> Self {
        Self {
            name: name.to_owned(),
            status,
            detail: detail.into(),
            fix,
        }
    }
}

fn table(rows: &[Row]) -> String {
    let width = rows.iter().map(|row| row.name.len()).max().unwrap_or(0);
    let mut text = String::new();
    for row in rows {
        text.push_str(&format!(
            "{:<4}  {:<width$}  {}\n",
            row.status.name(),
            row.name,
            row.detail
        ));
        if let Some(fix) = &row.fix {
            text.push_str(&format!("      {:<width$}  fix: {fix}\n", ""));
        }
    }
    text
}

pub fn run(cli: Cli) -> Result<Output> {
    set_deadline(BUDGET);
    let rows = checks::all();
    clear_deadline();
    let worst = rows
        .iter()
        .map(|row| row.status)
        .max()
        .unwrap_or(Status::Ok);
    let output = if cli.json {
        let rows: Vec<_> = rows
            .iter()
            .map(|row| {
                json!({
                    "name": row.name,
                    "status": row.status.name(),
                    "detail": row.detail,
                    "fix": row.fix,
                })
            })
            .collect();
        Output::Json(json!({ "status": worst.name(), "checks": rows }))
    } else {
        Output::Text(table(&rows))
    };
    Ok(output.exiting(match worst {
        Status::Ok => 0,
        Status::Fail => 1,
        Status::Warn => 2,
    }))
}
