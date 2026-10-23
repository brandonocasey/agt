//! The writes of an approval thread and when the host may be asked to approve.

use super::skips::{Skip, Unsupported};
use super::{Item, Planner, Write};
use crate::forge::service::ApproveCap;
use crate::review::commands::approve::blocking_threads;
use crate::review::commands::publish::render::{body, short};
use crate::store::model::{Approval, Comment, RemoteKind, Target, Thread};

impl Planner<'_> {
    /// An approval's writes: a comment, plus the host's approval when formal, and a note on
    /// a formal approval the host does not allow. Only for the head it approved, and only
    /// while no finding blocks approval.
    pub(super) fn approval(
        &self,
        thread: &Thread,
        first: &Comment,
        approval: &Approval,
        target: &Target,
    ) -> std::result::Result<(Vec<Write>, RemoteKind, Option<Skip>), Skip> {
        self.approvable(approval, target)?;
        let head = &self.review.head_sha;
        let text = body(target, thread, first, None, self.attempt);
        let subject = self.connection.subject();
        // A note, not a thread: some hosts make threads resolvable, so they can block merging.
        let note = || {
            self.write(
                Item::Thread {
                    comment: first.id.clone(),
                },
                self.service().note(subject, &text),
                &text,
            )
        };
        let own = self.foreign().is_none();
        match self.caps().approve {
            ApproveCap::Review { self_allowed } if approval.formal && (self_allowed || !own) => {
                let request = self.service().approve(subject, head, Some(&text));
                let write = self.write(
                    Item::ApprovalReview {
                        comment: first.id.clone(),
                    },
                    request,
                    &text,
                );
                Ok((vec![write], RemoteKind::Review, None))
            }
            ApproveCap::Review { .. } => {
                let reason = approval.formal.then(|| Skip::Unsupported {
                    service: self.service().name(),
                    what: Unsupported::SelfApproval {
                        posted_as_comment: true,
                    },
                });
                Ok((vec![note()], RemoteKind::General, reason))
            }
            ApproveCap::Call
                if approval.formal && !approval.approved && approval.refused.is_none() =>
            {
                let approve = Write {
                    item: Item::Approve {
                        thread: thread.id.clone(),
                    },
                    request: Some(self.service().approve(subject, head, None)),
                    sent: vec![None],
                    fallback: None,
                };
                Ok((vec![approve, note()], RemoteKind::General, None))
            }
            ApproveCap::None if approval.formal => {
                let reason = Skip::Unsupported {
                    service: self.service().name(),
                    what: Unsupported::FormalApproval,
                };
                Ok((vec![note()], RemoteKind::General, Some(reason)))
            }
            ApproveCap::Call | ApproveCap::None => Ok((vec![note()], RemoteKind::General, None)),
        }
    }

    /// The host's approval for a posted approval thread that is formal but not yet accepted.
    pub(super) fn pending_approve(
        &self,
        thread: &Thread,
        target: &Target,
    ) -> Option<std::result::Result<Write, Skip>> {
        let approval = thread.approval.as_ref()?;
        if !approval.formal || approval.approved || approval.refused.is_some() {
            return None;
        }
        // Posted as the approving review itself, as when its response was lost and sync
        // claimed it.
        if thread
            .remote
            .as_ref()
            .is_some_and(|remote| remote.kind == RemoteKind::Review)
            || self.held(thread).is_some()
        {
            return None;
        }
        let own = self.foreign().is_none();
        let caps = self.caps();
        if caps.approve == ApproveCap::None {
            return None;
        }
        if own
            && caps.approve
                == (ApproveCap::Review {
                    self_allowed: false,
                })
        {
            return Some(Err(Skip::Unsupported {
                service: self.service().name(),
                what: Unsupported::SelfApproval {
                    posted_as_comment: false,
                },
            }));
        }
        if let Err(reason) = self.approvable(approval, target) {
            return Some(Err(reason));
        }
        Some(Ok(Write {
            item: Item::Approve {
                thread: thread.id.clone(),
            },
            request: Some(self.service().approve(
                self.connection.subject(),
                &self.review.head_sha,
                None,
            )),
            sent: vec![None],
            fallback: None,
        }))
    }

    /// Whether an approval may go out now: at the head it approved, with no blocking finding.
    pub(super) fn approvable(
        &self,
        approval: &Approval,
        target: &Target,
    ) -> std::result::Result<(), Skip> {
        let head = &self.review.head_sha;
        if &approval.head != head {
            return Err(Skip::HeadMoved {
                approved: short(&approval.head).to_owned(),
                head: short(head).to_owned(),
            });
        }
        let blocking: Vec<String> = blocking_threads(target).map(str::to_owned).collect();
        if blocking.is_empty() {
            Ok(())
        } else {
            Err(Skip::OpenFindings(blocking))
        }
    }
}
