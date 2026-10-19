use serde_json::{Value, json};

use super::plan::{Item, Write};
use crate::core::clock;
use crate::core::error::{Error, Result};
use crate::forge::markers::agent_id;
use crate::forge::service::Inline;
use crate::review::commands::Ctx;
use crate::review::commands::sync::{Connection, landed};
use crate::store::model::{
    Approval, Comment, Delivery, PostState, RecordPost, RemoteComment, Target,
};

/// Runs writes in order. Each item is marked `posting` and saved before its call and gets its
/// remote id right after, so a rerun can match a write whose response was lost.
pub(super) fn run_writes(
    ctx: &Ctx,
    connection: &Connection,
    target: &Target,
    writes: Vec<Write>,
    attempt: &str,
    posted: &mut Vec<Value>,
) -> Result<()> {
    for write in writes {
        let Some(request) = &write.request else {
            if let Item::Resolve { thread, .. } = &write.item {
                change(ctx, target, |target| {
                    clear_pending(target, thread);
                    Ok(())
                })?;
            }
            posted.push(json!({ "kind": write.kind(), "items": write.ids(), "local_only": true }));
            continue;
        };
        change(ctx, target, |target| {
            mark(target, &write, attempt);
            Ok(())
        })?;
        let response = match connection.service.send(request) {
            Ok(response) => response,
            // A host may refuse a repeat or ineligible approval; the comment still posts, and
            // the failure is reported instead of blocking every later write. Only a refusal
            // the host stands by (see `TransportError::refused`) is kept, so a timeout or a
            // server error leaves the approval to be sent again.
            Err(error) if matches!(write.item, Item::Approve { .. }) => {
                let reason = error.to_string();
                if error.refused() {
                    change(ctx, target, |target| {
                        set_approval(target, &write.ids()[0], |approval| {
                            approval.refused = Some(reason.clone());
                        });
                        Ok(())
                    })?;
                }
                posted.push(json!({ "kind": write.kind(), "items": write.ids(), "error": reason }));
                continue;
            }
            // The summary comment was deleted on the host: post it anew.
            Err(error)
                if error.status == Some(404)
                    && let Item::Summary {
                        runs,
                        remote: Some(_),
                    } = &write.item =>
            {
                let text = write.sent.first().cloned().flatten().unwrap_or_default();
                let again = Write {
                    item: Item::Summary {
                        runs: runs.clone(),
                        remote: None,
                    },
                    request: Some(connection.service.note(connection.subject(), &text)),
                    sent: write.sent.clone(),
                };
                run_writes(ctx, connection, target, vec![again], attempt, posted)?;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let remote = save(ctx, connection, target, &write.item, &response.body)?;
        if let Item::ApprovalReview { comment } = &write.item {
            let thread = comment.split('/').next().unwrap_or_default().to_owned();
            change(ctx, target, |target| {
                set_approval(target, &thread, |approval| approval.approved = true);
                Ok(())
            })?;
        }
        posted.push(json!({ "kind": write.kind(), "items": write.ids(), "remote_id": remote }));
        if let (Item::Draft { comment }, Some(draft), Inline::DraftThenPublish(drafts)) =
            (&write.item, remote, connection.service.inline())
        {
            let publish = Write {
                item: Item::PublishDraft {
                    comment: comment.clone(),
                    draft: draft.clone(),
                },
                request: Some(drafts.publish_draft(connection.subject(), &draft)),
                sent: vec![None],
            };
            run_writes(ctx, connection, target, vec![publish], attempt, posted)?;
        }
    }
    Ok(())
}

/// Edits the target under its lock; fails if it was deleted since `base` was read.
pub(super) fn change<R>(
    ctx: &Ctx,
    base: &Target,
    edit: impl FnOnce(&mut Target) -> Result<R>,
) -> Result<R> {
    let now = clock::now()?;
    ctx.store.update_existing(&ctx.dir, base, &now, edit)
}

pub(super) fn comment_mut<'a>(target: &'a mut Target, id: &str) -> Option<&'a mut Comment> {
    target
        .threads
        .iter_mut()
        .flat_map(|thread| thread.comments.iter_mut())
        .find(|comment| comment.id == id)
}

pub(super) fn set_approval(target: &mut Target, thread: &str, edit: impl FnOnce(&mut Approval)) {
    if let Some(approval) = target
        .threads
        .iter_mut()
        .find(|local| local.id == thread)
        .and_then(|local| local.approval.as_mut())
    {
        edit(approval);
    }
}

pub(super) fn clear_pending(target: &mut Target, id: &str) {
    if let Some(thread) = target.threads.iter_mut().find(|thread| thread.id == id) {
        thread.resolve_pending = None;
    }
}

pub(super) fn mark(target: &mut Target, write: &Write, attempt: &str) {
    let mut texts = write.sent.iter().cloned();
    let ids = match &write.item {
        Item::Review { comments } => comments.clone(),
        Item::Thread { comment }
        | Item::ApprovalReview { comment }
        | Item::Draft { comment }
        | Item::Reply { comment } => vec![comment.clone()],
        Item::Summary { runs, remote } => {
            let text = texts.next().flatten();
            for (run, head) in runs {
                if let Some(run) = target
                    .runs
                    .iter_mut()
                    .find(|candidate| candidate.id == *run)
                {
                    let previous = run.record.take();
                    let url = previous
                        .as_ref()
                        .filter(|record| record.remote_id == *remote)
                        .and_then(|record| record.url.clone());
                    // What the host may still show stays trusted: the run's own record comment,
                    // or the text this edit replaces.
                    let mut earlier = Vec::new();
                    for body in previous
                        .into_iter()
                        .flat_map(|record| record.earlier.into_iter().chain(record.delivery.sent))
                    {
                        if Some(&body) != text.as_ref() && !earlier.contains(&body) {
                            earlier.push(body);
                        }
                    }
                    let mut delivery = Delivery::new(PostState::Posting);
                    delivery.begin(attempt, text.clone());
                    run.record = Some(RecordPost {
                        delivery,
                        head: head.clone(),
                        remote_id: remote.clone(),
                        url,
                        earlier,
                    });
                }
            }
            return;
        }
        Item::PublishDraft { .. } | Item::Resolve { .. } | Item::Approve { .. } => return,
    };
    for id in ids {
        let text = texts.next().flatten();
        if let Some(comment) = comment_mut(target, &id) {
            comment.delivery.begin(attempt, text);
        }
    }
}

pub(super) fn own(
    id: String,
    url: Option<String>,
    login: &str,
    review_id: Option<String>,
) -> RemoteComment {
    RemoteComment {
        id,
        url,
        login: Some(login.to_owned()),
        is_me: true,
        editor: None,
        review_id,
    }
}

/// Saves what the host returned; returns the new remote id.
pub(super) fn save(
    ctx: &Ctx,
    connection: &Connection,
    target: &Target,
    item: &Item,
    response: &Value,
) -> Result<Option<String>> {
    let login = connection.login.as_str();
    let service = &connection.service;
    let review_url = target.review.as_ref().map(|review| review.url.as_str());
    match item {
        Item::Review { comments } => {
            let (review, listed) = service.review_posted(connection.subject(), response)?;
            let found: Vec<(String, RemoteComment)> = listed
                .into_iter()
                .filter_map(|item| {
                    let local = agent_id(&item.body)?;
                    Some((local, own(item.id, item.url, login, Some(review.clone()))))
                })
                .filter(|(local, _)| comments.contains(local))
                .collect();
            change(ctx, target, |target| {
                for (local, remote) in found {
                    if let Some(comment) = comment_mut(target, &local) {
                        comment.delivery.post = PostState::Posted;
                        comment.remote = Some(remote);
                    }
                }
                Ok(())
            })?;
            Ok(Some(review))
        }
        Item::Thread { comment } | Item::ApprovalReview { comment } | Item::Reply { comment } => {
            let (id, url) = service
                .comment_created(review_url, response)
                .ok_or_else(|| Error(format!("no comment id returned for {comment}")))?;
            change(ctx, target, |target| {
                if let Some(local) = comment_mut(target, comment) {
                    local.delivery.post = PostState::Posted;
                    local.remote = Some(own(id.clone(), url, login, None));
                }
                Ok(())
            })?;
            Ok(Some(id))
        }
        Item::Draft { comment } => {
            let draft = service.draft_created(response).ok_or_else(|| {
                Error(format!(
                    "{} returned no draft id for {comment}",
                    service.name()
                ))
            })?;
            change(ctx, target, |target| {
                if let Some(local) = comment_mut(target, comment) {
                    local.draft_id = Some(draft.clone());
                }
                Ok(())
            })?;
            Ok(Some(draft))
        }
        Item::PublishDraft { draft, .. } => Ok(Some(draft.clone())),
        Item::Approve { thread } => {
            change(ctx, target, |target| {
                set_approval(target, thread, |approval| approval.approved = true);
                Ok(())
            })?;
            Ok(None)
        }
        Item::Resolve { thread, .. } => {
            change(ctx, target, |target| {
                clear_pending(target, thread);
                Ok(())
            })?;
            Ok(None)
        }
        Item::Summary { runs, .. } => {
            let (id, url) = service
                .comment_created(review_url, response)
                .ok_or_else(|| Error("no comment id returned for the summary".into()))?;
            change(ctx, target, |target| {
                for run in target
                    .runs
                    .iter_mut()
                    .filter(|run| runs.iter().any(|(id, _)| *id == run.id))
                {
                    if let Some(record) = run.record.as_mut() {
                        landed(record, id.clone(), url.clone());
                    }
                }
                Ok(())
            })?;
            Ok(Some(id))
        }
    }
}
