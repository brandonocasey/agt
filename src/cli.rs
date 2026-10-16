use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::model::{PassKind, Severity, Side, ThreadType, Triage};

/// Local PR/MR-style review threads for coding agents.
#[derive(Parser)]
#[command(version, about)]
pub struct Cli {
    /// Use this stored target key instead of the current branch.
    #[arg(long, global = true, conflicts_with = "range")]
    pub target: Option<String>,
    /// Review a commit (`<rev>`) or a range (`<from>..<to>`) instead of the current branch.
    #[arg(long, global = true)]
    pub range: Option<String>,
    /// Branch the merge-base is taken against [default: origin/HEAD, then main or master].
    #[arg(long, global = true, value_name = "REF")]
    pub default_branch: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start or end a review run.
    Run {
        #[command(subcommand)]
        action: RunAction,
    },
    /// Start or end a review pass inside a run.
    Pass {
        #[command(subcommand)]
        action: PassAction,
    },
    /// Record reviewer findings.
    Finding {
        #[command(subcommand)]
        action: FindingAction,
    },
    /// Open a new thread.
    Comment(CommentArgs),
    /// Add a comment to a thread.
    Reply(ReplyArgs),
    /// Resolve, triage or reopen a thread.
    Resolve(ResolveArgs),
    /// List threads and killed findings.
    List(ListArgs),
    /// Show a thread, killed finding, run (`rv-xxxx`) or pass (`rv-xxxx.c1p0`).
    Show { id: String },
    /// Report what is reviewed at a head. Never writes.
    Status {
        /// Commit to evaluate instead of the current head and work tree.
        #[arg(long)]
        head: Option<String>,
    },
    /// Import the PR/MR of this branch (or `--pr`) from GitHub or GitLab. Read-only on the forge.
    Sync {
        /// PR/MR number or URL [default: the PR/MR whose head is the current branch].
        #[arg(long)]
        pr: Option<String>,
    },
    /// Post local findings, replies, resolves and final records to an own PR/MR.
    Publish {
        /// Print every payload without posting or writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Allow a PR/MR authored by someone else.
        #[arg(long)]
        not_own: bool,
        /// With `--not-own`, also post our findings and local threads (never final records).
        #[arg(long, requires = "not_own")]
        findings: bool,
        /// Post only these thread ids (comma-separated) and their replies and resolves.
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// PR/MR number or URL [default: the PR/MR whose head is the current branch].
        #[arg(long)]
        pr: Option<String>,
    },
    /// Drop targets 90 days after their PR/MR closed, else 90 days after their last change.
    Prune {
        /// Prune every repository in the store, not only the current one.
        #[arg(long)]
        all: bool,
        /// Print what would be pruned without deleting.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub enum RunAction {
    /// Start a run and print its id.
    Start,
    /// End a run, record patch-ids for committed work and prune old targets.
    End {
        #[arg(long)]
        run: String,
        /// Reveal findings the blind reviewers agreed with, for the final record.
        #[arg(long, requires = "reveal_disagree")]
        reveal_agree: Option<u32>,
        /// Reveal findings the blind reviewers disagreed with, for the final record.
        #[arg(long, requires = "reveal_agree")]
        reveal_disagree: Option<u32>,
        /// Repaired head of a range or commit target: a descendant of its end [default: the end].
        #[arg(long)]
        head: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum PassAction {
    /// Start a pass and record what it reviews.
    Start {
        #[arg(long)]
        run: String,
        #[arg(long, value_enum)]
        kind: PassKind,
        /// Start of the reviewed delta [default: merge-base for full, previous pass head otherwise].
        #[arg(long)]
        base: Option<String>,
        /// JSON array of reviewers, from a file or `-` for stdin.
        #[arg(long, value_name = "FILE")]
        reviewers: Option<PathBuf>,
        /// Reviewed head of a range or commit target, such as a fix: a descendant of its end
        /// [default: the end].
        #[arg(long)]
        head: Option<String>,
    },
    /// End a pass.
    End {
        #[arg(long)]
        run: String,
        #[arg(long)]
        pass: String,
        /// Freeze the pass's findings (blind review lock).
        #[arg(long)]
        lock: bool,
    },
}

#[derive(Subcommand)]
pub enum FindingAction {
    /// Add verified and killed findings from JSON on stdin.
    Add {
        #[arg(long)]
        run: String,
        #[arg(long)]
        pass: String,
        #[arg(long)]
        reviewer: String,
    },
    /// Move a surviving finding to its pass's killed list (allowed on a locked pass).
    Kill {
        id: String,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Args)]
pub struct AuthorArgs {
    /// Human author login [default: git user.name].
    #[arg(long)]
    pub author: Option<String>,
    /// Agent model id.
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub effort: Option<String>,
    /// Agent harness, such as `claude-code` or `codex`.
    #[arg(long)]
    pub harness: Option<String>,
    #[arg(long)]
    pub harness_version: Option<String>,
    /// Run the agent author belongs to.
    #[arg(long)]
    pub run: Option<String>,
    /// Pass the agent author belongs to.
    #[arg(long)]
    pub pass: Option<String>,
    /// Reviewer id; fills model, effort and harness from the stored pass.
    #[arg(long)]
    pub reviewer: Option<String>,
}

#[derive(Args)]
pub struct CommentArgs {
    /// Comment text, or `-` for stdin.
    #[arg(long)]
    pub body: String,
    #[arg(long = "type", value_enum, default_value = "comment")]
    pub kind: ThreadType,
    #[arg(long, value_enum)]
    pub severity: Option<Severity>,
    /// File the thread is anchored to; omit for a general comment.
    #[arg(long)]
    pub path: Option<String>,
    #[arg(long)]
    pub line: Option<u32>,
    #[arg(long)]
    pub old_line: Option<u32>,
    #[arg(long, value_enum)]
    pub side: Option<Side>,
    #[arg(long)]
    pub start_line: Option<u32>,
    /// Commit the line numbers refer to [default: HEAD, or the work tree when dirty].
    #[arg(long)]
    pub commit: Option<String>,
    #[command(flatten)]
    pub author: AuthorArgs,
}

#[derive(Args)]
pub struct ReplyArgs {
    pub id: String,
    /// Reply text, or `-` for stdin.
    #[arg(long)]
    pub body: String,
    /// Keep the reply in the store only; `publish` never posts it.
    #[arg(long)]
    pub local: bool,
    #[command(flatten)]
    pub author: AuthorArgs,
}

#[derive(Args)]
pub struct ResolveArgs {
    pub id: String,
    /// `fixed <sha>`, `not fixed: <reason>`, `stale` or `answered`.
    #[arg(long)]
    pub resolution: Option<String>,
    /// `fixed` and `stale` resolve; `valid` and `question` only record the triage.
    #[arg(long, value_enum)]
    pub triage: Option<Triage>,
    /// Mark the thread unresolved again.
    #[arg(long)]
    pub reopen: bool,
}

#[derive(Args)]
pub struct ListArgs {
    /// Only open threads, as `status` counts them: unresolved, still on the host, and not
    /// another person's general comment.
    #[arg(long)]
    pub open: bool,
    /// Only findings of this run.
    #[arg(long)]
    pub run: Option<String>,
    /// Only findings of this cycle of `--run`.
    #[arg(long, requires = "run")]
    pub cycle: Option<u32>,
    /// Map anchors to this commit, such as a fix after a range's end [default: the target's
    /// head].
    #[arg(long)]
    pub head: Option<String>,
    #[arg(long, value_enum, default_value = "json")]
    pub format: Format,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Format {
    Json,
    Prompt,
}
