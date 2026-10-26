use serde_json::{Value, json};

use super::plan::{Item, Write};
use crate::core::clock;
use crate::core::error::{Error, Result};
use crate::forge::markers::{self, agent_id};
use crate::forge::service::Inline;
use crate::review::commands::Ctx;
use crate::review::commands::sync::Connection;
use crate::review::commands::sync::merge::landed;
use crate::store::model::{
    Approval, Delivery, PostState, RecordPost, RemoteComment, SummaryPart, Target,
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
                let thread = write.ids().remove(0);
                if error.refused() {
                    change(ctx, target, |target| {
                        set_approval(target, &thread, |approval| {
                            approval.refused = Some(reason.clone());
                        });
                        Ok(())
                    })?;
                }
                // When the host will not let you approve (401, 403), the approval's text posts
                // as a comment instead, so it is not lost. Other refusals post nothing, such as
                // a 409 for a head that moved since it was approved.
                let forbidden = matches!(error.status, Some(401 | 403));
                let comment = target
                    .threads
                    .iter()
                    .find(|local| local.id == thread)
                    .and_then(|local| local.comments.first())
                    .map(|first| first.id.clone());
                let again = match (forbidden, comment, &write.fallback) {
                    (true, Some(comment), Some(text)) => Some(Write {
                        item: Item::Thread { comment },
                        request: Some(connection.service.note(connection.subject(), text)),
                        sent: vec![Some(text.clone())],
                        fallback: None,
                    }),
                    _ => None,
                };
                let mut entry =
                    json!({ "kind": write.kind(), "items": write.ids(), "error": reason });
                if again.is_some() {
                    entry["fallback"] = json!("comment");
                }
                posted.push(entry);
                if let Some(again) = again {
                    run_writes(ctx, connection, target, vec![again], attempt, posted)?;
                }
                continue;
            }
            // The summary comment or one of its parts was deleted on the host: post it anew.
            Err(error)
                if error.status == Some(404)
                    && let Some(item) = reposted(&write.item) =>
            {
                let text = write.sent.first().cloned().flatten().unwrap_or_default();
                let again = Write {
                    item,
                    request: Some(connection.service.note(connection.subject(), &text)),
                    sent: write.sent.clone(),
                    fallback: None,
                };
                run_writes(ctx, connection, target, vec![again], attempt, posted)?;
                continue;
            }
            // The host refused where the finding goes: post it as a general thread instead.
            Err(error) if error.refused() && write.fallback.is_some() => {
                let text = write.fallback.clone().unwrap_or_default();
                let comment = write.ids().remove(0);
                posted.push(json!({
                    "kind": write.kind(),
                    "items": write.ids(),
                    "error": error.to_string(),
                    "fallback": "thread",
                }));
                // A draft the host will not publish is dropped, so it does not stay beside
                // the thread that replaces it.
                if let (Item::PublishDraft { draft, .. }, Inline::DraftThenPublish(drafts)) =
                    (&write.item, connection.service.inline())
                {
                    let delete = drafts.delete_draft(connection.subject(), draft);
                    if let Err(error) = connection.service.send(&delete) {
                        posted.push(json!({
                            "kind": "delete-draft",
                            "items": write.ids(),
                            "error": error.to_string(),
                        }));
                    }
                    change(ctx, target, |target| {
                        if let Some(local) = target.comment_mut(&comment) {
                            local.draft_id = None;
                        }
                        Ok(())
                    })?;
                }
                let again = Write {
                    item: Item::Placed { comment },
                    request: Some(connection.service.thread(connection.subject(), &text)),
                    sent: vec![Some(text)],
                    fallback: None,
                };
                run_writes(ctx, connection, target, vec![again], attempt, posted)?;
                continue;
            }
            // A file thread the host refuses where a general comment may not stand in for it:
            // the finding stays unposted, the failure is reported and the rest goes on.
            Err(error) if error.refused() && matches!(write.item, Item::Placed { .. }) => {
                posted.push(json!({
                    "kind": write.kind(),
                    "items": write.ids(),
                    "error": error.to_string(),
                }));
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
                fallback: write.fallback.clone(),
            };
            run_writes(ctx, connection, target, vec![publish], attempt, posted)?;
        }
    }
    Ok(())
}

/// The summary item that posts anew what an edit found deleted.
fn reposted(item: &Item) -> Option<Item> {
    match item {
        Item::Summary {
            runs,
            remote: Some(_),
        } => Some(Item::Summary {
            runs: runs.clone(),
            remote: None,
        }),
        Item::SummaryPart {
            runs,
            number,
            remote: Some(_),
        } => Some(Item::SummaryPart {
            runs: runs.clone(),
            number: *number,
            remote: None,
        }),
        _ => None,
    }
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

pub(super) fn set_approval(target: &mut Target, thread: &str, edit: impl FnOnce(&mut Approval)) {
    if let Some(approval) = target
        .thread_mut(thread)
        .and_then(|local| local.approval.as_mut())
    {
        edit(approval);
    }
}

pub(super) fn clear_pending(target: &mut Target, id: &str) {
    if let Some(thread) = target.thread_mut(id) {
        thread.resolve_pending = None;
    }
}

pub(super) fn mark(target: &mut Target, write: &Write, attempt: &str) {
    let mut texts = write.sent.iter().cloned();
    let ids = match &write.item {
        Item::Review { comments, bodies } => comments.iter().chain(bodies).cloned().collect(),
        Item::Thread { comment }
        | Item::Placed { comment }
        | Item::ApprovalReview { comment }
        | Item::Draft { comment }
        | Item::Reply { comment } => vec![comment.clone()],
        Item::Summary { runs, remote } => {
            let text = texts.next().flatten();
            // The parts of the comment the runs share, for a run that joins it now.
            let mut shared: Vec<SummaryPart> = Vec::new();
            for (run, _) in runs {
                if let Some(Some(record)) = target.record_mut(run)
                    && remote.is_some()
                    && record.remote_id == *remote
                    && shared.is_empty()
                {
                    shared.clone_from(&record.parts);
                }
            }
            for (run, head) in runs {
                if let Some(record) = target.record_mut(run) {
                    let previous = record.take();
                    let parts = match previous.as_ref().map(|record| record.parts.clone()) {
                        Some(parts) if !parts.is_empty() => parts,
                        _ => shared.clone(),
                    };
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
                    *record = Some(RecordPost {
                        delivery,
                        head: head.clone(),
                        remote_id: remote.clone(),
                        url,
                        earlier,
                        parts,
                    });
                }
            }
            return;
        }
        Item::SummaryPart {
            runs,
            number,
            remote,
        } => {
            let text = texts.next().flatten();
            let index = number - 2;
            for run in runs {
                let Some(Some(record)) = target.record_mut(run) else {
                    continue;
                };
                if record.parts.len() <= index {
                    record.parts.resize_with(index + 1, || SummaryPart {
                        delivery: Delivery::new(PostState::Pending),
                        remote_id: None,
                        url: None,
                    });
                }
                let part = &mut record.parts[index];
                if part.remote_id != *remote {
                    part.url = None;
                }
                // The text this edit replaces stays trusted until the edit lands.
                if let Some(old) = part.delivery.sent.take()
                    && Some(&old) != text.as_ref()
                    && !record.earlier.contains(&old)
                {
                    record.earlier.push(old);
                }
                part.delivery.begin(attempt, text.clone());
                part.remote_id.clone_from(remote);
            }
            return;
        }
        Item::PublishDraft { .. } | Item::Resolve { .. } | Item::Approve { .. } => return,
    };
    for id in ids {
        let text = texts.next().flatten();
        if let Some(comment) = target.comment_mut(&id) {
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
        Item::Review { comments, bodies } => {
            let (review, listed) = service.review_posted(connection.subject(), response)?;
            let found: Vec<(String, RemoteComment)> = listed
                .into_iter()
                .filter_map(|item| {
                    let local = agent_id(&item.body)?;
                    Some((local, own(item.id, item.url, login, Some(review.clone()))))
                })
                .filter(|(local, _)| comments.contains(local))
                .collect();
            // The comments in the review's body: no thread of their own, so the review is
            // their remote comment.
            let body = service.comment_created(review_url, response);
            change(ctx, target, |target| {
                for (local, remote) in found {
                    if let Some(comment) = target.comment_mut(&local) {
                        comment.delivery.post = PostState::Posted;
                        comment.remote = Some(remote);
                    }
                }
                for local in bodies {
                    if let (Some(comment), Some((id, url))) =
                        (target.comment_mut(local), body.clone())
                    {
                        comment.delivery.post = PostState::Posted;
                        comment.remote = Some(own(id, url, login, Some(review.clone())));
                    }
                }
                Ok(())
            })?;
            Ok(Some(review))
        }
        Item::Thread { comment }
        | Item::Placed { comment }
        | Item::ApprovalReview { comment }
        | Item::Reply { comment } => {
            let (id, url) = service
                .comment_created(review_url, response)
                .ok_or_else(|| Error(format!("no comment id returned for {comment}")))?;
            change(ctx, target, |target| {
                if let Some(local) = target.comment_mut(comment) {
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
                if let Some(local) = target.comment_mut(comment) {
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
                for (run, _) in runs {
                    if let Some(Some(record)) = target.record_mut(run) {
                        landed(record, id.clone(), url.clone());
                    }
                }
                Ok(())
            })?;
            Ok(Some(id))
        }
        Item::SummaryPart { runs, number, .. } => {
            let (id, url) = service
                .comment_created(review_url, response)
                .ok_or_else(|| {
                    Error(format!("no comment id returned for summary part {number}"))
                })?;
            change(ctx, target, |target| {
                for run in runs {
                    if let Some(Some(record)) = target.record_mut(run)
                        && let Some(part) = record.parts.get_mut(number - 2)
                    {
                        part.delivery.post = PostState::Posted;
                        part.remote_id = Some(id.clone());
                        part.url.clone_from(&url);
                        let part_id = markers::summary_part_id(*number);
                        record
                            .earlier
                            .retain(|body| agent_id(body).as_deref() != Some(part_id.as_str()));
                    }
                }
                Ok(())
            })?;
            Ok(Some(id))
        }
    }
}
