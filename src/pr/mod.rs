//! `agt pr`: one GitHub/GitLab-neutral command for the PR/MR of the current repository: view,
//! list, create, update, merge behind a head-sha guard, and pause or restore auto-merge. The
//! forge comes from the tracker of the origin remote, or the one `--remote` or a PR/MR URL
//! names; writes are refused where `push` is `off`. Output is JSON.

mod github;
mod gitlab;
mod memo;

use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};

use crate::core::config::{Config, Kind};
use crate::core::error::{Error, Result, fail};
use crate::core::output::{self, Output};
use crate::forge::auto::{Offline, pr_write_allowed};
use crate::forge::landed::Source;
use crate::forge::select::{Selected, require_tracker};
use crate::forge::service::{ReviewService, SERVICES, kind};
use crate::forge::{PrRef, parse_pr};
use crate::git::Repo;
use crate::store::model::{Forge, ReviewState};

/// View, list, create, update and merge the PRs/MRs of this repository, on GitHub or GitLab.
#[derive(Args)]
pub struct Cli {
    /// Tracker to use: a git remote name or `host/repo` [default: the repository of a PR/MR
    /// URL, else the tracker of `origin`].
    #[arg(long, global = true)]
    remote: Option<String>,
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
    /// branch, head sha, author, labels, auto_merge (on or off) with its method, merge queue or
    /// train state, mergeable and conflicts.
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
    /// Merge only while the head is --sha (the forge enforces it too). Never deletes the branch
    /// or bypasses protections.
    ///
    /// With --auto, merge when checks pass; GitHub needs --method unless auto-merge was on, and
    /// merges a PR that can merge now at once, as gh does, unless its base requires a merge
    /// queue (then auto-merge queues it).
    /// Without --auto, a GitHub PR whose base requires a merge queue is added to the queue
    /// through auto-merge, so it needs --method too. GitLab cannot rebase on merge.
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

/// A PR/MR as every subcommand reports it.
pub struct Pr {
    /// The forge's node id: GitHub's GraphQL id, GitLab's iid.
    pub id: String,
    pub number: u64,
    pub url: String,
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
    /// The merge queue or train state, when queued.
    pub queue: Option<String>,
    pub mergeable: String,
    pub merge_state: String,
    pub conflicts: bool,
    /// The head branch lives in this repository (not a fork).
    pub same_repo: bool,
    /// GitLab: the MR squashes on merge.
    pub squash: bool,
    /// GitHub: the base branch requires a merge queue.
    pub merge_queue: bool,
}

impl Pr {
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
            "auto_merge": if self.auto_merge.is_some() { "on" } else { "off" },
            "auto_merge_method": self.auto_merge,
            "queue": self.queue,
            "mergeable": self.mergeable,
            "merge_state": self.merge_state,
            "conflicts": self.conflicts,
        })
    }
}

/// The repository of the selected tracker, with the service that talks to its forge.
struct Remote {
    origin: Source,
    service: Box<dyn ReviewService>,
    selected: Selected,
}

fn number(text: &str, remote: &Remote) -> Result<u64> {
    let PrRef {
        number, location, ..
    } = parse_pr(text)?;
    if let Some((host, repo)) = location
        && (host != remote.origin.host || repo != remote.origin.repo)
    {
        return fail(format!(
            "{text} is not in this repository ({}/{})",
            remote.origin.host, remote.origin.repo
        ));
    }
    Ok(number)
}

impl Remote {
    fn connect(
        repo: &Repo,
        config: &Config,
        remote: Option<&str>,
        pr: Option<&PrRef>,
    ) -> Result<Self> {
        let selected = require_tracker(repo, config, Kind::PullRequests, remote, pr)?;
        let origin = Source::of(&selected)?;
        let service = (kind(origin.forge).connect)(&origin.host);
        Ok(Self {
            origin,
            service,
            selected,
        })
    }

    /// Refuses `what` when the tracker's `push` is `off`.
    fn allow_write(&self, what: &str) -> Result<()> {
        pr_write_allowed(what, &self.selected.name(), self.selected.push)
    }

    fn key(&self, number: u64) -> String {
        format!("{}/{}#{number}", self.origin.host, self.origin.repo)
    }

    fn view(&self, number: u64) -> Result<Pr> {
        let (t, repo) = (self.service.transport(), self.origin.repo.as_str());
        match self.origin.forge {
            Forge::Github => github::view(t, repo, number),
            Forge::Gitlab => gitlab::view(t, repo, number),
        }
    }

    fn list(&self, branch: Option<&str>, state: State, mine: bool) -> Result<Vec<Pr>> {
        let (t, repo) = (self.service.transport(), self.origin.repo.as_str());
        match self.origin.forge {
            Forge::Github => github::list(t, repo, branch, state, mine),
            Forge::Gitlab => gitlab::list(t, repo, branch, state, mine),
        }
    }

    fn create(
        &self,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<u64> {
        let (t, repo) = (self.service.transport(), self.origin.repo.as_str());
        match self.origin.forge {
            Forge::Github => github::create(t, repo, branch, base, title, body, draft),
            Forge::Gitlab => gitlab::create(t, repo, branch, base, title, body, draft),
        }
    }

    fn update(&self, current: &Pr, changes: &Changes) -> Result<()> {
        let (t, repo) = (self.service.transport(), self.origin.repo.as_str());
        match self.origin.forge {
            Forge::Github => github::update(t, repo, current, changes),
            Forge::Gitlab => gitlab::update(t, repo, current, changes),
        }
    }

    /// Merges now, without auto-merge.
    fn merge(&self, pr: &Pr, sha: &str, method: Option<How>) -> Result<()> {
        let (t, repo) = (self.service.transport(), self.origin.repo.as_str());
        match self.origin.forge {
            Forge::Github => github::merge(t, repo, pr.number, sha, method),
            Forge::Gitlab => gitlab::merge(t, repo, pr, sha, method, false),
        }
    }

    fn disable_auto(&self, pr: &Pr) -> Result<()> {
        let (t, repo) = (self.service.transport(), self.origin.repo.as_str());
        match self.origin.forge {
            Forge::Github => github::disable_auto(t, pr),
            Forge::Gitlab => gitlab::disable_auto(t, repo, pr.number),
        }
    }

    /// The open PR/MR of `branch`, else the newest of any state.
    fn of_branch(&self, branch: &str) -> Result<Option<Pr>> {
        let mut found = self.list(Some(branch), State::All, false)?;
        found.retain(|pr| pr.same_repo);
        let at = found
            .iter()
            .position(|pr| pr.state == ReviewState::Open)
            .unwrap_or(0);
        Ok((!found.is_empty()).then(|| found.swap_remove(at)))
    }

    fn enable_auto(&self, pr: &Pr, sha: &str, how: Option<How>) -> Result<()> {
        let (t, repo) = (self.service.transport(), self.origin.repo.as_str());
        match self.origin.forge {
            Forge::Github => {
                let how = how
                    .or_else(|| pr.auto_merge.as_deref().and_then(How::named))
                    .ok_or_else(|| Error("GitHub auto-merge needs --method".into()))?;
                // GitHub offers auto-merge only while a PR cannot merge yet; like gh, merge a
                // PR that can merge now, unless the base requires a merge queue: auto-merge
                // is what queues it.
                if !pr.merge_queue
                    && matches!(pr.merge_state.as_str(), "clean" | "has_hooks" | "unstable")
                {
                    return self.merge(pr, sha, Some(how));
                }
                github::enable_auto(t, pr, sha, how)
            }
            Forge::Gitlab => gitlab::merge(t, repo, pr, sha, how, true),
        }
    }
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
        return fail(format!("#{} is {}, not open", pr.number, pr.state.as_str()));
    }
    if sha.len() < 7 || !pr.head_sha.starts_with(sha) {
        return fail(format!(
            "#{} head is {}, not {sha}; inspect it again before merging",
            pr.number, pr.head_sha
        ));
    }
    Ok(())
}

fn not_queued(pr: &Pr) -> Result<()> {
    match &pr.queue {
        Some(queue) => fail(format!(
            "#{} is in a merge queue or train ({queue}); leave it untouched and do not push to its branch",
            pr.number
        )),
        None => Ok(()),
    }
}

/// Names a missing or unauthenticated gh/glab instead of repeating the raw tool error.
fn explain(error: Error) -> Error {
    let message = &error.0;
    for tool in SERVICES.iter().map(|kind| kind.tool) {
        if message.starts_with(&format!("run {tool}:")) {
            return Error(format!(
                "{tool} is not installed or not on PATH (needed for this repository's forge)"
            ));
        }
        if message.starts_with(&format!("{tool} "))
            && ["auth login", "HTTP 401", "not logged in", "authentication"]
                .iter()
                .any(|hint| message.contains(hint))
        {
            return Error(format!(
                "{tool} is not authenticated: run `{tool} auth login`\n{message}"
            ));
        }
    }
    error
}

/// Runs the command. Also returns the title and body of a PR/MR that this call merged.
pub fn run(cli: Cli) -> Result<(Output, Option<String>)> {
    let mut merged_text = None;
    let output = execute(cli, &mut merged_text).map_err(explain)?;
    Ok((output, merged_text))
}

fn execute(cli: Cli, merged_text: &mut Option<String>) -> Result<Output> {
    Offline::current().forbid("agt pr")?;
    let repo = Repo::discover()?;
    let config = Config::load_for_repo(&repo)?;
    let given = match &cli.command {
        Command::Update { number, .. }
        | Command::Merge { number, .. }
        | Command::AutoMerge { number, .. } => Some(number.as_str()),
        Command::View { number, .. } => number.as_deref(),
        Command::List { .. } | Command::Create { .. } => None,
    };
    let pr_ref = given.map(parse_pr).transpose()?;
    let remote = Remote::connect(&repo, &config, cli.remote.as_deref(), pr_ref.as_ref())?;
    let slug = remote.origin.repo.as_str();
    let shown = |pr: Pr| Ok(Output::Json(pr.json()));
    match cli.command {
        Command::View {
            number: given,
            branch,
        } => {
            if let Some(given) = given {
                return shown(remote.view(number(&given, &remote)?)?);
            }
            let branch = branch_of(&repo, branch)?;
            let Some(found) = remote.of_branch(&branch)? else {
                return fail(format!("no PR/MR for branch {branch} in {slug}"));
            };
            shown(remote.view(found.number)?)
        }
        Command::List {
            mine,
            branch,
            state,
        } => Ok(Output::Json(Value::Array(
            remote
                .list(branch.as_deref(), state, mine)?
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
            remote.allow_write("agt pr create")?;
            let branch = branch_of(&repo, None)?;
            if let Some(open) = remote
                .list(Some(&branch), State::Open, false)?
                .into_iter()
                .find(|pr| pr.same_repo)
            {
                output::print(Output::Json(open.json()))?;
                return fail(format!(
                    "branch {branch} already has {}: {}",
                    open.number, open.url
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
            let made = remote.create(&branch, &base, &title, &body, draft)?;
            shown(remote.view(made)?)
        }
        Command::Update {
            number: given,
            title,
            body,
            add_label,
            remove_label,
            draft,
        } => {
            remote.allow_write("agt pr update")?;
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
            let at = number(&given, &remote)?;
            let current = remote.view(at)?;
            remote.update(&current, &changes)?;
            shown(remote.view(at)?)
        }
        Command::Merge {
            number: given,
            sha,
            method,
            auto,
        } => {
            remote.allow_write("agt pr merge")?;
            let at = number(&given, &remote)?;
            let current = remote.view(at)?;
            guard(&current, &sha)?;
            if current.draft {
                return fail(format!("#{at} is a draft; mark it ready first"));
            }
            // A GitHub base that requires a merge queue takes a PR only through auto-merge,
            // which queues it (what `gh pr merge` does).
            if auto || current.merge_queue {
                remote.enable_auto(&current, &current.head_sha, method)?;
            } else {
                remote.merge(&current, &current.head_sha, method)?;
            }
            let after = remote.view(at)?;
            if after.state == ReviewState::Merged {
                *merged_text = Some(format!("{}\n{}", after.title, after.body));
            }
            shown(after)
        }
        Command::AutoMerge {
            switch,
            number: given,
            sha,
        } => {
            remote.allow_write("agt pr auto-merge")?;
            let at = number(&given, &remote)?;
            let key = remote.key(at);
            let current = remote.view(at)?;
            not_queued(&current)?;
            let changed = match switch {
                Switch::Off => pause(&remote, &current, &key)?,
                Switch::On => restore(&remote, &current, &key, sha.as_deref())?,
            };
            let now = remote.view(at)?;
            let on = now.auto_merge.is_some();
            // Turning auto-merge on merges at once when the checks already passed.
            let merged =
                matches!(switch, Switch::On) && changed && now.state == ReviewState::Merged;
            if merged {
                *merged_text = Some(format!("{}\n{}", now.title, now.body));
            }
            if changed && !merged && on == matches!(switch, Switch::Off) {
                return fail(format!(
                    "auto-merge is still {} on #{at} after the change",
                    if on { "on" } else { "off" }
                ));
            }
            if matches!(switch, Switch::On) {
                memo::forget(&key)?;
            }
            Ok(Output::Json(
                json!({"changed": changed, "merged": merged, "pr": now.json()}),
            ))
        }
    }
}

/// Turns auto-merge off after recording its method; false when it was off already.
fn pause(remote: &Remote, pr: &Pr, key: &str) -> Result<bool> {
    let Some(method) = &pr.auto_merge else {
        return Ok(false);
    };
    memo::record(key, method)?;
    remote.disable_auto(pr)?;
    Ok(true)
}

/// Turns auto-merge back on with the recorded method; false when it is on already.
fn restore(remote: &Remote, pr: &Pr, key: &str, sha: Option<&str>) -> Result<bool> {
    if pr.auto_merge.is_some() {
        return Ok(false);
    }
    let Some(method) = memo::recorded(key)? else {
        return fail(format!(
            "no paused auto-merge is recorded for #{}; enable it with `agt pr merge --auto`",
            pr.number
        ));
    };
    let sha = sha.unwrap_or(&pr.head_sha);
    guard(pr, sha)?;
    remote.enable_auto(pr, &pr.head_sha, How::named(&method))?;
    Ok(true)
}
