//! The auto-merge of a local branch. `merge --auto` records an intent and waits on the gates.
//! It merges when they pass at the head the intent names. The gates are judged at that head for
//! `rebase` too.
//! A forge's rebase merge does the same.

use std::time::{Duration, Instant};

use super::{Local, method_of};
use crate::core::clock;
use crate::core::error::{Result, fail};
use crate::pr::intent::{self, Intent};
use crate::pr::provider::{Outcome, Provider};
use crate::pr::{How, Pr, gates};
use crate::store::model::ReviewState;

/// Seconds between two looks at the gates.
const POLL_SECS: u64 = 5;

/// Seconds of polling before a call gives up; the merge itself is never cut short.
const WAIT_SECS: u64 = 270;

/// What one look at an intent found.
enum Attempt {
    Merged,
    Waiting(Vec<String>),
}

impl Local<'_> {
    /// Records the intent to merge `pr` at `sha`, waits until the gates pass, then merges.
    /// A newer call replaces this one, and `auto-merge off` stops it.
    /// Both fail it, unless a plain merge landed the branch first.
    pub(super) fn auto_merge(&self, pr: &Pr, sha: &str, how: Option<How>) -> Result<Outcome> {
        let method = method_of(how)?;
        let key = self.key(&pr.at());
        let branch = pr.head_branch.as_str();
        let waiter = format!("{}-{}", std::process::id(), clock::unix_nanos());
        {
            let _lock = intent::lock(&key)?;
            intent::set(
                &key,
                Intent {
                    branch: branch.to_owned(),
                    sha: sha.to_owned(),
                    method: method.to_owned(),
                    waiter: waiter.clone(),
                },
            )?;
        }
        let started = Instant::now();
        let mut waited = 0;
        loop {
            let failing = {
                let _lock = intent::lock(&key)?;
                match self.attempt(&key, branch, &waiter)? {
                    Attempt::Merged => return Ok(Outcome::Done),
                    Attempt::Waiting(failing) => failing,
                }
            };
            // Polls count under `AGT_NOW`, where `sleep` returns at once. The clock stops a poll
            // that would start late.
            let left = Duration::from_secs(WAIT_SECS).saturating_sub(started.elapsed());
            if waited >= WAIT_SECS || left < Duration::from_secs(POLL_SECS) {
                return Ok(Outcome::Pending(failing));
            }
            clock::sleep(POLL_SECS);
            waited += POLL_SECS;
        }
    }

    /// Looks at the intent of `branch` once. It merges when the intent holds and the gates pass.
    /// The caller MUST hold the lock of `key`.
    fn attempt(&self, key: &str, branch: &str, waiter: &str) -> Result<Attempt> {
        let intent = match intent::get(key)? {
            Some(intent) if intent.waiter == waiter => intent,
            Some(_) => return fail(format!("another call took over the auto-merge of {branch}")),
            None => return self.gone(branch),
        };
        let tip = self.tip(branch)?;
        if tip != intent.sha {
            intent::clear(key)?;
            return fail(format!(
                "{branch} is at {tip}, not {}; the auto-merge was cleared",
                intent.sha
            ));
        }
        let target = self.target()?;
        if self.repo.is_ancestor(&tip, &self.tip(&target)?)? {
            intent::finish(key)?;
            return Ok(Attempt::Merged);
        }
        let how = How::named(&intent.method);
        let failing = self.failing_gates(branch, &tip, &target, how)?;
        if !failing.is_empty() {
            return Ok(Attempt::Waiting(failing));
        }
        // A merge that fails after the gates passed is final. So is a rebase that conflicts.
        let landed = self.land(branch, &tip, how);
        intent::finish(key)?;
        landed?;
        Ok(Attempt::Merged)
    }

    /// A waiter whose intent is gone reports the merge when a plain merge landed `branch`.
    /// Otherwise `auto-merge off` ended it, and that is an error.
    fn gone(&self, branch: &str) -> Result<Attempt> {
        let target_tip = self.tip(&self.target()?)?;
        if self.repo.is_ancestor(&self.tip(branch)?, &target_tip)? {
            return Ok(Attempt::Merged);
        }
        fail(format!("the auto-merge of {branch} was turned off"))
    }

    /// The gates that fail for `branch` at `tip`.
    /// The gates are checks, review, approval and that the branch lands.
    fn failing_gates(
        &self,
        branch: &str,
        tip: &str,
        target: &str,
        how: Option<How>,
    ) -> Result<Vec<String>> {
        let mut failing = gates::failing(self.gates, branch, tip, target)?;
        let target_tip = self.tip(target)?;
        if self.repo.is_ancestor(&target_tip, tip)? {
            return Ok(failing);
        }
        if matches!(how, Some(How::Rebase)) {
            if let Err(blockers) = self.rebase_place(branch, target, &target_tip, tip)? {
                failing.push(format!("lands: {}", blockers.join("; ")));
            }
        } else {
            failing.push(format!(
                "lands: {branch} is behind {target}; rebase it or use --method rebase"
            ));
        }
        Ok(failing)
    }

    /// `pr` with the auto-merge its intent asks for. An intent that does not fit is dropped.
    /// It does not fit when the branch moved or is not open.
    pub(super) fn with_intent(&self, mut pr: Pr) -> Result<Pr> {
        let key = self.key(&pr.at());
        let Some(intent) = intent::get(&key)? else {
            return Ok(pr);
        };
        if fits(&intent, &pr) {
            pr.auto_merge = Some(intent.method);
            return Ok(pr);
        }
        let _lock = intent::lock(&key)?;
        match intent::get(&key)? {
            Some(now) if fits(&now, &pr) => pr.auto_merge = Some(now.method),
            Some(_) => {
                intent::clear(&key)?;
            }
            None => {}
        }
        Ok(pr)
    }
}

fn fits(intent: &Intent, pr: &Pr) -> bool {
    pr.state == ReviewState::Open && intent.sha == pr.head_sha
}
