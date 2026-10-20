use clap::builder::PossibleValue;
use clap::{Args, Subcommand, ValueEnum, value_parser};

use crate::core::error::{Result, fail};
use crate::task::records::is_id;

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
}

/// Which task a command works on.
#[derive(Args)]
pub struct Target {
    /// The task id, `[a-z0-9][a-z0-9._-]{0,63}`.
    #[arg(value_name = "ID")]
    id: String,
}

impl Target {
    pub fn id(&self) -> Result<String> {
        if is_id(&self.id) {
            Ok(self.id.clone())
        } else {
            fail("id must match [a-z0-9][a-z0-9._-]{0,63}")
        }
    }
}

/// The fields `create` sets and `update` changes.
#[derive(Args)]
pub struct Fields {
    #[arg(long)]
    pub parent_pr: Option<String>,
    #[arg(long)]
    pub target_branch: Option<String>,
    #[arg(long)]
    pub task_boundary: Option<String>,
    /// Where the user's authorization for the work came from.
    #[arg(long)]
    pub authorization: Option<String>,
    /// What the authorization covers [default on create with --authorization: unspecified].
    #[arg(long)]
    pub scope: Option<String>,
    #[arg(long)]
    pub next_step: Option<String>,
}

#[derive(Args)]
pub struct Create {
    #[command(flatten)]
    pub target: Target,
    /// What the task is for.
    #[arg(long)]
    pub objective: String,
    #[command(flatten)]
    pub fields: Fields,
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
    #[command(flatten)]
    pub fields: Fields,
    /// Add a correction the user made.
    #[arg(long)]
    pub correction: Option<String>,
    #[arg(long, value_enum)]
    pub status: Option<Status>,
    /// Add a completed check; what was checked. Needs `--check-result`.
    #[arg(long, requires = "check_result")]
    pub check_name: Option<String>,
    /// What the check found.
    #[arg(long, requires = "check_name")]
    pub check_result: Option<String>,
    /// The code revision checked [default: the task's HEAD, as recorded by this update].
    #[arg(long, requires = "check_name")]
    pub check_code_revision: Option<String>,
    #[arg(long, requires = "check_name")]
    pub check_inputs: Option<String>,
    /// Where the check ran [default: the platform and host name].
    #[arg(long, requires = "check_name")]
    pub check_environment: Option<String>,
}
