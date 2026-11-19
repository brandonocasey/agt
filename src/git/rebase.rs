//! Rebasing the checked-out branch onto a commit, through libgit2's rebase, with the hooks git
//! runs for a rebase: `pre-rebase`, `prepare-commit-msg` and `post-commit` for each commit, and
//! `post-rewrite`.

use std::fs;

use super::merge::move_branch_to;
use super::{Repo, commit_id, hooks, identity};
use crate::core::error::{Error, Result};

/// How a rebase ended.
pub enum Rebased {
    /// The branch holds the replayed commits.
    Done,
    /// The rebase stopped and was aborted, so the branch is as it was. `conflicts` names the
    /// paths that conflicted; `error` is set when it stopped for another reason.
    Stopped {
        conflicts: Vec<String>,
        error: Option<String>,
    },
}

impl Repo {
    /// Rebases the branch checked out here onto `onto`, replaying the commits `onto` lacks, as
    /// `git rebase --no-autostash`. `tip` is the commit the branch is expected to be at;
    /// when it is at another commit the call fails before it changes anything.
    pub fn rebase_onto(&self, onto: &str, tip: &str) -> Result<Rebased> {
        let repo = self.open_writing()?;
        let at = at_tip(&repo, tip)?;
        let onto_id = commit_id(&repo, onto)?;
        if onto_id == at || repo.graph_descendant_of(at, onto_id)? {
            return Ok(Rebased::Done);
        }
        if let Err(said) = hooks::run(&repo, "pre-rebase", &[onto], "") {
            return Err(Error(format!(
                "the pre-rebase hook refused to rebase: {said}"
            )));
        }
        // The hook may have committed.
        at_tip(&repo, tip)?;
        let upstream = repo.find_annotated_commit(onto_id)?;
        let mut options = git2::RebaseOptions::new();
        options.inmemory(true);
        let mut rebase = repo.rebase(None, Some(&upstream), None, Some(&mut options))?;
        let mut replaying = Replaying {
            repo: &repo,
            committer: identity::committer_in(&repo)?,
            last: onto_id,
            rewrites: String::new(),
        };
        while let Some(step) = rebase.next() {
            let stopped = match step {
                Ok(step) => replaying.replay(&mut rebase, step.id())?,
                Err(error) => Some(Rebased::Stopped {
                    conflicts: Vec::new(),
                    error: Some(error.message().to_owned()),
                }),
            };
            if let Some(stopped) = stopped {
                return Ok(stopped);
            }
        }
        let rebased = repo.find_commit(replaying.last)?;
        move_branch_to(&repo, &rebased, &format!("rebase (finish): onto {onto_id}"))?;
        // A failing post-rewrite hook does not undo the rebase.
        let _ = hooks::run(&repo, "post-rewrite", &["rebase"], &replaying.rewrites);
        Ok(Rebased::Done)
    }
}

/// The commit HEAD is at, which must be `tip`.
fn at_tip(repo: &git2::Repository, tip: &str) -> Result<git2::Oid> {
    let at = repo.head()?.peel_to_commit()?.id();
    if at != commit_id(repo, tip)? {
        return Err(Error(format!(
            "the branch was at {at}, not {tip}, when the rebase started"
        )));
    }
    Ok(at)
}

/// A rebase in progress that commits each step it applies in memory.
struct Replaying<'r> {
    repo: &'r git2::Repository,
    committer: git2::Signature<'static>,
    /// The commit made by the last step, `onto` before the first.
    last: git2::Oid,
    /// The old and new id of each replayed commit, as `post-rewrite` reads them.
    rewrites: String,
}

impl Replaying<'_> {
    /// Commits the step the rebase just applied, which replays `picked`. `Some` when it conflicted
    /// or a hook or the commit failed; a commit that the base already has is dropped, as git does.
    fn replay(
        &mut self,
        rebase: &mut git2::Rebase<'_>,
        picked: git2::Oid,
    ) -> Result<Option<Rebased>> {
        let index = rebase.inmemory_index()?;
        if index.has_conflicts() {
            let conflicts = index
                .conflicts()?
                .flatten()
                .filter_map(|conflict| {
                    let side = conflict.our.or(conflict.their).or(conflict.ancestor)?;
                    Some(String::from_utf8_lossy(&side.path).into_owned())
                })
                .collect();
            return Ok(Some(Rebased::Stopped {
                conflicts,
                error: None,
            }));
        }
        let stopped = |error: String| {
            Some(Rebased::Stopped {
                conflicts: Vec::new(),
                error: Some(error),
            })
        };
        let message = match edited_message(self.repo, picked) {
            Ok(message) => message,
            Err(said) => return Ok(stopped(said)),
        };
        match rebase.commit(None, &self.committer, message.as_deref()) {
            Ok(new) => {
                self.last = new;
                self.rewrites.push_str(&format!("{picked} {new}\n"));
                // git ignores the exit status of post-commit. The rebase is in memory, so the hook
                // sees HEAD at the pre-rebase tip.
                let _ = hooks::run(self.repo, "post-commit", &[], "");
                Ok(None)
            }
            Err(error) if error.code() == git2::ErrorCode::Applied => Ok(None),
            Err(error) => Ok(stopped(error.message().to_owned())),
        }
    }
}

/// The message of `picked` after the `prepare-commit-msg` hook, `None` when the hook left it as
/// it was. Fails with what the hook said when it refuses.
fn edited_message(
    repo: &git2::Repository,
    picked: git2::Oid,
) -> std::result::Result<Option<String>, String> {
    if !hooks::exists(repo, "prepare-commit-msg") {
        return Ok(None);
    }
    let original = repo
        .find_commit(picked)
        .map_err(|error| error.message().to_owned())?;
    let file = repo.path().join("COMMIT_EDITMSG");
    fs::write(&file, original.message_bytes())
        .map_err(|error| format!("write {}: {error}", file.display()))?;
    let path = file.to_string_lossy().into_owned();
    hooks::run(repo, "prepare-commit-msg", &[&path, "message"], "")?;
    let after = fs::read(&file).map_err(|error| format!("read {}: {error}", file.display()))?;
    if after == original.message_bytes() {
        return Ok(None);
    }
    String::from_utf8(after)
        .map(Some)
        .map_err(|_| "the prepare-commit-msg hook wrote a message that is not UTF-8".to_owned())
}
