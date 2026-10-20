//! Which stored issues `agt prune` may drop.

use crate::core::clock;
use crate::store::issues::{Category, Issue, LinkSource};
use crate::store::model::PostState;
use crate::store::prune::past_retention;

use super::merge::has_pending;

/// Whether `issue` closed at least 90 days before `now` and holds nothing that exists only
/// here: no queued change, unsent comment or create, conflict, failed send, note, local-only
/// comment or link, or comment deleted on the host. An open issue is never prunable. The close
/// time is the newer of the local and the host's last update, since neither records the close
/// itself. A timestamp that cannot be read keeps the issue.
pub fn prunable(issue: &Issue, now: i64) -> bool {
    let links = &issue.links;
    if issue.fields.state != Category::Closed
        || has_pending(issue)
        || !issue.notes.is_empty()
        || !issue.conflicts.is_empty()
        || !issue.errors.is_empty()
        || !links.issues.is_empty()
        || !links.reviews.is_empty()
        || !links.tasks.is_empty()
        || !links.branches.is_empty()
        || links.prs.iter().any(|pr| pr.source == LinkSource::Local)
        || issue
            .create
            .as_ref()
            .is_some_and(|create| create.post != PostState::Posted)
        || issue.comments.iter().any(|comment| {
            comment.local || comment.deleted_remote || comment.delivery.post != PostState::Posted
        })
    {
        return false;
    }
    let remote = issue
        .remote
        .as_ref()
        .map(|remote| remote.updated_at.as_str());
    let stamps = std::iter::once(issue.updated_at.as_str()).chain(remote);
    clock::latest(stamps)
        .ok()
        .flatten()
        .is_some_and(|last| past_retention(now, last))
}
