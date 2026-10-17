//! Which stored issues `agt prune` may drop.

use crate::core::clock;
use crate::store::issues::{Category, Issue};
use crate::store::model::PostState;
use crate::store::prune::PRUNE_AFTER_SECS;

use super::merge::has_pending;

/// Whether `issue` closed at least 90 days before `now` and holds nothing that exists only
/// here: no queued change, unsent comment or create, conflict, failed send, note or local-only
/// comment. An open issue is never prunable. The close time is the newer of the local and the
/// host's last update, since neither records the close itself.
pub fn prunable(issue: &Issue, now: i64) -> bool {
    if issue.fields.state != Category::Closed
        || has_pending(issue)
        || !issue.notes.is_empty()
        || !issue.conflicts.is_empty()
        || !issue.errors.is_empty()
        || issue
            .create
            .as_ref()
            .is_some_and(|create| create.post != PostState::Posted)
        || issue
            .comments
            .iter()
            .any(|comment| comment.local || comment.delivery.post != PostState::Posted)
    {
        return false;
    }
    let remote = issue
        .remote
        .as_ref()
        .and_then(|remote| clock::parse(&remote.updated_at).ok());
    let Some(last) = clock::parse(&issue.updated_at)
        .ok()
        .into_iter()
        .chain(remote)
        .max()
    else {
        return false;
    };
    now - last >= PRUNE_AFTER_SECS
}
