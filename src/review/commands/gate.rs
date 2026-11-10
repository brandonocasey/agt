use crate::core::error::Result;

use super::approve::blocking_threads;
use super::status::status_of;
use super::{Ctx, Selection};

/// The stored review of one branch at one head.
pub struct BranchReview {
    pub reviewed: bool,
    pub unreviewed: usize,
    /// Ids of the open findings that block an approval.
    pub blocking: Vec<String>,
    /// An approval is recorded at exactly the head.
    pub approved: bool,
}

/// The review of local `branch` at `head` against `target`, as `status` reports it. It reads
/// the stored review only. Nothing is synced, and the current checkout does not count.
pub fn branch_review(branch: &str, head: &str, target: &str) -> Result<BranchReview> {
    let ctx = Ctx::new(
        Selection {
            target: None,
            range: None,
            default_branch: Some(target.to_owned()),
        },
        true,
        true,
        false,
    )?;
    let head = ctx.repo.commit(head)?;
    let scope = ctx.scope_of_branch(branch.to_owned(), head.clone())?;
    let status = status_of(&ctx, &scope, true)?;
    let threads = scope.existing.iter().flat_map(|target| &target.threads);
    Ok(BranchReview {
        reviewed: status["reviewed"] == true,
        unreviewed: status["unreviewed_commits"].as_array().map_or(0, Vec::len),
        blocking: scope
            .existing
            .iter()
            .flat_map(blocking_threads)
            .map(str::to_owned)
            .collect(),
        approved: threads
            .into_iter()
            .any(|thread| thread.approval.as_ref().is_some_and(|a| a.head == head)),
    })
}
