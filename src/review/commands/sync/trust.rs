//! What sync may believe of fetched comments.

use std::collections::HashSet;

use crate::forge::CommentData;
use crate::forge::markers::{agent_id, is_landed_as};
use crate::forge::service::{Capabilities, ReviewService};
use crate::store::model::{Comment, Target};

/// Trust rule: the author is the authenticated user and nobody else edited the comment.
pub(super) fn trusted(comment: &CommentData, login: &str) -> bool {
    comment.login.as_deref() == Some(login)
        && comment
            .editor
            .as_deref()
            .is_none_or(|editor| editor == login)
}

/// What sync may believe of fetched comments. GitLab notes show no editor, so there only
/// text this store sent is trusted.
pub(super) struct Trust<'a> {
    pub(super) login: &'a str,
    pub(super) caps: &'a Capabilities,
    pub(super) service: &'a dyn ReviewService,
    sent: HashSet<String>,
    pub(super) killed: HashSet<String>,
}

impl<'a> Trust<'a> {
    /// The context of merging into `target`: its sent texts and killed finding ids.
    pub(super) fn for_target(
        target: &Target,
        login: &'a str,
        service: &'a dyn ReviewService,
    ) -> Self {
        let comments = target
            .threads
            .iter()
            .flat_map(|thread| &thread.comments)
            .filter_map(|comment| comment.delivery.sent.as_deref());
        let records = target
            .runs
            .iter()
            .filter_map(|run| run.record.as_ref())
            .chain(target.qa_runs.iter().filter_map(|run| run.record.as_ref()))
            .flat_map(|record| {
                record.delivery.sent.iter().chain(&record.earlier).chain(
                    record
                        .parts
                        .iter()
                        .filter_map(|part| part.delivery.sent.as_ref()),
                )
            })
            .map(String::as_str);
        let killed = target
            .runs
            .iter()
            .flat_map(|run| &run.passes)
            .flat_map(|pass| &pass.killed)
            .map(|killed| killed.id.clone())
            .collect();
        Self::new(login, service, comments.chain(records), killed)
    }

    /// `sent` are the texts this store posted; `killed` the ids of findings that no longer
    /// have a thread but may not be reused.
    pub(super) fn new<'t>(
        login: &'a str,
        service: &'a dyn ReviewService,
        sent: impl Iterator<Item = &'t str>,
        killed: HashSet<String>,
    ) -> Self {
        Self {
            login,
            caps: service.caps(),
            service,
            sent: sent.map(|text| text.trim().to_owned()).collect(),
            killed,
        }
    }

    pub(super) fn sent_text(&self, local: Option<&str>, comment: &CommentData) -> bool {
        self.caps.edits_attributed || local.is_some_and(|local| local.trim() == comment.body.trim())
    }

    /// A trusted comment, on GitLab only with a body this store sent.
    pub(super) fn verified(&self, comment: &CommentData) -> bool {
        trusted(comment, self.login)
            && (self.caps.edits_attributed || self.sent.contains(comment.body.trim()))
    }

    /// The agent-review id of a verified comment.
    pub(super) fn own_id(&self, comment: &CommentData) -> Option<String> {
        self.verified(comment)
            .then(|| agent_id(&comment.body))
            .flatten()
    }

    /// Whether `incoming` is the landed write of local `comment`: one this store is posting,
    /// marked with its id and the same attempt. Another store's ids repeat, so a pending
    /// comment is never claimed.
    pub(super) fn claims(&self, comment: &Comment, incoming: &CommentData) -> bool {
        comment.remote.is_none()
            && trusted(incoming, self.login)
            && is_landed_as(&comment.delivery, &comment.id, &incoming.body)
            && self.sent_text(comment.delivery.sent.as_deref(), incoming)
    }
}
