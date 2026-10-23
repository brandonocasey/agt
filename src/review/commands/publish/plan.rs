mod approval;
mod placement;
pub(super) mod skips;

use self::approval::Approved;
use self::placement::Spot;
use self::skips::{Held, Skip, Skipped, Unsupported};

use serde_json::{Value, json};

use super::Options;

use super::render::{body, location, paragraph, record_head};
use super::summary::{Covered, Links, QaSummary, summary_texts};
use crate::core::error::{Error, Result};
use crate::forge::anchors;
use crate::forge::markers;
use crate::forge::service::{Capabilities, DraftPoster, Inline, Position, Request, ReviewService};
use crate::git::{CommitInfo, Repo};
use crate::review::commands::Ctx;
use crate::review::commands::qa::latest;
use crate::review::commands::status::run_reviews_at;
use crate::review::commands::sync::Connection;
use crate::review::commands::view::author_label;
use crate::store::model::{
    Author, Comment, PostState, QaRun, RecordPost, RemoteComment, RemoteKind, RemoteThread, Review,
    Run, Side, Target, Thread,
};

/// Stands for the id of the draft a dry run has not created yet.
pub(super) const NEW_DRAFT: &str = "<draft id>";

/// What one write call posts, which says how to mark and save it.
pub(super) enum Item {
    /// A review holding the in-diff comments of one reviewed commit, and in its body the
    /// comments that have no diff line.
    Review {
        comments: Vec<String>,
        bodies: Vec<String>,
    },
    /// A thread's first comment as a general comment or discussion.
    Thread {
        comment: String,
    },
    /// An approval posted as a review that carries its comment, which approves the head.
    ApprovalReview {
        comment: String,
    },
    /// A draft note, published right after it is created.
    Draft {
        comment: String,
    },
    PublishDraft {
        comment: String,
        draft: String,
    },
    Reply {
        comment: String,
    },
    Resolve {
        thread: String,
        resolved: bool,
    },
    /// The PR/MR's one summary comment, created once and then edited in place: `runs` are
    /// the runs it covers with the heads their records name, `remote` the comment it edits.
    Summary {
        runs: Vec<(String, String)>,
        remote: Option<String>,
    },
    /// Part `number` of a summary too long for one comment, edited in place like the summary.
    SummaryPart {
        runs: Vec<String>,
        number: usize,
        remote: Option<String>,
    },
    /// The host's approval of the PR/MR head, sent before the approval comment.
    Approve {
        thread: String,
    },
}

pub(super) struct Write {
    pub(super) item: Item,
    /// `None` for a change that is only recorded locally.
    pub(super) request: Option<Request>,
    /// The text each item of the write sends, in the order of its ids.
    pub(super) sent: Vec<Option<String>>,
    /// The text of a general thread to post instead when the host refuses this write, so a
    /// finding it cannot place is not lost.
    pub(super) fallback: Option<String>,
}

impl Write {
    pub(super) fn kind(&self) -> &'static str {
        match &self.item {
            Item::Review { .. } => "review",
            Item::Thread { .. } | Item::ApprovalReview { .. } => "thread",
            Item::Draft { .. } => "draft",
            Item::PublishDraft { .. } => "publish-draft",
            Item::Reply { .. } => "reply",
            Item::Resolve { resolved: true, .. } => "resolve",
            Item::Resolve { .. } => "unresolve",
            Item::Summary { .. } | Item::SummaryPart { .. } => "summary",
            Item::Approve { .. } => "approve",
        }
    }

    pub(super) fn ids(&self) -> Vec<String> {
        match &self.item {
            Item::Review { comments, bodies } => comments.iter().chain(bodies).cloned().collect(),
            Item::Thread { comment }
            | Item::ApprovalReview { comment }
            | Item::Draft { comment }
            | Item::PublishDraft { comment, .. }
            | Item::Reply { comment } => vec![comment.clone()],
            Item::Resolve { thread, .. } | Item::Approve { thread } => vec![thread.clone()],
            Item::Summary { .. } => vec![markers::SUMMARY_ID.to_owned()],
            Item::SummaryPart { number, .. } => vec![markers::summary_part_id(*number)],
        }
    }

    /// The dry-run entries: a draft is followed by the call that publishes it.
    pub(super) fn views(&self, connection: &Connection) -> Vec<Value> {
        let service = &connection.service;
        let call = |request: Option<&Request>| {
            json!({
                "kind": self.kind(),
                "items": self.ids(),
                "call": request.map(|request| service.render(request)).unwrap_or_default(),
                "input": request.and_then(|request| request.body.as_ref()),
            })
        };
        let mut views = vec![call(self.request.as_ref())];
        if let (Item::Draft { comment }, Inline::DraftThenPublish(drafts)) =
            (&self.item, service.inline())
        {
            let publish = drafts.publish_draft(connection.subject(), NEW_DRAFT);
            let write = Write {
                item: Item::PublishDraft {
                    comment: comment.clone(),
                    draft: NEW_DRAFT.to_owned(),
                },
                request: Some(publish),
                sent: vec![None],
                fallback: None,
            };
            views.extend(write.views(connection));
        }
        views
    }
}

/// The in-diff comments of one reviewed commit, posted as one review.
pub(super) struct Batch {
    pub(super) commit: String,
    pub(super) ids: Vec<String>,
    pub(super) comments: Vec<(Position, String)>,
}

/// A comment with no diff line that goes in the body of a review: its id and its text.
struct Paragraph {
    id: String,
    text: String,
}

/// The posting plan for one target state; built the same way for a dry run and a real one.
pub(super) struct Planner<'a> {
    ctx: &'a Ctx,
    attempt: &'a str,
    /// `--only`: the thread ids to post; empty means all.
    only: &'a [String],
    /// The approval thread `publish --approve` recorded; held by neither `only` nor a foreign PR/MR.
    approval: Option<&'a str>,
    run: Option<&'a str>,
    repo: &'a Repo,
    connection: &'a Connection,
    review: &'a Review,
    /// The PR/MR diff's files, read once when a comment does not fit the diff.
    pr_files: std::cell::OnceCell<Vec<anchors::FileDiff>>,
}

/// Only agents and the authenticated user speak through `publish`.
pub(super) fn allowed(author: &Author, login: &str) -> bool {
    match author {
        Author::Agent { .. } => true,
        Author::Human { login: name, .. } => name == login,
    }
}

impl<'a> Planner<'a> {
    pub(super) fn new(
        ctx: &'a Ctx,
        connection: &'a Connection,
        target: &'a Target,
        attempt: &'a str,
        options: &'a Options,
    ) -> Result<Self> {
        let review = target
            .review
            .as_ref()
            .ok_or_else(|| Error(format!("target {} has no synced PR/MR", target.key)))?;
        Ok(Self {
            ctx,
            attempt,
            only: &options.only,
            approval: options.approval.as_deref(),
            run: options.run.as_deref(),
            repo: &ctx.repo,
            connection,
            review,
            pr_files: std::cell::OnceCell::new(),
        })
    }

    fn service(&self) -> &dyn ReviewService {
        self.connection.service.as_ref()
    }

    fn caps(&self) -> &Capabilities {
        self.connection.service.caps()
    }

    fn write(&self, item: Item, request: Request, text: &str) -> Write {
        Write {
            item,
            request: Some(request),
            sent: vec![Some(text.to_owned())],
            fallback: None,
        }
    }

    /// New threads: in-diff comments as the service takes them (one review per reviewed commit,
    /// or a draft note each, published one by one), else general comments. Also returns the
    /// remote kind each thread gets.
    pub(super) fn threads(
        &self,
        target: &Target,
    ) -> (Vec<Write>, Vec<Skipped>, Vec<(String, RemoteKind)>) {
        let mut writes = Vec::new();
        let mut skipped = Vec::new();
        let mut expected = Vec::new();
        let mut reviews: Vec<Batch> = Vec::new();
        let mut paragraphs: Vec<Paragraph> = Vec::new();
        for thread in &target.threads {
            if !self.selected_thread(thread) {
                continue;
            }
            let Some(first) = thread.comments.first() else {
                continue;
            };
            if first.local {
                continue;
            }
            if thread.remote.is_some()
                || thread.deleted_remote
                || first.remote.is_some()
                || first.delivery.post == PostState::Posted
            {
                // An approval posted as a comment, then made formal or refused and retried.
                match self.pending_approve(thread, target) {
                    Some(Ok(write)) => writes.push(write),
                    Some(Err(reason)) => {
                        skipped.push(Skipped::new(&format!("{}/formal", thread.id), reason))
                    }
                    None => {}
                }
                continue;
            }
            // A finding or QA thread settled before it was ever posted asks nothing of the
            // host; the summary counts it.
            if found(thread) && thread.resolved {
                continue;
            }
            if let Some(reason) = self.blocked(target, thread, first) {
                skipped.push(Skipped::new(&thread.id, reason));
                continue;
            }
            if let (Some(draft), Inline::DraftThenPublish(drafts)) =
                (&first.draft_id, self.service().inline())
            {
                writes.push(self.publish_draft(drafts, &first.id, draft));
                expected.push((thread.id.clone(), RemoteKind::Discussion));
                continue;
            }
            if let Some(approval) = &thread.approval {
                match self.approval(thread, first, approval, target) {
                    Ok(Approved {
                        writes: mut approve,
                        kind,
                        note,
                    }) => {
                        writes.append(&mut approve);
                        if let Some(kind) = kind {
                            expected.push((thread.id.clone(), kind));
                        }
                        if let Some(note) = note {
                            skipped.push(Skipped::new(&format!("{}/formal", thread.id), note));
                        }
                    }
                    Err(reason) => skipped.push(Skipped::new(&thread.id, reason)),
                }
                continue;
            }
            let spot = thread
                .anchor
                .as_ref()
                .map_or(Spot::General, |anchor| self.spot(anchor));
            if matches!(spot, Spot::Outside | Spot::OtherFile(_))
                && let Some(write) = self.file_thread(target, thread, first)
            {
                writes.push(write);
                expected.push((thread.id.clone(), RemoteKind::ReviewThread));
                continue;
            }
            if let Some(placed) = self.in_body(target, thread, first, &spot) {
                match placed {
                    Ok(paragraph) => paragraphs.push(paragraph),
                    Err(reason) => skipped.push(Skipped::new(&thread.id, reason)),
                }
                continue;
            }
            // A general comment the host cannot resolve would stay on the PR/MR after the fix;
            // the summary lists the open finding instead. Someone else's PR/MR gets no summary,
            // so there the comment is the only place the finding can go.
            if matches!(spot, Spot::General | Spot::OtherFile(_))
                && found(thread)
                && !self.caps().general_resolvable
                && self.foreign().is_none()
            {
                skipped.push(Skipped::new(&thread.id, Skip::InSummary));
                continue;
            }
            match spot {
                Spot::InDiff(position) => {
                    let text = body(target, thread, first, None, self.attempt);
                    match self.service().inline() {
                        Inline::ReviewBatch(_) => {
                            let commit = position.commit.clone();
                            let comment = (position, text);
                            match reviews.iter_mut().find(|batch| batch.commit == commit) {
                                Some(batch) => {
                                    batch.ids.push(first.id.clone());
                                    batch.comments.push(comment);
                                }
                                None => reviews.push(Batch {
                                    commit,
                                    ids: vec![first.id.clone()],
                                    comments: vec![comment],
                                }),
                            }
                            expected.push((thread.id.clone(), RemoteKind::ReviewThread));
                        }
                        Inline::DraftThenPublish(drafts) => {
                            let request = drafts.draft(self.connection.subject(), &text, &position);
                            let mut write = self.write(
                                Item::Draft {
                                    comment: first.id.clone(),
                                },
                                request,
                                &text,
                            );
                            // The host may refuse the position: the finding then posts as a
                            // general thread that names its lines.
                            write.fallback = thread.anchor.as_ref().map(|anchor| {
                                let lines = location(anchor);
                                body(target, thread, first, Some(&lines), self.attempt)
                            });
                            writes.push(write);
                            expected.push((thread.id.clone(), RemoteKind::Discussion));
                        }
                    }
                }
                Spot::Outside => {
                    skipped.push(Skipped::new(&thread.id, Skip::OutsideDiff));
                }
                Spot::General | Spot::OtherFile(_) => {
                    let location = match &spot {
                        Spot::OtherFile(location) => Some(location.as_str()),
                        _ => None,
                    };
                    let text = body(target, thread, first, location, self.attempt);
                    let request = self.service().thread(self.connection.subject(), &text);
                    writes.push(self.write(
                        Item::Thread {
                            comment: first.id.clone(),
                        },
                        request,
                        &text,
                    ));
                    expected.push((thread.id.clone(), self.caps().general_kind));
                }
            }
        }
        let writes = self
            .review_writes(reviews, paragraphs)
            .into_iter()
            .chain(writes)
            .collect();
        (writes, skipped, expected)
    }

    /// Where a comment with no diff line goes when an agent posts no Conversation comment: in
    /// the body of a review on someone else's PR; on our own it stays in the summary (a
    /// finding) or is not posted. `None` where the comment goes elsewhere.
    fn in_body(
        &self,
        target: &Target,
        thread: &Thread,
        first: &Comment,
        spot: &Spot,
    ) -> Option<std::result::Result<Paragraph, Skip>> {
        let location = match spot {
            Spot::General => None,
            Spot::OtherFile(location) => Some(location.as_str()),
            _ => return None,
        };
        if !self.in_review_body(first) {
            return None;
        }
        Some(if self.foreign().is_some() {
            Ok(Paragraph {
                id: first.id.clone(),
                text: paragraph(target, thread, first, location),
            })
        } else if found(thread) {
            Err(Skip::InSummary)
        } else {
            Err(Skip::NoConversation)
        })
    }

    /// The reviews that carry the in-diff comments of each commit, the first also holding the
    /// comments with no diff line; a review of only a body at the head when no commit has any.
    fn review_writes(&self, mut reviews: Vec<Batch>, paragraphs: Vec<Paragraph>) -> Vec<Write> {
        if !paragraphs.is_empty() && reviews.is_empty() {
            reviews.push(Batch {
                commit: self.review.head_sha.clone(),
                ids: Vec::new(),
                comments: Vec::new(),
            });
        }
        let mut paragraphs = Some(paragraphs);
        reviews
            .into_iter()
            .filter_map(
                |Batch {
                     commit,
                     ids,
                     comments,
                 }| {
                    // Only a service that batches collects any.
                    let Inline::ReviewBatch(batches) = self.service().inline() else {
                        return None;
                    };
                    let held = paragraphs.take().unwrap_or_default();
                    let marker = markers::agent(&markers::review_id(&commit), None);
                    let summary = held
                        .iter()
                        .map(|held| held.text.as_str())
                        .chain([marker.as_str()])
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    let request = batches.review_batch(
                        self.connection.subject(),
                        &commit,
                        &summary,
                        &comments,
                    );
                    let sent = comments
                        .into_iter()
                        .map(|(_, text)| Some(text))
                        .chain(held.iter().map(|held| Some(held.text.clone())))
                        .collect();
                    Some(Write {
                        item: Item::Review {
                            comments: ids,
                            bodies: held.into_iter().map(|held| held.id).collect(),
                        },
                        request: Some(request),
                        sent,
                        fallback: None,
                    })
                },
            )
            .collect()
    }

    fn publish_draft(&self, drafts: &dyn DraftPoster, comment: &str, draft: &str) -> Write {
        Write {
            item: Item::PublishDraft {
                comment: comment.to_owned(),
                draft: draft.to_owned(),
            },
            request: Some(drafts.publish_draft(self.connection.subject(), draft)),
            sent: vec![None],
            fallback: None,
        }
    }

    /// Unposted comments on posted threads, then resolve and reopen changes.
    pub(super) fn replies(&self, target: &Target) -> (Vec<Write>, Vec<Skipped>) {
        let mut writes = Vec::new();
        let mut skipped = Vec::new();
        for thread in &target.threads {
            let Some(remote) = &thread.remote else {
                continue;
            };
            if thread.deleted_remote {
                continue;
            }
            let held = if imported(thread) {
                self.unnamed(thread)
            } else {
                self.held(thread)
            };
            let before_pr = || match self.run {
                Some(run) => self.run_before_pr(target, run),
                None => self.pre_pr(target, thread),
            };
            if let Some(reason) = held.or_else(before_pr) {
                skipped.push(Skipped::new(&thread.id, reason));
                continue;
            }
            for comment in thread.comments.iter().skip(1) {
                if self.run.is_some_and(|run| !comment_in_run(comment, run)) {
                    continue;
                }
                if comment.local
                    || comment.remote.is_some()
                    || comment.delivery.post == PostState::Posted
                {
                    continue;
                }
                if !allowed(&comment.author, &self.connection.login) {
                    skipped.push(Skipped::new(
                        &comment.id,
                        Skip::NotAllowed {
                            author: author_label(&comment.author),
                            login: self.connection.login.clone(),
                        },
                    ));
                    continue;
                }
                match self.reply(target, thread, remote, comment) {
                    Ok(write) => writes.push(write),
                    Err(reason) => skipped.push(Skipped::new(&comment.id, reason)),
                }
            }
        }
        for thread in &target.threads {
            if !self.selected_thread(thread) {
                continue;
            }
            let (Some(remote), Some(resolved)) = (&thread.remote, thread.resolve_pending) else {
                continue;
            };
            if self.run.is_some()
                || thread.deleted_remote
                || self.pre_pr(target, thread).is_some()
                || self.unnamed(thread).is_some()
                || (self.foreign_threads().is_some() && !imported(thread))
            {
                continue;
            }
            let caps = self.caps();
            let unsupported = if !caps.resolve {
                Some(Unsupported::Resolve)
            } else if !resolved && !caps.unresolve {
                Some(Unsupported::Unresolve)
            } else {
                None
            };
            if let Some(what) = unsupported.filter(|_| remote.resolvable) {
                let service = self.service().name();
                skipped.push(Skipped::new(
                    &thread.id,
                    Skip::Unsupported { service, what },
                ));
            }
            writes.push(self.resolve(&thread.id, remote, resolved));
        }
        (writes, skipped)
    }

    fn reply(
        &self,
        target: &Target,
        thread: &Thread,
        remote: &RemoteThread,
        comment: &Comment,
    ) -> std::result::Result<Write, Skip> {
        let text = body(target, thread, comment, None, self.attempt);
        let item = Item::Reply {
            comment: comment.id.clone(),
        };
        let subject = self.connection.subject();
        if self.caps().replies_natively(remote.kind) {
            return Ok(self.write(item, self.service().reply(subject, remote, &text), &text));
        }
        // Such a thread has no replies, and an agent posts no general comment: you answer
        // with a new comment that quotes the one you answer.
        if matches!(comment.author, Author::Agent { .. }) {
            return Err(Skip::NoReply);
        }
        let text = format!("{}{text}", quote(thread));
        Ok(self.write(item, self.service().note(subject, &text), &text))
    }

    /// A thread the host cannot resolve, or cannot resolve in that direction, is only cleared
    /// locally.
    fn resolve(&self, thread: &str, remote: &RemoteThread, resolved: bool) -> Write {
        let caps = self.caps();
        let local = !remote.resolvable || !caps.resolve || (!resolved && !caps.unresolve);
        Write {
            item: Item::Resolve {
                thread: thread.to_owned(),
                resolved,
            },
            request: (!local).then(|| {
                self.service()
                    .resolve(self.connection.subject(), remote, resolved)
            }),
            sent: vec![None],
            fallback: None,
        }
    }

    /// The summary comment, and its parts when it is long, when due: a run ended since it was
    /// posted, or its text changed. It is posted once, then edited in place. It always covers
    /// the whole PR/MR, whatever `--run` selects to post.
    pub(super) fn records(&self, target: &Target) -> Result<(Vec<Write>, Vec<Skipped>)> {
        // With `--run`, the summary adds no run that was never published here: only the
        // selected one, which names this PR/MR, and those an earlier summary already covers.
        let shown = |record: &Option<RecordPost>, publication: &Option<String>| {
            self.run.is_none()
                || publication.as_deref() == Some(self.review.url.as_str())
                || record
                    .as_ref()
                    .is_some_and(|record| record.delivery.post == PostState::Posted)
        };
        let runs: Vec<&Run> = target
            .runs
            .iter()
            .filter(|run| run.ended_at.is_some() && !run.passes.is_empty())
            .filter(|run| shown(&run.record, &run.publication_target))
            .filter(|run| self.run_before_pr(target, &run.id).is_none())
            .collect();
        let qa_runs: Vec<&QaRun> = target
            .qa_runs
            .iter()
            .filter(|run| run.ended_at.is_some() && !run.checks.is_empty())
            .filter(|run| shown(&run.record, &run.publication_target))
            .filter(|run| self.run_before_pr(target, &run.id).is_none())
            .collect();
        let records = || {
            runs.iter()
                .map(|run| &run.record)
                .chain(qa_runs.iter().map(|run| &run.record))
                .filter_map(Option::as_ref)
        };
        let unposted = runs
            .iter()
            .map(|run| &run.record)
            .chain(qa_runs.iter().map(|run| &run.record))
            .any(|record| {
                record
                    .as_ref()
                    .is_none_or(|record| record.delivery.post != PostState::Posted)
            });
        let reason = self
            .foreign()
            .map(|whose| Skip::Foreign {
                whose,
                held: Held::Records,
            })
            .or_else(|| (!self.only.is_empty()).then_some(Skip::OnlyThreads));
        if let Some(reason) = reason {
            let held = unposted
                .then(|| Skipped::new(markers::SUMMARY_ID, reason))
                .into_iter()
                .collect();
            return Ok((Vec::new(), held));
        }
        if runs.is_empty() && qa_runs.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let covered = runs
            .iter()
            .map(|run| {
                let head = record_head(run);
                let at_head = run_reviews_at(self.ctx, target, &run.id, &head)?;
                Ok(Covered { run, head, at_head })
            })
            .collect::<Result<Vec<_>>>()?;
        let commits = self.pr_commits();
        let head = &self.review.head_sha;
        let head_tree = self.repo.tree_of(head).ok();
        let qa = QaSummary {
            checks: latest(&qa_runs, head_tree.as_deref()),
            runs: qa_runs.clone(),
            head: head.clone(),
        };
        // The posted summary: the comment the latest run's record landed in, when it is one.
        let posted = records()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .find(|record| {
                record.remote_id.is_some()
                    && record
                        .delivery
                        .sent
                        .as_deref()
                        .and_then(markers::agent_id)
                        .is_some_and(|id| id == markers::SUMMARY_ID)
            });
        let parts = posted.map_or(&[][..], |record| record.parts.as_slice());
        let links = Links {
            summary: posted.and_then(|record| record.url.clone()),
            parts: parts.iter().map(|part| part.url.clone()).collect(),
        };
        let texts = summary_texts(
            target,
            &covered,
            &qa,
            commits.as_deref(),
            &links,
            self.attempt,
        );
        // Unchanged text needs no edit; the markers hold the attempt, which always differs.
        let same = |text: &str, sent: Option<&str>| {
            sent.is_some_and(|sent| {
                markers::strip_agent(text).trim() == markers::strip_agent(sent).trim()
            })
        };
        let subject = self.connection.subject();
        let ids: Vec<String> = covered
            .iter()
            .map(|covered| covered.run.id.clone())
            .chain(qa_runs.iter().map(|run| run.id.clone()))
            .collect();
        let mut writes = Vec::new();
        for (index, text) in texts.iter().enumerate() {
            let part = index.checked_sub(1).and_then(|index| parts.get(index));
            let (remote, current) = match part {
                _ if index == 0 => (
                    posted.and_then(|record| record.remote_id.clone()),
                    posted.is_some()
                        && !unposted
                        && same(
                            text,
                            posted.and_then(|record| record.delivery.sent.as_deref()),
                        ),
                ),
                Some(part) => (
                    part.remote_id.clone(),
                    part.delivery.post == PostState::Posted
                        && same(text, part.delivery.sent.as_deref()),
                ),
                None => (None, false),
            };
            if current {
                continue;
            }
            let request = match &remote {
                Some(id) => self.service().edit_note(subject, id, text),
                None => self.service().note(subject, text),
            };
            let item = if index == 0 {
                let runs = covered
                    .iter()
                    .map(|covered| (covered.run.id.clone(), covered.head.clone()))
                    .chain(qa_runs.iter().map(|run| (run.id.clone(), head.clone())))
                    .collect();
                Item::Summary { runs, remote }
            } else {
                Item::SummaryPart {
                    runs: ids.clone(),
                    number: index + 1,
                    remote,
                }
            };
            writes.push(self.write(item, request, text));
        }
        Ok((writes, Vec::new()))
    }

    /// The PR/MR's commits, oldest first, when git can list them.
    fn pr_commits(&self) -> Option<Vec<CommitInfo>> {
        let head = &self.review.head_sha;
        let base = self.review.base_sha.as_deref()?;
        let base = self.repo.merge_base(base, head).ok().flatten()?;
        self.repo.commits(Some(&base), head).ok()
    }

    /// A thread on a changed file that the diff cannot place on a line, as a resolvable thread
    /// on the whole file where the host has one. Its text names the lines.
    fn file_thread(&self, target: &Target, thread: &Thread, first: &Comment) -> Option<Write> {
        let anchor = thread.anchor.as_ref()?;
        // A file the PR/MR deletes has nothing at its head to comment on, and the host
        // rejects a file the PR/MR does not change.
        if anchor.side == Side::Old || !self.known_in_pr_diff(&anchor.path) {
            return None;
        }
        let lines = anchor.line.map(|_| location(anchor));
        let text = body(target, thread, first, lines.as_deref(), self.attempt);
        let request = self.service().file_thread(
            self.connection.subject(),
            self.review,
            &self.pr_old_path(&anchor.path),
            &anchor.path,
            &text,
        )?;
        let item = Item::Thread {
            comment: first.id.clone(),
        };
        let mut write = self.write(item, request, &text);
        // A host without file threads refuses the position; the text already names the lines.
        // An agent's text does not become a general comment where the host has none.
        if !self.in_review_body(first) {
            write.fallback = Some(text);
        }
        Some(write)
    }
}

/// The quote that opens a reply to a general comment, in the form GitHub's own quote reply
/// takes: the first line of the thread's first comment, who wrote it and a link to it.
fn quote(thread: &Thread) -> String {
    let Some(first) = thread.comments.first() else {
        return String::new();
    };
    let line = first
        .body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("<!--"))
        .unwrap_or_default();
    let login = match (&first.remote, &first.author) {
        (
            Some(RemoteComment {
                login: Some(login), ..
            }),
            _,
        )
        | (_, Author::Human { login, .. }) => login.as_str(),
        _ => "",
    };
    let link = first
        .remote
        .as_ref()
        .and_then(|remote| remote.url.as_deref())
        .map_or_else(String::new, |url| format!(", [comment]({url})"));
    format!("> {line}\n> — @{login}{link}\n\n")
}

/// A review finding or a QA thread: what the summary reports.
pub(super) fn found(thread: &Thread) -> bool {
    thread.finding.is_some() || thread.check.is_some()
}

/// A thread imported from the host, started by someone else or outside this store.
pub(super) fn imported(thread: &Thread) -> bool {
    thread.finding.is_none() && thread.comments.first().is_some_and(Comment::is_imported)
}

fn comment_in_run(comment: &Comment, selected: &str) -> bool {
    matches!(&comment.author, Author::Agent { run: Some(run), .. } if run == selected)
}
