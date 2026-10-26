//! The writes of an approval thread and when the host may be asked to approve.

use super::skips::{Skip, Unsupported};
use super::{Item, Planner, Write};
use crate::forge::markers;
use crate::forge::service::{ApproveCap, Inline};
use crate::review::commands::approve::blocking_threads;
use crate::review::commands::publish::render::{body, short};
use crate::store::model::{Approval, Comment, RemoteKind, Target, Thread};

/// What an approval thread asks of the host.
pub(super) struct Approved {
    pub(super) writes: Vec<Write>,
    /// The remote kind the thread gets; none when nothing is posted for it.
    pub(super) kind: Option<RemoteKind>,
    /// Why the formal part is not sent, when it is not.
    pub(super) note: Option<Skip>,
}

impl Approved {
    fn nothing(note: Option<Skip>) -> Self {
        Self {
            writes: Vec::new(),
            kind: None,
            note,
        }
    }
}

impl Planner<'_> {
    /// An approval's writes: the host's approval when formal, which posts no comment. The
    /// review GitHub takes carries only the hidden marker; on your own PR, which GitHub does
    /// not let you approve, a comment review carries the approval text instead, as a comment
    /// does when GitLab refuses the approval (see `run_writes`). A non-formal
    /// approval is recorded locally, and the summary headline shows it. Only for the head it
    /// approved, and only while no finding blocks approval.
    pub(super) fn approval(
        &self,
        thread: &Thread,
        first: &Comment,
        approval: &Approval,
        target: &Target,
    ) -> std::result::Result<Approved, Skip> {
        let head = &self.review.head_sha;
        let subject = self.connection.subject();
        let own = self.foreign().is_none();
        match self.caps().approve {
            ApproveCap::Review { self_allowed } if approval.formal && (self_allowed || !own) => {
                self.approvable(approval, target)?;
                let text = markers::agent(&first.id, Some(self.attempt));
                let request = self.service().approve(subject, head, Some(&text));
                let write = self.write(
                    Item::ApprovalReview {
                        comment: first.id.clone(),
                    },
                    request,
                    &text,
                );
                Ok(Approved {
                    writes: vec![write],
                    kind: Some(RemoteKind::Review),
                    note: None,
                })
            }
            ApproveCap::Review { .. } if approval.formal => {
                self.approvable(approval, target)?;
                let skip = |commented| Skip::Unsupported {
                    service: self.service().name(),
                    what: Unsupported::SelfApproval { commented },
                };
                let Inline::ReviewBatch(poster) = self.service().inline() else {
                    return Ok(Approved::nothing(Some(skip(false))));
                };
                let text = body(target, thread, first, None, self.attempt);
                let request = poster.review_batch(subject, head, &text, &[]);
                let write = self.write(
                    Item::Thread {
                        comment: first.id.clone(),
                    },
                    request,
                    &text,
                );
                Ok(Approved {
                    writes: vec![write],
                    kind: Some(RemoteKind::Review),
                    note: Some(skip(true)),
                })
            }
            ApproveCap::Review { .. } => Ok(Approved::nothing(None)),
            ApproveCap::Call
                if approval.formal && !approval.approved && approval.refused.is_none() =>
            {
                self.approvable(approval, target)?;
                let approve = Write {
                    item: Item::Approve {
                        thread: thread.id.clone(),
                    },
                    request: Some(self.service().approve(subject, head, None)),
                    sent: vec![None],
                    fallback: Some(body(target, thread, first, None, self.attempt)),
                };
                Ok(Approved {
                    writes: vec![approve],
                    kind: None,
                    note: None,
                })
            }
            // Refused before the comment that carries it posted.
            ApproveCap::Call if approval.formal && approval.refused.is_some() => {
                self.approvable(approval, target)?;
                let text = body(target, thread, first, None, self.attempt);
                let request = self.service().note(subject, &text);
                let write = self.write(
                    Item::Thread {
                        comment: first.id.clone(),
                    },
                    request,
                    &text,
                );
                Ok(Approved {
                    writes: vec![write],
                    kind: Some(RemoteKind::General),
                    note: None,
                })
            }
            ApproveCap::None if approval.formal => {
                let reason = Skip::Unsupported {
                    service: self.service().name(),
                    what: Unsupported::FormalApproval,
                };
                Ok(Approved::nothing(Some(reason)))
            }
            ApproveCap::Call | ApproveCap::None => Ok(Approved::nothing(None)),
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
                what: Unsupported::SelfApproval { commented: false },
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
