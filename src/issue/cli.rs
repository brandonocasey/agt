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
    /// Pull what changed on every issue tracker (each with its `scope`, `involved` by default),
    /// then push queued changes to each tracker whose `push` is `manual` or `auto`. With `push`
    /// `off`, the default, only the pull happens and the changes stay local.
    Sync,
    /// List the local issues.
    List(ListArgs),
    /// Show one issue with its comments, links and queued changes.
    Show {
        /// `owner/repo#12`, `#12`, `12` or `local-xxxx`.
        key: String,
    },
    /// Per tracker: counts, queued changes, conflicts and errors. Never writes.
    Status,
    /// Link an issue to a PR/MR or a branch, locally; `--pr --comment` also queues a comment
    /// naming the PR/MR.
    Link(LinkArgs),
    /// Queue a comment, or with `--private` keep a note that is never sent.
    Comment {
        key: String,
        /// The comment; `-` reads stdin.
        text: String,
        /// Keep it as a local note on the issue instead of queueing it.
        #[arg(long)]
        private: bool,
    },
    /// Queue a new issue.
    Create(CreateArgs),
    /// Queue changes to issue fields or its parent.
    Edit(EditArgs),
    /// Drop every queued, unpushed change of one issue; a never-pushed issue is removed. Local only.
    Discard {
        /// `owner/repo#12`, `#12`, `12` or `local-xxxx`.
        key: String,
    },
    /// Only push: send queued changes to each tracker whose `push` is `manual` or `auto`
    /// (`[issues]`, `[sync]` or the `[[tracker]]`); with `off`, the default, they stay local.
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
    /// Default `open`, or `all` with `--text`, `--pr`, `--branch`, `--title` or `--parent`.
    #[arg(long, value_enum)]
    pub state: Option<StateFilter>,
    #[arg(long)]
    pub label: Option<String>,
    #[arg(long)]
    pub assignee: Option<String>,
    #[arg(long)]
    pub author: Option<String>,
    /// Only direct children of this issue; includes closed children unless --state says otherwise.
    #[arg(long, value_name = "KEY")]
    pub parent: Option<String>,
    /// Only issues with queued changes.
    #[arg(long)]
    pub pending: bool,
    /// Only issues whose title, body, comments or notes contain the text; adds `matched`.
    #[arg(long)]
    pub text: Option<String>,
    /// Only issues linked to this PR/MR number or URL; adds `via`. With `--branch` or
    /// `--title`, an issue that matches any of the three is kept.
    #[arg(long)]
    pub pr: Option<String>,
    /// Only issues tied to this branch name; adds `via`.
    #[arg(long)]
    pub branch: Option<String>,
    /// The PR/MR title: only issues its references such as `#12` name; adds `via`.
    #[arg(long)]
    pub title: Option<String>,
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
pub struct LinkArgs {
    pub key: String,
    #[arg(long, group = "target")]
    pub pr: Option<String>,
    #[arg(long, group = "target")]
    pub branch: Option<String>,
    /// With `--pr`: also queue the comment `Linked: <url>`, once per PR/MR.
    #[arg(long)]
    pub comment: bool,
}

#[derive(Args)]
pub struct CreateArgs {
    /// Create a child of this issue, in the same tracker. GitLab children become Tasks.
    #[arg(long, value_name = "KEY")]
    pub parent: Option<String>,
    #[arg(long)]
    pub title: String,
    /// The body; `-` reads stdin.
    #[arg(long, default_value = "")]
    pub body: String,
    #[command(flatten)]
    pub lists: Lists,
}

/// The label and assignee additions `create` and `edit` share.
#[derive(Args)]
pub struct Lists {
    #[arg(long = "add-label", alias = "label", value_name = "LABEL")]
    pub add_labels: Vec<String>,
    #[arg(long = "add-assignee", alias = "assignee", value_name = "LOGIN")]
    pub add_assignees: Vec<String>,
}

#[derive(Args)]
pub struct EditArgs {
    pub key: String,
    /// Set or replace the parent issue, in the same tracker.
    #[arg(long, value_name = "KEY", conflicts_with = "remove_parent")]
    pub parent: Option<String>,
    /// Remove the parent relationship.
    #[arg(long)]
    pub remove_parent: bool,
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
    #[command(flatten)]
    pub lists: Lists,
    #[arg(long = "remove-label", value_name = "LABEL")]
    pub remove_labels: Vec<String>,
    /// Removals apply first, so one call can swap a single assignee.
    #[arg(long = "remove-assignee", value_name = "LOGIN")]
    pub remove_assignees: Vec<String>,
}
