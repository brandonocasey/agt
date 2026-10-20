use serde_json::{Value, json};

use crate::core::error::{Result, fail};
use crate::review::cli::ApproveArgs;
use crate::store::model::{Approval, Target, Thread, ThreadType};

use super::Ctx;
use super::findings::{NewThread, author, local_thread};
use super::input::read_body;

/// Records an approval of the head as a general thread for `publish`.
pub(super) fn approve(ctx: &Ctx, args: &ApproveArgs) -> Result<Value> {
    let scope = ctx.scope()?;
    if !scope.head_known {
        return fail("approve needs a head: check out the branch or pass --target");
    }
    let head = scope.head.clone();
    let blocking: Vec<&str> = scope.existing.iter().flat_map(blocking_threads).collect();
    if !blocking.is_empty() {
        return fail(format!(
            "open findings block approval: {}",
            blocking.join(", ")
        ));
    }
    let body = match &args.body {
        Some(body) => read_body(body)?,
        None => format!("Approved at {}.", &head[..head.len().min(12)]),
    };
    let default = ctx.default_author()?;
    let formal = args.formal;
    let thread = ctx.update(&scope, |target, now| {
        // Approving the same head again reuses its approval, so it posts once; `--formal`
        // upgrades a comment approval and retries a refused formal one.
        if let Some(existing) = target.threads.iter_mut().rev().find(|thread| {
            thread
                .approval
                .as_ref()
                .is_some_and(|approval| approval.head == head)
        }) {
            if formal && let Some(approval) = existing.approval.as_mut() {
                approval.formal = true;
                approval.refused = None;
            }
            return Ok(existing.clone());
        }
        let author = author(&args.author, target, default)?;
        let mut thread = local_thread(
            NewThread {
                id: target.next_thread_id(),
                kind: ThreadType::Comment,
                severity: None,
                anchor: None,
                finding: None,
            },
            author,
            body,
            now,
        );
        thread.resolved = true;
        thread.approval = Some(Approval {
            head: head.clone(),
            formal,
            approved: false,
            refused: None,
        });
        target.threads.push(thread.clone());
        Ok(thread)
    })?;
    Ok(json!({ "target": scope.key, "thread": thread }))
}

/// Ids of the open threads that stop an approval of `target`.
pub(super) fn blocking_threads(target: &Target) -> impl Iterator<Item = &str> {
    target
        .threads
        .iter()
        .filter(|thread| is_open(thread) && thread.kind.blocks_approval())
        .map(|thread| thread.id.as_str())
}

/// Open review items: unresolved findings and threads still on the host. A general comment or
/// review body counts only when it is ours: written here, or posted by `publish` (it carries an
/// `agent-review` marker). Other people's, and my own unmarked ones, have no resolved state.
pub(super) fn is_open(thread: &Thread) -> bool {
    let ours = thread
        .comments
        .first()
        .is_some_and(|first| match &first.remote {
            None => true,
            Some(remote) => remote.is_me && crate::forge::markers::agent_id(&first.body).is_some(),
        });
    // An approval is a statement, never something left to resolve.
    thread.approval.is_none()
        && !thread.resolved
        && !thread.deleted_remote
        && (thread.finding.is_some()
            || thread
                .remote
                .as_ref()
                .is_none_or(|remote| remote.resolvable)
            || ours)
}
