//! What `agt pr` asks of a provider: the PR/MR host, with one implementation per forge. The
//! shared flow in `mod.rs` names no forge and reads no field that only one host sets.

use std::fmt;

use super::upload::Uploader;
use super::{Changes, How, Pr, State, memo};
use crate::core::error::{Error, Result};
use crate::store::model::ReviewState;

/// How a PR/MR is addressed: by number on a forge.
#[derive(Clone)]
pub enum At {
    Number(u64),
}

impl fmt::Display for At {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(number) => write!(out, "#{number}"),
        }
    }
}

/// What a provider can do that the shared flow must know before it calls.
pub struct Capabilities {
    /// `auto-merge on` over an auto-merge that is on does nothing; a provider that waits on
    /// gates itself waits again instead.
    pub resumes_auto_merge: bool,
    /// Why `create`, `update` and `upload` are refused, for a provider with nothing to store a
    /// title, body or label in.
    pub no_text: Option<&'static str>,
}

/// The PRs/MRs `list` keeps.
pub struct Filter<'a> {
    pub branch: Option<&'a str>,
    pub state: State,
    /// Only those the signed-in user opened.
    pub mine: bool,
}

/// A PR/MR to open.
pub struct NewPr<'a> {
    pub head: &'a str,
    pub base: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub draft: bool,
}

pub trait Provider {
    /// The address `text` (what the user typed) names; a forge refuses a PR/MR of another
    /// repository.
    fn at(&self, text: &str) -> Result<At>;

    /// Names the PR/MR in the paused auto-merge records.
    fn key(&self, at: &At) -> String;

    /// Where the PRs/MRs are, for messages.
    fn place(&self) -> String;

    /// Refuses `what` where writes are switched off.
    fn allow_write(&self, what: &str) -> Result<()>;

    /// Names a missing or unauthenticated tool instead of repeating its raw error.
    fn explain(&self, error: Error) -> Error;

    fn capabilities(&self) -> Capabilities;

    fn view(&self, at: &At) -> Result<Pr>;

    fn list(&self, filter: &Filter) -> Result<Vec<Pr>>;

    /// The open PR/MR of `branch`, else the newest of any state.
    fn of_branch(&self, branch: &str) -> Result<Option<Pr>> {
        let found = self.list(&Filter {
            branch: Some(branch),
            state: State::All,
            mine: false,
        })?;
        Ok(open_first(found))
    }

    fn create(&self, new: &NewPr) -> Result<At>;

    fn update(&self, current: &Pr, changes: &Changes) -> Result<()>;

    /// Refuses before any file is read when the host cannot take uploads.
    fn upload_check(&self) -> Result<()> {
        Ok(())
    }

    /// Uploads one image per call; the one lookup the host needs happens here, not per image.
    fn uploader(&self) -> Result<Uploader<'_>>;

    /// Merges now, without auto-merge.
    fn merge(&self, pr: &Pr, sha: &str, how: Option<How>) -> Result<()>;

    fn enable_auto(&self, pr: &Pr, sha: &str, how: Option<How>) -> Result<()>;

    /// Turns auto-merge off; false when it was off already, which only Forgejo can tell.
    fn disable_auto(&self, pr: &Pr) -> Result<bool>;

    /// The method of the auto-merge `pr` has, which `off` records; `None` when it is off.
    /// `recorded` is what an earlier call left for a host that cannot say.
    fn auto_method(&self, pr: &Pr, recorded: Option<&str>) -> Option<String>;

    /// The title and body that closing keywords are read from once `after` is merged.
    fn merged_text(&self, _before: &Pr, after: &Pr) -> String {
        format!("{}\n{}", after.title, after.body)
    }

    /// The method a paused auto-merge of `at` was left with.
    fn recorded(&self, at: &At) -> Result<Option<String>> {
        memo::recorded(&self.key(at))
    }

    fn record(&self, at: &At, method: &str) -> Result<()> {
        memo::record(&self.key(at), method)
    }

    fn forget(&self, at: &At) -> Result<()> {
        memo::forget(&self.key(at))
    }
}

/// The open PR/MR of the repository itself (not a fork), else the newest of any state.
pub fn open_first(mut found: Vec<Pr>) -> Option<Pr> {
    found.retain(|pr| pr.same_repo);
    let at = found
        .iter()
        .position(|pr| pr.state == ReviewState::Open)
        .unwrap_or(0);
    if found.is_empty() {
        return None;
    }
    Some(found.swap_remove(at))
}
