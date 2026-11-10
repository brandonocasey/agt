//! A local PR is a local branch against the local default branch. Merging moves that branch.
//! Everything is read from local refs. Nothing here contacts a remote, pushes or asks a forge.

mod auto;
mod rebase;

use std::collections::HashSet;
use std::fs;

use super::gates::Gates;
use super::intent;
use super::provider::{At, Capabilities, Filter, Outcome, Provider};
use super::{How, Native, Pr, State};
use crate::core::error::{Error, Result, fail};
use crate::core::shell::quote;
use crate::git::Repo;
use crate::git::fast_forward::{Update, fast_forward};
use crate::store::model::ReviewState;

/// The most PRs `list` returns.
const LIST_LIMIT: usize = 100;

/// A local branch with the fields a PR shows.
struct Row {
    branch: String,
    tip: String,
    author: String,
    title: String,
}

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

    /// The PR of `branch` against `target` at `target_tip`, with its messages as body.
    fn pr(&self, branch: &str, target: &str, target_tip: &str) -> Result<Pr> {
        let tip = self.tip(branch)?;
        let range = format!("{target_tip}..{tip}");
        let messages = self
            .repo
            .git(&["log", "--format=%B%x00", "--end-of-options", &range])?;
        let messages: Vec<&str> = messages
            .split('\0')
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .collect();
        let author = self.repo.authored(&[tip.as_str()])?;
        let row = Row {
            title: self.repo.subject(&tip)?,
            author: author
                .get(&tip)
                .map(|(name, _)| name.clone())
                .unwrap_or_default(),
            branch: branch.to_owned(),
            tip,
        };
        let merged = self.repo.is_ancestor(&row.tip, target_tip)?;
        let descends = self.repo.is_ancestor(target_tip, &row.tip)?;
        self.row_pr(row, target, (merged, descends), messages.join("\n\n"))
    }

    /// The local branches `selector` picks, newest commit first, from one `git for-each-ref` call.
    fn rows(&self, selector: &[&str]) -> Result<Vec<Row>> {
        let mut args = vec![
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:lstrip=2)%00%(objectname)%00%(authorname)%00%(contents:subject)",
        ];
        args.extend(selector);
        args.push("refs/heads/");
        let listing = self.repo.git(&args)?;
        Ok(listing
            .lines()
            .filter_map(|line| {
                let mut fields = line.splitn(4, '\0');
                Some(Row {
                    branch: fields.next()?.to_owned(),
                    tip: fields.next()?.to_owned(),
                    author: fields.next()?.to_owned(),
                    title: fields.next()?.to_owned(),
                })
            })
            .collect())
    }

    /// The names of the branches `selector` picks.
    fn names(&self, selector: &[&str]) -> Result<HashSet<String>> {
        Ok(self
            .rows(selector)?
            .into_iter()
            .map(|row| row.branch)
            .collect())
    }

    /// The PR of `row` against `target`.
    /// `merged` says the tip is in the target. `descends` says the target is in the tip.
    fn row_pr(
        &self,
        row: Row,
        target: &str,
        (merged, descends): (bool, bool),
        body: String,
    ) -> Result<Pr> {
        let (mergeable, merge_state) = match (merged, descends) {
            (true, _) => ("unknown", ""),
            (false, true) => ("mergeable", "mergeable"),
            (false, false) => ("blocked", "behind"),
        };
        self.with_intent(Pr {
            id: String::new(),
            number: None,
            url: None,
            title: row.title,
            body,
            state: if merged {
                ReviewState::Merged
            } else {
                ReviewState::Open
            },
            draft: false,
            head_branch: row.branch,
            base_branch: target.to_owned(),
            head_sha: row.tip,
            author: row.author,
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

    /// Moves `target` to `branch`, rebasing first when it is behind and `how` says so.
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
        if self.repo.is_ancestor(&target_tip, &head)? {
            self.check_holders(branch)?;
        } else {
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
                "{target} not updated: {}{}{}",
                refused.reason,
                // Git's own command fails again once a work tree holds the target.
                // The retry is this merge, which picks the place anew.
                refused
                    .retry
                    .map(|_| format!(
                        "; retry with: agt pr --local merge {} --sha {head}",
                        quote(branch)
                    ))
                    .unwrap_or_default(),
                if rebased {
                    format!("; {branch} was rebased and was at {tip}")
                } else {
                    String::new()
                }
            )),
        }
    }
}

/// What the records call the merge method `how`. The methods that would create a commit fail.
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

    fn enable_auto_command(&self, at: &At, pr: &Pr) -> String {
        format!(
            "agt pr --local merge {} --sha {} --auto",
            quote(&at.id()),
            pr.head_sha
        )
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
            let name = self.repo.user_name()?;
            Some(name.ok_or_else(|| {
                Error(
                    "--mine needs an author name: set one with `git config user.name <name>`"
                        .into(),
                )
            })?)
        } else {
            None
        };
        let rows = match filter.state {
            State::Open => self.rows(&["--no-merged", &target_tip])?,
            State::Merged => self.rows(&["--merged", &target_tip])?,
            State::All => self.rows(&[])?,
        };
        let merged = match filter.state {
            State::All => self.names(&["--merged", &target_tip])?,
            State::Open | State::Merged => HashSet::new(),
        };
        let descending = match filter.state {
            State::Merged => HashSet::new(),
            State::Open | State::All => self.names(&["--contains", &target_tip])?,
        };
        let mut found = Vec::new();
        for row in rows {
            if row.branch == target
                || filter.branch.is_some_and(|only| only != row.branch)
                || me.as_ref().is_some_and(|me| *me != row.author)
            {
                continue;
            }
            let is_merged = matches!(filter.state, State::Merged) || merged.contains(&row.branch);
            let descends = descending.contains(&row.branch);
            found.push(self.row_pr(row, &target, (is_merged, descends), String::new())?);
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

    /// The merge adds no commits. The messages are read from the PR before it merges.
    fn merged_text(&self, before: &Pr, _after: &Pr) -> String {
        format!("{}\n{}", before.title, before.body)
    }
}
