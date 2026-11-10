//! The local provider: a PR is a local branch against the local default branch, and merging it
//! moves that branch. Everything is read from local refs; nothing here fetches, pushes or asks a
//! forge.

mod auto;
mod rebase;

use std::fs;

use super::gates::Gates;
use super::intent;
use super::provider::{At, Capabilities, Filter, Outcome, Provider};
use super::{How, Native, Pr, State};
use crate::core::error::{Error, Result, fail};
use crate::git::Repo;
use crate::git::fast_forward::{Update, fast_forward};
use crate::store::model::ReviewState;

/// The most PRs `list` returns.
const LIST_LIMIT: usize = 100;

pub struct Local<'a> {
    repo: &'a Repo,
    gates: &'a dyn Gates,
    /// The git common directory, which tells repositories apart in the records.
    common: String,
}

impl<'a> Local<'a> {
    pub fn new(repo: &'a Repo, gates: &'a dyn Gates) -> Result<Self> {
        let common = fs::canonicalize(repo.common_dir()?)
            .map_err(|error| Error(format!("resolve the git directory: {error}")))?;
        Ok(Self {
            repo,
            gates,
            common: common.display().to_string(),
        })
    }

    /// The local default branch, found from local refs only.
    fn target(&self) -> Result<String> {
        self.repo.local_default_branch()?.ok_or_else(|| {
            Error("no local default branch: the repository has none of main, master or init.defaultBranch yet".into())
        })
    }

    fn tip(&self, branch: &str) -> Result<String> {
        self.repo
            .branch_tip(branch)?
            .ok_or_else(|| Error(format!("no local branch {branch}")))
    }

    /// The PR of `branch` against `target`, which is at `target_tip`.
    fn pr(&self, branch: &str, target: &str, target_tip: &str) -> Result<Pr> {
        let tip = self.tip(branch)?;
        let merged = self.repo.is_ancestor(&tip, target_tip)?;
        let range = format!("{target_tip}..{tip}");
        let messages = self
            .repo
            .git(&["log", "--format=%B%x00", "--end-of-options", &range])?;
        let messages: Vec<&str> = messages
            .split('\0')
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .collect();
        let (mergeable, merge_state) = match (merged, self.repo.is_ancestor(target_tip, &tip)?) {
            (true, _) => ("unknown", ""),
            (false, true) => ("mergeable", "mergeable"),
            (false, false) => ("blocked", "behind"),
        };
        let author = self.repo.authored(&[tip.as_str()])?;
        self.with_intent(Pr {
            id: String::new(),
            number: None,
            url: None,
            title: self.repo.subject(&tip)?,
            body: messages.join("\n\n"),
            state: if merged {
                ReviewState::Merged
            } else {
                ReviewState::Open
            },
            draft: false,
            head_branch: branch.to_owned(),
            base_branch: target.to_owned(),
            head_sha: tip.clone(),
            author: author
                .get(&tip)
                .map(|(name, _)| name.clone())
                .unwrap_or_default(),
            labels: Vec::new(),
            auto_merge: None,
            auto_merge_known: true,
            queue: None,
            mergeable: mergeable.to_owned(),
            merge_state: merge_state.to_owned(),
            conflicts: false,
            same_repo: true,
            native: Native::None,
        })
    }

    /// Moves `target` to `branch`, rebasing the branch first when it is behind and `how` says so.
    /// The branch MUST still be at `sha`.
    fn land(&self, branch: &str, sha: &str, how: Option<How>) -> Result<()> {
        method_of(how)?;
        let target = self.target()?;
        let tip = self.tip(branch)?;
        if tip != sha {
            return fail(format!(
                "{branch} is at {tip}, not {sha}; inspect it again before merging"
            ));
        }
        let target_tip = self.tip(&target)?;
        let mut head = tip.clone();
        let mut rebased = false;
        if !self.repo.is_ancestor(&target_tip, &head)? {
            if !matches!(how, Some(How::Rebase)) {
                return fail(format!(
                    "{branch} is behind {target}; rebase it first or merge with --method rebase"
                ));
            }
            head = self.rebase(branch, &target, &target_tip, &tip)?;
            rebased = true;
        }
        match fast_forward(self.repo, &target, &target_tip, &head)? {
            Update::Moved { .. } => Ok(()),
            Update::Refused(refused) => fail(format!(
                "{target} not updated: {}{}",
                refused.reason,
                if rebased {
                    format!("; {branch} was rebased and was at {tip}")
                } else {
                    String::new()
                }
            )),
        }
    }
}

/// What the records call the merge method `how`; the methods that would create a commit fail.
fn method_of(how: Option<How>) -> Result<&'static str> {
    match how {
        None => Ok("fast-forward"),
        Some(How::Rebase) => Ok("rebase"),
        Some(How::Merge | How::Squash) => {
            fail("a local merge creates no commit; use --method rebase, or none for a fast-forward")
        }
    }
}

/// The branch an address names.
fn branch_of(at: &At) -> Result<&str> {
    match at {
        At::Branch(branch) => Ok(branch),
        At::Number(_) => fail("a local PR is addressed by its branch name"),
    }
}

impl Provider for Local<'_> {
    fn at(&self, text: &str) -> Result<At> {
        Ok(At::Branch(text.to_owned()))
    }

    fn key(&self, at: &At) -> String {
        format!("{}#{}", self.common, at.id())
    }

    fn place(&self) -> String {
        "this repository".to_owned()
    }

    fn allow_write(&self, _what: &str) -> Result<()> {
        Ok(())
    }

    fn explain(&self, error: Error) -> Error {
        error
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            resumes_auto_merge: true,
            no_text: Some(
                "a local PR is the branch itself, and nothing stores a title, body, label or image for it",
            ),
        }
    }

    fn view(&self, at: &At) -> Result<Pr> {
        let branch = branch_of(at)?;
        let target = self.target()?;
        if branch == target {
            return fail(format!(
                "{branch} is the local default branch, not a PR against it"
            ));
        }
        self.pr(branch, &target, &self.tip(&target)?)
    }

    fn list(&self, filter: &Filter) -> Result<Vec<Pr>> {
        let target = self.target()?;
        let target_tip = self.tip(&target)?;
        let me = if filter.mine {
            self.repo.user_name()?
        } else {
            None
        };
        let refs = self.repo.git(&[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)",
            "refs/heads/",
        ])?;
        let mut found = Vec::new();
        for branch in refs
            .lines()
            .filter_map(|name| name.strip_prefix("refs/heads/"))
        {
            if branch == target || filter.branch.is_some_and(|only| only != branch) {
                continue;
            }
            let pr = self.pr(branch, &target, &target_tip)?;
            let wanted = match filter.state {
                State::Open => pr.state == ReviewState::Open,
                State::Merged => pr.state == ReviewState::Merged,
                State::All => true,
            };
            if wanted && me.as_ref().is_none_or(|me| *me == pr.author) {
                found.push(pr);
            }
            if found.len() == LIST_LIMIT {
                break;
            }
        }
        Ok(found)
    }

    fn merge(&self, pr: &Pr, sha: &str, how: Option<How>) -> Result<Outcome> {
        let key = self.key(&pr.at());
        let _lock = intent::lock(&key)?;
        self.land(&pr.head_branch, sha, how)?;
        intent::finish(&key)?;
        Ok(Outcome::Done)
    }

    fn enable_auto(&self, pr: &Pr, sha: &str, how: Option<How>) -> Result<Outcome> {
        self.auto_merge(pr, sha, how)
    }

    fn disable_auto(&self, pr: &Pr) -> Result<bool> {
        let key = self.key(&pr.at());
        let _lock = intent::lock(&key)?;
        intent::clear(&key)
    }

    fn auto_method(&self, pr: &Pr, _recorded: Option<&str>) -> Option<String> {
        pr.auto_merge.clone()
    }

    /// The method of the intent that waits, else the one `auto-merge off` left.
    fn recorded(&self, at: &At) -> Result<Option<String>> {
        let key = self.key(at);
        match intent::get(&key)? {
            Some(intent) => Ok(Some(intent.method)),
            None => intent::paused(&key),
        }
    }

    fn record(&self, at: &At, method: &str) -> Result<()> {
        let key = self.key(at);
        let _lock = intent::lock(&key)?;
        intent::pause(&key, method)
    }

    fn forget(&self, at: &At) -> Result<()> {
        let key = self.key(at);
        let _lock = intent::lock(&key)?;
        intent::forget_paused(&key)
    }

    /// The merge leaves no commits between the target and the branch, so the messages are read
    /// from the PR as it was before.
    fn merged_text(&self, before: &Pr, _after: &Pr) -> String {
        format!("{}\n{}", before.title, before.body)
    }
}
