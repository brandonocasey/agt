use std::path::PathBuf;

use clap::{ArgGroup, Args, Subcommand, ValueEnum};

use crate::store::model::{CheckKind, Outcome, PassKind, Severity, Side, ThreadType, Triage};

#[derive(Args)]
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
    /// End a review run (a run starts with `pass start --kind full`).
    Run {
        #[command(subcommand)]
        action: RunAction,
    },
    /// Start or end a review pass inside a run.
    Pass {
        #[command(subcommand)]
        action: PassAction,
    },
    /// Move a reviewer finding to the killed list.
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
    /// Report what is reviewed at a head. Never writes, unless `--sync` imports the PR/MR first.
    Status {
        /// Commit to evaluate instead of the current head and work tree.
        #[arg(long)]
        head: Option<String>,
        /// Also print every commit, run and imported record.
        #[arg(long)]
        full: bool,
        #[command(flatten)]
        sync: SyncFirst,
    },
    /// Import the PR/MR of this branch (or `--pr`) from GitHub or GitLab. Read-only on the forge.
    Sync {
        /// PR/MR number or URL [default: the PR/MR whose head is the current branch].
        #[arg(long)]
        pr: Option<String>,
    },
    /// Post local findings, replies, resolves and final records to an own PR/MR. Needs
    /// `[forge] post = true` in the configuration.
    Publish {
        /// Print every payload without posting anything. With `--approve` the approval is still
        /// recorded locally.
        #[arg(long)]
        dry_run: bool,
        /// Allow a PR/MR authored by someone else; `--only` then names the threads to post.
        #[arg(long)]
        not_own: bool,
        /// Post only these thread ids (comma-separated) and their replies and resolves.
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// PR/MR number or URL [default: the PR/MR whose head is the current branch].
        #[arg(long)]
        pr: Option<String>,
        #[command(flatten)]
        approval: ApprovalArgs,
    },
}

/// Import the PR/MR before a read, so the report is not stale.
#[derive(Args)]
pub struct SyncFirst {
    /// Import the PR/MR of this branch (or `--pr`) first, as `sync` does.
    #[arg(long)]
    pub sync: bool,
    /// With `--sync`, the PR/MR number or URL [default: the PR/MR whose head is the branch].
    #[arg(long, requires = "sync", value_name = "PR")]
    pub pr: Option<String>,
}

#[derive(Subcommand)]
pub enum RunAction {
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
        /// What the main session spent, as a JSON array of `{"role", "tokens",
        /// "duration_ms", "tool_uses"}` (roles such as `fix`, `manual`, `main`), from a file
        /// or `-` for stdin.
        #[arg(long, value_name = "FILE")]
        usage: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum PassAction {
    /// Start a pass and record what it reviews. A full pass without `--run` starts the run too.
    Start {
        /// Run to add the pass to [default for `--kind full`: a new run].
        #[arg(long)]
        run: Option<String>,
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
        /// Add these findings first: JSON from a file or `-` for stdin, an array or
        /// `{"findings": [...], "usage": [...]}`; each finding names its `reviewer` or takes
        /// `--reviewer`.
        #[arg(long, value_name = "FILE")]
        findings: Option<PathBuf>,
        /// Reviewer of findings that name none.
        #[arg(long, requires = "findings")]
        reviewer: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum FindingAction {
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
    /// Stored reviewer as `<run>.<pass>.<reviewer>`, such as `rv-7k2m.c1p0.r1`; fills model,
    /// effort, harness and session from the stored pass.
    #[arg(long, value_name = "ID")]
    pub reviewer: Option<String>,
    /// The harness's id for the agent author, such as the main session's id; posted comments
    /// name its first 8 characters.
    #[arg(long)]
    pub session: Option<String>,
}

impl AuthorArgs {
    /// Whether any author flag is given.
    pub fn is_set(&self) -> bool {
        self.author.is_some()
            || self.model.is_some()
            || self.effort.is_some()
            || self.harness.is_some()
            || self.harness_version.is_some()
            || self.reviewer.is_some()
            || self.session.is_some()
    }
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
    /// A related issue on the host (`owner/repo#12`, `#12` or `12`) the posted text names.
    #[arg(long, value_name = "ISSUE")]
    pub issue: Option<String>,
    #[command(flatten)]
    pub author: AuthorArgs,
}

#[derive(Args)]
pub struct QaCommentArgs {
    #[command(flatten)]
    pub comment: CommentArgs,
    /// Check the comment is about, as `qa-xxxx.k1`.
    #[arg(long, value_name = "CHECK")]
    pub check: Option<String>,
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
#[command(group = ArgGroup::new("outcome")
    .args(["fixed", "not_fixed", "rejected", "stale", "answered", "issue", "triage"]))]
pub struct ResolveArgs {
    /// Threads to change, all in one write.
    #[arg(required = true)]
    pub ids: Vec<String>,
    /// Fixed in this commit: triage `fixed`, resolution `fixed <sha>`, and a reply
    /// "✅ Fixed in <sha> <subject>." linking the commit unless `--reply` gives other text.
    #[arg(long, value_name = "SHA")]
    pub fixed: Option<String>,
    /// A fix check found it not fixed: reopen with `not fixed: <reason>` and a reply
    /// "❌ Not fixed: <reason>." unless `--reply` gives other text.
    #[arg(long, value_name = "REASON")]
    pub not_fixed: Option<String>,
    /// Won't fix (wrong, or not worth it): resolve with `rejected: <reason>` and a
    /// reply "🚫 Won't fix: <reason>." unless `--reply` gives other text.
    #[arg(long, value_name = "REASON")]
    pub rejected: Option<String>,
    /// Left for a follow-up issue on the host (`owner/repo#12`, `#12` or `12`): resolve with
    /// `moved to <issue>` and a reply "📋 Moved to <issue>." unless `--reply` gives other text.
    #[arg(long, value_name = "ISSUE")]
    pub issue: Option<String>,
    /// The code the thread names is gone: triage `stale`, resolve, and reply
    /// "➖ No longer applies: the code changed." unless `--reply` gives other text.
    #[arg(long)]
    pub stale: bool,
    /// Resolve as answered, with the reply "💬 Answered above." unless `--reply` gives other text.
    #[arg(long)]
    pub answered: bool,
    /// Only record the triage: `valid` or `question` (a question holds the thread).
    #[arg(long, value_enum)]
    pub triage: Option<Verdict>,
    /// Also add this reply to each thread, or `-` for stdin.
    #[arg(long)]
    pub reply: Option<String>,
    #[command(flatten)]
    pub author: AuthorArgs,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Verdict {
    Valid,
    Question,
}

impl From<Verdict> for Triage {
    fn from(verdict: Verdict) -> Self {
        match verdict {
            Verdict::Valid => Self::Valid,
            Verdict::Question => Self::Question,
        }
    }
}

/// `publish --approve`: record an approval of the head, then publish it with the rest.
#[derive(Args)]
pub struct ApprovalArgs {
    /// Approve the head first: record an approval (reused for the same head), then publish.
    /// It posts no comment, except as `--formal` says; the summary headline shows it. Refused
    /// while a bug, requirement or question is open.
    #[arg(long)]
    pub approve: bool,
    /// With `--approve`, also submit the host's approval (GitHub APPROVE review, GitLab approve).
    /// On your own GitHub PR, which GitHub will not let you approve, post a COMMENT review with
    /// the approval text instead; when GitLab will not let you approve, post the text as a comment.
    #[arg(long, requires = "approve")]
    pub formal: bool,
    /// With `--approve`, the approval text kept in the store and posted only in that COMMENT
    /// review or comment, or `-` for stdin [default: "Approved at <sha>."].
    #[arg(long, requires = "approve")]
    pub body: Option<String>,
    /// With `--approve`: who approves.
    #[command(flatten)]
    pub author: AuthorArgs,
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
    #[command(flatten)]
    pub sync: SyncFirst,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Format {
    Json,
    Prompt,
}

/// `agt qa`: what was tested, where, and what came up, stored with the review threads.
#[derive(Args)]
pub struct QaCli {
    /// Use this stored target key instead of the current branch.
    #[arg(long, global = true, conflicts_with = "range")]
    pub target: Option<String>,
    /// Test a commit (`<rev>`) or a range (`<from>..<to>`) instead of the current branch.
    #[arg(long, global = true)]
    pub range: Option<String>,
    /// Branch the merge-base is taken against [default: origin/HEAD, then main or master].
    #[arg(long, global = true, value_name = "REF")]
    pub default_branch: Option<String>,
    #[command(subcommand)]
    pub command: QaCommand,
}

#[derive(Subcommand)]
pub enum QaCommand {
    /// Start or end a QA run.
    Run {
        #[command(subcommand)]
        action: QaRunAction,
    },
    /// Record one tested scenario and its outcome at the current state.
    Check(CheckArgs),
    /// Open a thread for something a check found.
    Finding(QaFindingArgs),
    /// Add a QA comment, as `agt review comment` does; `--check` ties it to a check.
    Comment(QaCommentArgs),
    /// Reply to a thread as QA, as `agt review reply` does.
    Reply(ReplyArgs),
    /// List the latest outcome of every check and the QA threads.
    List,
    /// Show a QA run (`qa-xxxx`), check (`qa-xxxx.k1`) or thread.
    Show { id: String },
    /// Report the latest outcome of each check at a head and whether QA can approve. Never writes.
    Status {
        /// Commit to evaluate instead of the current head and work tree.
        #[arg(long)]
        head: Option<String>,
    },
    /// Approve the head for QA. Refused with no checks, a failing, blocked or stale check, or an
    /// open bug, requirement or question found by a check. Sends no host approval.
    Approve,
    /// Post local QA and review items to an own PR/MR, as `agt review publish` does.
    Publish {
        /// Print every payload without posting anything.
        #[arg(long)]
        dry_run: bool,
        /// Allow a PR/MR authored by someone else; `--only` then names the threads to post. The
        /// summary, with its QA section, is never posted there.
        #[arg(long)]
        not_own: bool,
        /// Post only these thread ids (comma-separated) and their replies and resolves.
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// PR/MR number or URL [default: the PR/MR whose head is the current branch].
        #[arg(long)]
        pr: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum QaRunAction {
    /// Start a QA run; prints its id.
    Start {
        /// Environment fact every check of the run shares, as `key=value` (repeatable).
        #[arg(long, value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Who tests [default: git user.name].
        #[command(flatten)]
        tester: AuthorArgs,
    },
    /// End a QA run.
    End {
        #[arg(long)]
        run: String,
        /// What the session spent, as a JSON array of `{"role", "tokens", "duration_ms",
        /// "tool_uses"}`, from a file or `-` for stdin.
        #[arg(long, value_name = "FILE")]
        usage: Option<PathBuf>,
    },
}

#[derive(Args)]
pub struct CheckArgs {
    #[arg(long)]
    pub run: String,
    /// What was tested, such as `login with an expired password`. A later check of the same
    /// name replaces this one.
    #[arg(long)]
    pub name: String,
    #[arg(long, value_enum)]
    pub outcome: Outcome,
    #[arg(long, value_enum, default_value = "manual")]
    pub kind: CheckKind,
    /// The steps taken, or `-` for stdin.
    #[arg(long)]
    pub steps: Option<String>,
    /// The command an automated check ran.
    #[arg(long)]
    pub command: Option<String>,
    /// Environment fact that adds to or overrides the run's, as `key=value` (repeatable).
    #[arg(long, value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// Screenshot or log path, or URL (repeatable). Only URLs are posted.
    #[arg(long)]
    pub evidence: Vec<String>,
    #[arg(long)]
    pub note: Option<String>,
}

#[derive(Args)]
pub struct QaFindingArgs {
    /// The check that found it, such as `qa-7k2m.k1`.
    #[arg(long)]
    pub check: String,
    #[arg(long = "type", value_enum)]
    pub kind: ThreadType,
    /// Required for a bug.
    #[arg(long, value_enum)]
    pub severity: Option<Severity>,
    /// What happened, or `-` for stdin.
    #[arg(long)]
    pub body: String,
    /// A related issue on the host (`owner/repo#12`, `#12` or `12`) the posted text names.
    #[arg(long, value_name = "ISSUE")]
    pub issue: Option<String>,
    /// Who found it [default: the run's tester].
    #[command(flatten)]
    pub author: AuthorArgs,
}
