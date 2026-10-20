use clap::{Args, value_parser};

/// `search`: the text of human turns, one session, or the index itself.
#[derive(Args)]
pub struct SearchArgs {
    /// Also index this transcript file.
    #[arg(long, value_name = "FILE")]
    pub file: Option<String>,
    /// Scan every source again instead of continuing from the index.
    #[arg(long)]
    pub rebuild: bool,
    /// Text to look for in human turns, case-insensitively. Without it, `--session` prints the
    /// session's sources and turn count, and with neither the index coverage is printed.
    #[arg(long)]
    pub query: Option<String>,
    /// Most results to return.
    #[arg(long, default_value_t = 20, value_parser = value_parser!(u8).range(1..=100))]
    pub limit: u8,
    /// Only turns of this exact session id; its transcripts are found beyond any other.
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,
}
