use clap::{Args, Subcommand, ValueEnum};

#[derive(Args)]
pub struct Cli {
    /// Only this tracker: its key (`gh-github.com-owner-repo`) or `owner/repo`.
    #[arg(long, global = true, value_name = "KEY")]
    pub tracker: Option<String>,
    #[command(flatten)]
    pub offline: crate::forge::auto::Offline,
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
    /// Link an issue to a PR/MR, issue, review target, task or branch, locally; `--pr --comment`
    /// also queues a comment naming the PR/MR.
    Link(LinkArgs),
    /// Keep a private note on an issue; never sent.
    Note {
        key: String,
        /// The note; `-` reads stdin.
        text: String,
    },
    /// Queue a comment.
    Comment {
        key: String,
        /// The comment; `-` reads stdin.
        text: String,
    },
    /// Queue a new issue.
    Create(CreateArgs),
    /// Queue a new title, body, state, labels or assignees.
    Edit(EditArgs),
    /// Send queued changes; explicit `push = false` denies a tracker.
    Push {
        /// Show the requests without sending anything.
        #[arg(long)]
        dry_run: bool,
        /// Send local values that conflict with a newer remote value.
        #[arg(long)]
        force: bool,
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

#[derive(Clone, Copy, ValueEnum)]
pub enum State {
    Open,
    Closed,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Reason {
    Completed,
    NotPlanned,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::NotPlanned => "not_planned",
        }
    }
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
    /// With `--pr`: also queue the comment `Linked: <url>`, once per PR/MR.
    #[arg(long)]
    pub comment: bool,
}

#[derive(Args)]
pub struct CreateArgs {
    #[arg(long)]
    pub title: String,
    /// The body; `-` reads stdin.
    #[arg(long, default_value = "")]
    pub body: String,
    #[arg(long = "label")]
    pub labels: Vec<String>,
    #[arg(long = "assignee")]
    pub assignees: Vec<String>,
}

#[derive(Args)]
pub struct EditArgs {
    pub key: String,
    #[arg(long)]
    pub title: Option<String>,
    /// The body; `-` reads stdin.
    #[arg(long)]
    pub body: Option<String>,
    #[arg(long, value_enum)]
    pub state: Option<State>,
    /// Why it closed (GitHub only); needs `--state closed`.
    #[arg(long, value_enum, requires = "state")]
    pub reason: Option<Reason>,
    #[arg(long = "add-label", value_name = "LABEL")]
    pub add_labels: Vec<String>,
    #[arg(long = "remove-label", value_name = "LABEL")]
    pub remove_labels: Vec<String>,
    /// Removals apply first, so one call can swap a single assignee.
    #[arg(long = "add-assignee", value_name = "LOGIN")]
    pub add_assignees: Vec<String>,
    #[arg(long = "remove-assignee", value_name = "LOGIN")]
    pub remove_assignees: Vec<String>,
}
