//! `agt pr`: one GitHub/GitLab/Forgejo-neutral command for the PR/MR of the current repository:
//! view, list, create, update, merge behind a head-sha guard, pause or restore auto-merge, and
//! upload images for a body. The forge comes from the tracker of the origin remote, or the one
//! `--remote` or a PR/MR URL names; writes are refused where `push` is `off`. With `--local`
//! the PR is a local branch and its merge moves the local default branch (`local.rs`). Output
//! is JSON.

mod forgejo;
mod gates;
mod github;
mod gitlab;
mod intent;
mod local;
mod memo;
mod provider;
mod remote;
mod upload;

use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};

use self::local::Local;
use self::provider::{At, Filter, NewPr, Outcome, Provider};
use self::remote::Remote;
use crate::core::config::Config;
use crate::core::error::{Error, Result, fail};
use crate::core::output::{self, Output, TIMED_OUT};
use crate::forge::auto::Offline;
use crate::forge::parse_pr;
use crate::git::Repo;
use crate::store::model::{Forge, ReviewState};

pub use self::gates::{Gates, Review};

/// View, list, create, update and merge the PRs/MRs of this repository, on GitHub, GitLab or
/// Forgejo; with `--local`, merge local branches into the local default branch.
#[derive(Args)]
pub struct Cli {
    /// Tracker to use: a git remote name or `host/repo` [default: the repository of a PR/MR
    /// URL, else the tracker of `origin`].
    #[arg(long, global = true)]
    remote: Option<String>,
    /// A local branch is the PR, addressed by its name and merged into the local default branch;
    /// no tracker, network or push is involved. Never chosen automatically. `view` and `list`
    /// read local refs, `merge` takes `--method rebase` or none (fast-forward only) and never
    /// deletes a branch or work tree; `create`, `update` and `upload` have nothing to store.
    #[arg(long, global = true, conflicts_with = "remote")]
    local: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum How {
    Merge,
    Squash,
    Rebase,
}

impl How {
    fn named(name: &str) -> Option<Self> {
        Self::from_str(name, true).ok()
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub enum State {
    Open,
    Merged,
    All,
}

#[derive(Clone, Copy, ValueEnum)]
enum Switch {
    On,
    Off,
}

#[derive(Subcommand)]
enum Command {
    /// One PR/MR as JSON: number, url, title, state (open, merged, closed), draft, head and base
    /// branch, head sha, author, labels, auto_merge (on, off, or unknown where the forge does not say) with its method, merge
    /// queue or train state, mergeable and conflicts.
    ///
    /// `mergeable` is mergeable, conflicting, blocked (checks, approvals, discussions, draft or
    /// behind: see `merge_state`) or unknown (the forge has not computed it yet).
    ///
    /// <NUMBER> is 12, #12, !12 or a PR/MR URL; without it, the PR/MR of --branch or of the
    /// current branch (an open one first, else the newest).
    View {
        number: Option<String>,
        /// Head branch to look up instead of the current branch.
        #[arg(long, conflicts_with = "number")]
        branch: Option<String>,
    },
    /// PRs/MRs as a JSON array of the `view` fields, newest first (at most 100).
    List {
        /// Only those you authored.
        #[arg(long)]
        mine: bool,
        /// Only this head branch.
        #[arg(long)]
        branch: Option<String>,
        #[arg(long, value_enum, default_value = "open")]
        state: State,
    },
    /// Open a PR/MR for the current branch, which must be pushed. Fails, printing it, when one
    /// is open already.
    Create {
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        body: String,
        /// Target branch; default: the repository's default branch.
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        draft: bool,
    },
    /// Change a PR/MR's title, body, labels or draft state; prints it afterwards.
    Update {
        number: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long = "add-label", value_name = "LABEL")]
        add_label: Vec<String>,
        #[arg(long = "remove-label", value_name = "LABEL")]
        remove_label: Vec<String>,
        /// Convert to a draft; `--draft=false` marks it ready for review.
        #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true", value_name = "BOOL")]
        draft: Option<bool>,
    },
    /// Host images on the forge and print JSON with each one's `file`, `url` and `markdown`;
    /// paste the Markdown into the body of `create` or `update`.
    ///
    /// Takes png, jpg, jpeg, gif, webp and svg files. GitHub (github.com only) needs a user
    /// token, not an app or installation token. The images belong to the repository, not to a
    /// PR/MR. Forgejo is refused: it attaches an image only to an existing issue or comment.
    Upload {
        #[arg(required = true, value_name = "FILE")]
        files: Vec<PathBuf>,
    },
    /// Merge only while the head is --sha (the forge enforces it too). Never deletes the branch
    /// or bypasses protections.
    ///
    /// With --auto, merge when checks pass; GitHub needs --method unless auto-merge was on, and
    /// merges a PR that can merge now at once, as gh does, unless its base requires a merge
    /// queue (then auto-merge queues it).
    /// Without --auto, a GitHub PR whose base requires a merge queue is added to the queue
    /// through auto-merge, so it needs --method too. GitLab cannot rebase on merge.
    ///
    /// With --local, <NUMBER> is the branch name. Without --auto it merges at once; with --auto
    /// it records an intent and polls every 5 seconds for up to 270 seconds until the gates pass
    /// at the head: an `agt check run` pass for the tree, a review with no unreviewed commit and
    /// no open blocking finding, an approval at exactly the head, and a branch that lands. A
    /// gate that still fails exits 3, naming it, and keeps the intent.
    ///
    /// When this call merges the PR/MR, each local issue that its title or body names after a
    /// closing keyword (`Fixes local-ab12cd`; one keyword per key) is closed and listed as
    /// `closed_issues`. Auto-merge that merges later closes nothing.
    Merge {
        number: String,
        /// The head commit reviewed; the merge is refused when the PR/MR head differs.
        #[arg(long)]
        sha: String,
        #[arg(long, value_enum)]
        method: Option<How>,
        #[arg(long)]
        auto: bool,
    },
    /// Pause or restore auto-merge. `off` records the method it disables; `on` re-enables it
    /// with the same method (state: `agents/pr/auto-merge.json`). A queued PR/MR is refused.
    ///
    /// With --local, `off` clears the intent of the branch and stops the call waiting on it
    /// (state: `agents/pr/local-auto-merge.json`); `on` waits on the gates as `merge --auto`
    /// does and exits 3 when one still fails.
    AutoMerge {
        #[arg(value_enum)]
        switch: Switch,
        number: String,
        /// For `on`: the head to merge; default: the current head.
        #[arg(long)]
        sha: Option<String>,
    },
}

pub struct Changes {
    pub title: Option<String>,
    pub body: Option<String>,
    pub add_labels: Vec<String>,
    pub remove_labels: Vec<String>,
    /// `Some(true)` converts to a draft, `Some(false)` marks it ready.
    pub draft: Option<bool>,
}

impl Changes {
    /// The labels `current` has after the removals and then the additions, each once; `None`
    /// when there is nothing to add or remove.
    pub fn merged_labels<'a>(&'a self, current: &'a [String]) -> Option<Vec<&'a String>> {
        if self.add_labels.is_empty() && self.remove_labels.is_empty() {
            return None;
        }
        let mut labels: Vec<&String> = current
            .iter()
            .filter(|label| !self.remove_labels.contains(label))
            .collect();
        for label in &self.add_labels {
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        Some(labels)
    }
}

/// What a provider keeps from reading a PR/MR to merge it later; the shared flow never looks
/// inside.
pub enum Native {
    Github {
        /// The base branch requires a merge queue.
        merge_queue: bool,
    },
    Gitlab {
        /// The MR squashes on merge.
        squash: bool,
    },
    /// The host keeps nothing: Forgejo, and the local provider.
    None,
}

/// A PR/MR as every subcommand reports it.
pub struct Pr {
    /// The forge's node id: GitHub's GraphQL id, GitLab's iid.
    pub id: String,
    pub number: Option<u64>,
    pub url: Option<String>,
    pub title: String,
    /// Not part of the JSON output; `merge` hands it on so issues it fixes can close.
    pub body: String,
    pub state: ReviewState,
    pub draft: bool,
    pub head_branch: String,
    pub base_branch: String,
    pub head_sha: String,
    pub author: String,
    pub labels: Vec<String>,
    /// The method auto-merge will use, when it is on.
    pub auto_merge: Option<String>,
    /// The host says whether auto-merge is on; Forgejo does not, so `auto_merge` is `None`
    /// whatever its state.
    pub auto_merge_known: bool,
    /// The merge queue or train state, when queued.
    pub queue: Option<String>,
    pub mergeable: String,
    pub merge_state: String,
    pub conflicts: bool,
    /// The head branch lives in this repository (not a fork).
    pub same_repo: bool,
    pub native: Native,
}

impl Pr {
    /// The number a forge request names; a forge PR/MR always has one.
    fn forge_number(&self) -> u64 {
        self.number.unwrap_or_default()
    }

    fn at(&self) -> At {
        match self.number {
            Some(number) => At::Number(number),
            None => At::Branch(self.head_branch.clone()),
        }
    }

    fn json(&self) -> Value {
        json!({
            "number": self.number,
            "url": self.url,
            "title": self.title,
            "state": self.state.as_str(),
            "draft": self.draft,
            "head_branch": self.head_branch,
            "base_branch": self.base_branch,
            "head_sha": self.head_sha,
            "author": self.author,
            "labels": self.labels,
            "auto_merge": match (self.auto_merge_known, self.auto_merge.is_some()) {
                (false, _) => "unknown",
                (true, true) => "on",
                (true, false) => "off",
            },
            "auto_merge_method": self.auto_merge,
            "queue": self.queue,
            "mergeable": self.mergeable,
            "merge_state": self.merge_state,
            "conflicts": self.conflicts,
        })
    }
}

/// The local provider for `--local`, else the provider of the forge that the selected tracker
/// names.
fn provider<'a>(repo: &'a Repo, cli: &Cli, gates: &'a dyn Gates) -> Result<Box<dyn Provider + 'a>> {
    if cli.local {
        return Ok(Box::new(Local::new(repo, gates)?));
    }
    let config = Config::load_for_repo(repo)?;
    let given = match &cli.command {
        Command::Update { number, .. }
        | Command::Merge { number, .. }
        | Command::AutoMerge { number, .. } => Some(number.as_str()),
        Command::View { number, .. } => number.as_deref(),
        Command::List { .. } | Command::Create { .. } | Command::Upload { .. } => None,
    };
    let pr_ref = given.map(parse_pr).transpose()?;
    let remote = Remote::connect(repo, &config, cli.remote.as_deref(), pr_ref.as_ref())?;
    Ok(match remote.origin.forge {
        Forge::Github => Box::new(github::Github(remote)),
        Forge::Gitlab => Box::new(gitlab::Gitlab(remote)),
        Forge::Forgejo => Box::new(forgejo::Forgejo(remote)),
    })
}

fn branch_of(repo: &Repo, branch: Option<String>) -> Result<String> {
    match branch {
        Some(branch) => Ok(branch),
        None => repo
            .current_branch()?
            .ok_or_else(|| Error("HEAD is detached; pass --branch or a number".into())),
    }
}

/// Fails unless `pr` is open and its head is `sha`.
fn guard(pr: &Pr, sha: &str) -> Result<()> {
    if pr.state != ReviewState::Open {
        return fail(format!("{} is {}, not open", pr.at(), pr.state.as_str()));
    }
    if sha.len() < 7 || !pr.head_sha.starts_with(sha) {
        return fail(format!(
            "{} head is {}, not {sha}; inspect it again before merging",
            pr.at(),
            pr.head_sha
        ));
    }
    Ok(())
}

fn not_queued(pr: &Pr) -> Result<()> {
    match &pr.queue {
        Some(queue) => fail(format!(
            "{} is in a merge queue or train ({queue}); leave it untouched and do not push to its branch",
            pr.at()
        )),
        None => Ok(()),
    }
}

/// Runs the command; `gates` answers what the local auto-merge asks of other domains. Also
/// returns the title and body of a PR/MR that this call merged.
pub fn run(cli: Cli, gates: &dyn Gates) -> Result<(Output, Option<String>)> {
    if !cli.local {
        Offline::current().forbid("agt pr")?;
    }
    let repo = Repo::discover()?;
    let provider = provider(&repo, &cli, gates)?;
    let mut merged_text = None;
    let output = execute(&*provider, &repo, cli.command, &mut merged_text)
        .map_err(|error| provider.explain(error))?;
    Ok((output, merged_text))
}

fn execute(
    provider: &dyn Provider,
    repo: &Repo,
    command: Command,
    merged_text: &mut Option<String>,
) -> Result<Output> {
    let shown = |pr: Pr| Ok(Output::Json(pr.json()));
    match command {
        Command::View {
            number: given,
            branch,
        } => {
            if let Some(given) = given {
                return shown(provider.view(&provider.at(&given)?)?);
            }
            let branch = branch_of(repo, branch)?;
            let Some(found) = provider.of_branch(&branch)? else {
                return fail(format!(
                    "no PR/MR for branch {branch} in {}",
                    provider.place()
                ));
            };
            shown(provider.view(&found.at())?)
        }
        Command::List {
            mine,
            branch,
            state,
        } => Ok(Output::Json(Value::Array(
            provider
                .list(&Filter {
                    branch: branch.as_deref(),
                    state,
                    mine,
                })?
                .iter()
                .map(Pr::json)
                .collect(),
        ))),
        Command::Create {
            title,
            body,
            base,
            draft,
        } => {
            provider.allow_write("agt pr create")?;
            no_text(provider, "create")?;
            let branch = branch_of(repo, None)?;
            if let Some(open) = provider
                .list(&Filter {
                    branch: Some(&branch),
                    state: State::Open,
                    mine: false,
                })?
                .into_iter()
                .find(|pr| pr.same_repo)
            {
                output::print(Output::Json(open.json()))?;
                return fail(format!(
                    "branch {branch} already has {}: {}",
                    open.forge_number(),
                    open.url.as_deref().unwrap_or_default()
                ));
            }
            let base = match base {
                Some(base) => base,
                None => {
                    let full = repo.find_default_branch(None)?.ok_or_else(|| {
                        Error("cannot find the default branch; pass --base <branch>".into())
                    })?;
                    crate::git::branch_name(&full).to_owned()
                }
            };
            let made = provider.create(&NewPr {
                head: &branch,
                base: &base,
                title: &title,
                body: &body,
                draft,
            })?;
            shown(provider.view(&made)?)
        }
        Command::Upload { files } => {
            no_text(provider, "upload")?;
            upload::run(provider, &files)
        }
        Command::Update {
            number: given,
            title,
            body,
            add_label,
            remove_label,
            draft,
        } => {
            provider.allow_write("agt pr update")?;
            no_text(provider, "update")?;
            let changes = Changes {
                title,
                body,
                add_labels: add_label,
                remove_labels: remove_label,
                draft,
            };
            if changes.title.is_none()
                && changes.body.is_none()
                && changes.add_labels.is_empty()
                && changes.remove_labels.is_empty()
                && changes.draft.is_none()
            {
                return fail(
                    "nothing to update: pass --title, --body, --add-label, --remove-label or --draft",
                );
            }
            let at = provider.at(&given)?;
            let current = provider.view(&at)?;
            provider.update(&current, &changes)?;
            shown(provider.view(&at)?)
        }
        Command::Merge {
            number: given,
            sha,
            method,
            auto,
        } => {
            provider.allow_write("agt pr merge")?;
            let at = provider.at(&given)?;
            let current = provider.view(&at)?;
            guard(&current, &sha)?;
            if current.draft {
                return fail(format!("{at} is a draft; mark it ready first"));
            }
            let outcome = if auto {
                provider.enable_auto(&current, &current.head_sha, method)?
            } else {
                provider.merge(&current, &current.head_sha, method)?
            };
            let after = provider.view(&at)?;
            if after.state == ReviewState::Merged {
                *merged_text = Some(provider.merged_text(&current, &after));
            }
            settle(outcome, after.json())
        }
        Command::AutoMerge {
            switch,
            number: given,
            sha,
        } => {
            provider.allow_write("agt pr auto-merge")?;
            let at = provider.at(&given)?;
            let current = provider.view(&at)?;
            not_queued(&current)?;
            let (changed, outcome) = match switch {
                Switch::Off => (pause(provider, &current, &at)?, Outcome::Done),
                Switch::On => restore(provider, &current, &at, sha.as_deref())?,
            };
            let now = provider.view(&at)?;
            let on = now.auto_merge.is_some();
            let readable = now.auto_merge_known;
            // Turning auto-merge on merges at once when the checks already passed.
            let merged =
                matches!(switch, Switch::On) && changed && now.state == ReviewState::Merged;
            if merged {
                *merged_text = Some(provider.merged_text(&current, &now));
            }
            if readable && changed && !merged && on == matches!(switch, Switch::Off) {
                return fail(format!(
                    "auto-merge is still {} on {at} after the change",
                    if on { "on" } else { "off" }
                ));
            }
            if matches!(switch, Switch::On) && readable {
                provider.forget(&at)?;
            }
            settle(
                outcome,
                json!({"changed": changed, "merged": merged, "pr": now.json()}),
            )
        }
    }
}

/// Fails for a provider with nothing to store a title, body or label in.
fn no_text(provider: &dyn Provider, what: &str) -> Result<()> {
    match provider.capabilities().no_text {
        Some(why) => fail(format!("agt pr {what} is not available: {why}")),
        None => Ok(()),
    }
}

/// Turns auto-merge off after recording its method; false when it was off already.
fn pause(provider: &dyn Provider, pr: &Pr, at: &At) -> Result<bool> {
    let before = provider.recorded(at)?;
    let Some(method) = provider.auto_method(pr, before.as_deref()) else {
        return Ok(false);
    };
    provider.record(at, &method)?;
    if provider.disable_auto(pr)? {
        return Ok(true);
    }
    // Nothing was scheduled: drop the record this call made, not one an earlier pause left.
    if before.is_none() {
        provider.forget(at)?;
    }
    Ok(false)
}

/// Turns auto-merge back on with the recorded method; false when it is on already.
fn restore(
    provider: &dyn Provider,
    pr: &Pr,
    at: &At,
    sha: Option<&str>,
) -> Result<(bool, Outcome)> {
    if pr.auto_merge.is_some() && !provider.capabilities().resumes_auto_merge {
        return Ok((false, Outcome::Done));
    }
    let Some(method) = provider.recorded(at)? else {
        return fail(format!(
            "no paused auto-merge is recorded for {at}; enable it with `agt pr merge --auto`"
        ));
    };
    let sha = sha.unwrap_or(&pr.head_sha);
    guard(pr, sha)?;
    let outcome = provider.enable_auto(pr, &pr.head_sha, How::named(&method))?;
    Ok((true, outcome))
}

/// Prints `shown`. When a provider gave up waiting on gates, it also fails with their names and
/// the exit status of a timed-out wait; the auto-merge it left is kept.
fn settle(outcome: Outcome, shown: Value) -> Result<Output> {
    let Outcome::Pending(gates) = outcome else {
        return Ok(Output::Json(shown));
    };
    output::print(Output::Json(shown))?;
    let waiting = gates.join("; ");
    Ok(Output::Failure(
        Error(format!(
            "not merged yet, waiting on: {waiting}\nthe auto-merge is kept: run the command or `auto-merge on` again to keep waiting"
        )),
        TIMED_OUT,
    ))
}
