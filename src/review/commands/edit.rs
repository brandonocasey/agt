//! `edit` and `delete` change or remove one comment. A comment not posted yet changes in the
//! store. A posted one is marked, and `publish` sends the change.

use serde_json::{Value, json};

use super::Ctx;
use super::input::read_body;
use crate::core::error::{Error, Result, fail};
use crate::review::cli::{DeleteArgs, EditArgs};
use crate::store::model::{Comment, PostState, Target};

/// Whether the host has the comment. Imported comments and ones being posted cannot change.
fn is_posted(comment: &Comment) -> Result<bool> {
    let id = &comment.id;
    if comment.is_imported() {
        return fail(format!(
            "comment {id} was imported from the host and is read-only"
        ));
    }
    match comment.delivery.post {
        PostState::Pending => Ok(false),
        PostState::Posted => Ok(true),
        PostState::Posting if comment.draft_id.is_some() => fail(format!(
            "comment {id} is a draft that is not published; run publish to publish it first"
        )),
        PostState::Posting => fail(format!(
            "comment {id} is being posted; run sync first to see whether it landed"
        )),
    }
}

/// The index of the thread holding comment `id`, and the comment's index in it.
pub(super) fn locate(target: &Target, id: &str) -> Result<(usize, usize)> {
    target
        .threads
        .iter()
        .enumerate()
        .find_map(|(thread, found)| {
            let comment = found.comments.iter().position(|comment| comment.id == id)?;
            Some((thread, comment))
        })
        .ok_or_else(|| Error(format!("comment {id} not found")))
}

pub(super) fn edit(ctx: &Ctx, args: &EditArgs) -> Result<Value> {
    let scope = ctx.scope()?;
    if scope.existing.is_none() {
        return fail(format!("comment {} not found", args.id));
    }
    let body = read_body(&args.body)?;
    let thread = ctx.update(&scope, |target, now| {
        let (at, index) = locate(target, &args.id)?;
        let thread = &mut target.threads[at];
        let comment = &mut thread.comments[index];
        let posted = is_posted(comment)?;
        if comment.delete_pending {
            return fail(format!("comment {} is queued for deletion", args.id));
        }
        comment.body = body;
        comment.edit_pending = posted;
        thread.updated_at = now.to_owned();
        Ok(thread.clone())
    })?;
    Ok(json!({ "target": scope.key, "thread": thread }))
}

pub(super) fn delete(ctx: &Ctx, args: &DeleteArgs) -> Result<Value> {
    let scope = ctx.scope()?;
    if scope.existing.is_none() {
        return fail(format!("comment {} not found", args.id));
    }
    let (posted, thread_removed) = ctx.update(&scope, |target, now| {
        let (at, index) = locate(target, &args.id)?;
        let thread = &mut target.threads[at];
        let posted = is_posted(&thread.comments[index])?;
        thread.updated_at = now.to_owned();
        if posted {
            let comment = &mut thread.comments[index];
            comment.delete_pending = true;
            comment.edit_pending = false;
            return Ok((true, false));
        }
        if index > 0 {
            thread.comments.remove(index);
            return Ok((false, false));
        }
        target.threads.remove(at);
        Ok((false, true))
    })?;
    Ok(json!({
        "target": scope.key,
        "deleted": args.id,
        "pending": posted,
        "thread_removed": thread_removed,
    }))
}
