use clap::{Args, value_parser};

/// What the index reads, shared by `index`, `resolve` and `search`.
#[derive(Args)]
pub struct Sources {
    /// Also index this transcript file.
    #[arg(long, value_name = "FILE")]
    pub file: Option<String>,
    /// Scan every source again instead of continuing from the index.
    #[arg(long)]
    pub rebuild: bool,
    /// Stop discovery after this many files [env: AGENT_TASK_MAX_FILES].
    #[arg(long, value_name = "N", value_parser = value_parser!(u64).range(1..))]
    pub max_files: Option<u64>,
    /// Stop reading after this many bytes [env: AGENT_TASK_BYTE_LIMIT].
    #[arg(long, value_name = "N", value_parser = value_parser!(u64).range(1..))]
    pub byte_limit: Option<u64>,
}

#[derive(Args)]
pub struct IndexArgs {
    #[command(flatten)]
    pub sources: Sources,
    /// Look for this exact session id first, even beyond --max-files.
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,
}

#[derive(Args)]
pub struct ResolveArgs {
    #[command(flatten)]
    pub sources: Sources,
    /// The exact session id.
    #[arg(value_name = "SESSION")]
    positional: Option<String>,
    /// The exact session id; takes precedence over the positional one.
    #[arg(long, value_name = "ID")]
    session: Option<String>,
}

impl ResolveArgs {
    pub fn session(&self) -> Option<&str> {
        self.session.as_deref().or(self.positional.as_deref())
    }
}

#[derive(Args)]
pub struct SearchArgs {
    #[command(flatten)]
    pub sources: Sources,
    /// Text to look for in human turns, case-insensitively.
    #[arg(long)]
    pub query: String,
    /// Most results to return.
    #[arg(long, default_value_t = 20, value_parser = value_parser!(u8).range(1..=100))]
    pub limit: u8,
    /// Only turns of this exact session id.
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,
}
