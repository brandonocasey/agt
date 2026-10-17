use clap::{Args, Subcommand, ValueEnum};

#[derive(Args)]
pub struct Cli {
    /// Only this tracker: its key (`gh-github.com-owner-repo`) or `owner/repo`.
    #[arg(long, global = true, value_name = "KEY")]
    pub tracker: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Fetch what changed on the configured trackers. Writes nothing to a host.
    Sync,
    /// List the local issues.
    List(ListArgs),
    /// Show one issue with its comments, links and queued changes.
    Show {
        /// `owner/repo#12`, `#12`, `12` or `local-xxxx`.
        key: String,
    },
    /// Find issues whose title, body, comments or notes contain the text.
    Search { text: String },
    /// Per tracker: counts, queued changes, conflicts and errors. Never writes.
    Status,
    /// Find the issues for a PR/MR or a branch from the local data.
    Find(FindArgs),
    /// Link an issue to a PR/MR, issue, review target, task or branch, locally only.
    Link(LinkArgs),
    /// Keep a private note on an issue; never sent.
    Note {
        key: String,
        /// The note; `-` reads stdin.
        text: String,
    },
}

#[derive(Args)]
pub struct ListArgs {
    #[arg(long, value_enum, default_value = "open")]
    pub state: StateFilter,
    #[arg(long)]
    pub label: Option<String>,
    #[arg(long)]
    pub assignee: Option<String>,
    #[arg(long)]
    pub author: Option<String>,
    /// Only issues with queued changes.
    #[arg(long)]
    pub pending: bool,
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum StateFilter {
    Open,
    Closed,
    All,
}

#[derive(Args)]
pub struct FindArgs {
    /// A PR/MR number or URL.
    #[arg(long, required_unless_present = "branch")]
    pub pr: Option<String>,
    /// A branch name.
    #[arg(long, required_unless_present = "pr")]
    pub branch: Option<String>,
    /// The PR/MR title: references such as `#12` in it name issues.
    #[arg(long)]
    pub title: Option<String>,
}

#[derive(Args)]
pub struct LinkArgs {
    pub key: String,
    #[arg(long, group = "target")]
    pub pr: Option<String>,
    /// Another issue.
    #[arg(long, group = "target")]
    pub issue: Option<String>,
    /// A review target key.
    #[arg(long, group = "target")]
    pub review: Option<String>,
    /// An `agt task` id.
    #[arg(long, group = "target")]
    pub task: Option<String>,
    #[arg(long, group = "target")]
    pub branch: Option<String>,
}
