use clap::builder::PossibleValue;
use clap::{Args, Subcommand, ValueEnum, value_parser};

use crate::core::error::{Result, fail};
use crate::task::records::is_id;
use crate::task::sessions::{IndexArgs, ResolveArgs, SearchArgs};

#[derive(Subcommand)]
pub enum Command {
    /// Create a durable task record.
    Create(Create),
    /// List task records, most recently updated first.
    List,
    /// Show one task record.
    Show(Target),
    /// Update one task record.
    Update(Update),
    /// Add a completed check to a task record.
    Check(Check),
    /// Update the private session index and show coverage.
    Index(IndexArgs),
    /// Resolve an exact session id to its source.
    Resolve(ResolveArgs),
    /// Search indexed human turns with bounded context.
    Search(SearchArgs),
}

/// Which task a command works on.
#[derive(Args)]
pub struct Target {
    /// The task id, `[a-z0-9][a-z0-9._-]{0,63}`.
    #[arg(value_name = "ID")]
    positional: Option<String>,
    /// The task id; takes precedence over the positional one.
    #[arg(long, value_name = "ID")]
    id: Option<String>,
}

impl Target {
    pub fn id(&self) -> Result<String> {
        match self.id.as_ref().or(self.positional.as_ref()) {
            Some(id) if is_id(id) => Ok(id.clone()),
            _ => fail("id must match [a-z0-9][a-z0-9._-]{0,63}"),
        }
    }
}

#[derive(Args)]
pub struct Create {
    #[command(flatten)]
    pub target: Target,
    /// What the task is for.
    #[arg(long)]
    pub objective: String,
    #[arg(long)]
    pub parent_pr: Option<String>,
    #[arg(long)]
    pub target_branch: Option<String>,
    #[arg(long)]
    pub task_boundary: Option<String>,
    /// Where the user's authorization for the work came from.
    #[arg(long)]
    pub authorization: Option<String>,
    /// What the authorization covers [default with --authorization: unspecified].
    #[arg(long)]
    pub scope: Option<String>,
    #[arg(long)]
    pub next_step: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Active,
    Paused,
    Blocked,
    Complete,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Complete => "complete",
        }
    }
}

impl ValueEnum for Status {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::Active, Self::Paused, Self::Blocked, Self::Complete]
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(PossibleValue::new(self.as_str()))
    }
}

#[derive(Args)]
pub struct Update {
    #[command(flatten)]
    pub target: Target,
    /// The revision the record is at now; the update fails if it has moved on.
    #[arg(long, value_parser = value_parser!(i64).range(1..))]
    pub revision: i64,
    #[arg(long)]
    pub objective: Option<String>,
    #[arg(long)]
    pub parent_pr: Option<String>,
    #[arg(long)]
    pub target_branch: Option<String>,
    #[arg(long)]
    pub task_boundary: Option<String>,
    /// Add a correction the user made.
    #[arg(long)]
    pub correction: Option<String>,
    #[arg(long)]
    pub next_step: Option<String>,
    #[arg(long, value_enum)]
    pub status: Option<Status>,
    #[arg(long)]
    pub authorization: Option<String>,
    #[arg(long)]
    pub scope: Option<String>,
}

#[derive(Args)]
pub struct Check {
    #[command(flatten)]
    pub target: Target,
    /// The revision the record is at now; the update fails if it has moved on.
    #[arg(long, value_parser = value_parser!(i64).range(1..))]
    pub revision: i64,
    /// What was checked.
    #[arg(long)]
    pub name: String,
    /// What the check found.
    #[arg(long)]
    pub result: String,
    /// The code revision checked [default: the task's recorded HEAD].
    #[arg(long)]
    pub code_revision: Option<String>,
    #[arg(long)]
    pub inputs: Option<String>,
    /// Where the check ran [default: the platform and host name].
    #[arg(long)]
    pub environment: Option<String>,
}
