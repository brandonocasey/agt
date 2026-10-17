use serde_json::{Value, json};

use super::render::{body, location, record_body, record_head, short};
use crate::core::error::{Error, Result};
use crate::forge::anchors;
use crate::forge::markers;
use crate::forge::service::{
    ApproveCap, Capabilities, Inline, Placement, Position, Request, ReviewService,
};
use crate::git::Repo;
use crate::review::commands::Ctx;
use crate::review::commands::approve::blocking_threads;
use crate::review::commands::status::run_reviews_at;
use crate::review::commands::sync::Connection;
use crate::review::commands::view::author_label;
use crate::store::model::{
    Anchor, Approval, Author, Comment, Pass, PassKind, PostState, RemoteKind, RemoteThread,
    Resolution, Review, Run, Side, Target, Thread,
};

/// Stands for the id of the draft a dry run has not created yet.
pub(super) const NEW_DRAFT: &str = "<draft id>";

/// What one write call posts, which says how to mark and save it.
pub(super) enum Item {
    /// A review holding the in-diff comments of one reviewed commit.
    Review {
        comments: Vec<String>,
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
    Record {
        run: String,
        head: String,
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
            Item::Record { .. } => "record",
            Item::Approve { .. } => "approve",
        }
    }

    pub(super) fn ids(&self) -> Vec<String> {
        match &self.item {
            Item::Review { comments } => comments.clone(),
            Item::Thread { comment }
            | Item::ApprovalReview { comment }
            | Item::Draft { comment }
            | Item::PublishDraft { comment, .. }
            | Item::Reply { comment } => vec![comment.clone()],
            Item::Resolve { thread, .. } | Item::Approve { thread } => vec![thread.clone()],
            Item::Record { run, .. } => vec![markers::record_id(run)],
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
        if let Item::Draft { comment } = &self.item {
            let publish = service.publish_draft(connection.subject(), NEW_DRAFT);
            let write = Write {
                item: Item::PublishDraft {
                    comment: comment.clone(),
                    draft: NEW_DRAFT.to_owned(),
                },
                request: Some(publish),
                sent: vec![None],
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

/// Where a comment goes on the host.
pub(super) enum Spot {
    InDiff(Position),
    /// No anchor: a general comment.
    General,
    /// A file the PR/MR does not change: a general comment naming the location.
    OtherFile(String),
    /// Lines of a changed file that are not placeable in the PR/MR diff; never posted.
    Outside,
}

/// The posting plan for one target state; built the same way for a dry run and a real one.
pub(super) struct Planner<'a> {
    ctx: &'a Ctx,
    attempt: &'a str,
    /// `--findings`: on someone else's PR/MR, also post our findings and local threads.
    findings: bool,
    /// `--only`: the thread ids to post; empty means all.
    only: &'a [String],
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

/// Why something is not posted.
#[derive(Clone)]
pub(super) enum Skip {
    NotNamed,
    /// `--only` names threads, so final records are held.
    OnlyThreads,
    /// The PR/MR is someone else's (`--not-own`).
    Foreign {
        whose: String,
        held: Held,
    },
    PassOpen {
        run: String,
        pass: String,
    },
    PassLocked {
        run: String,
        pass: String,
        cycle: u32,
    },
    NotAllowed {
        author: String,
        login: String,
    },
    BeforeLink {
        run: String,
        url: String,
    },
    OutsideDiff,
    HeadMoved {
        approved: String,
        head: String,
    },
    OpenFindings(Vec<String>),
    /// The service lacks a capability the item needs.
    Unsupported {
        service: &'static str,
        what: Unsupported,
    },
}

#[derive(Clone)]
pub(super) enum Held {
    Threads,
    Records,
}

#[derive(Clone)]
pub(super) enum Unsupported {
    SelfApproval { posted_as_comment: bool },
    FormalApproval,
    Resolve,
    Unresolve,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotNamed => write!(f, "not named by --only"),
            Self::OnlyThreads => write!(f, "--only posts named threads, not final records"),
            Self::Foreign {
                whose,
                held: Held::Threads,
            } => write!(
                f,
                "{whose}: --not-own posts only replies and resolves on others' threads (add \
                 --findings for ours)"
            ),
            Self::Foreign {
                whose,
                held: Held::Records,
            } => write!(f, "{whose}: final records are never posted there"),
            Self::PassOpen { run, pass } => write!(f, "pass {run}.{pass} has not ended"),
            Self::PassLocked { run, pass, cycle } => write!(
                f,
                "pass {run}.{pass} is locked until a reveal pass in cycle {cycle} or the run ends"
            ),
            Self::NotAllowed { author, login } => write!(f, "written by {author}, not {login}"),
            Self::BeforeLink { run, url } => {
                write!(f, "run {run} ended before the target was linked to {url}")
            }
            Self::OutsideDiff => write!(
                f,
                "its lines are not in the PR/MR diff of the reviewed range"
            ),
            Self::HeadMoved { approved, head } => {
                write!(
                    f,
                    "it approves {approved}, but the PR/MR head is now {head}"
                )
            }
            Self::OpenFindings(ids) => {
                write!(f, "open findings block approval: {}", ids.join(", "))
            }
            Self::Unsupported { service, what } => match what {
                Unsupported::SelfApproval {
                    posted_as_comment: false,
                } => write!(f, "{service} does not let the author approve their own PR"),
                Unsupported::SelfApproval {
                    posted_as_comment: true,
                } => write!(
                    f,
                    "{service} does not let the author approve their own PR; posted as a comment"
                ),
                Unsupported::FormalApproval => {
                    write!(f, "{service} has no formal approval; posted as a comment")
                }
                Unsupported::Resolve => {
                    write!(f, "{service} cannot resolve threads; cleared locally")
                }
                Unsupported::Unresolve => {
                    write!(f, "{service} cannot reopen threads; cleared locally")
                }
            },
        }
    }
}

pub(super) struct Skipped {
    pub(super) item: String,
    pub(super) reason: Skip,
}

impl Skipped {
    pub(super) fn new(item: &str, reason: Skip) -> Self {
        Self {
            item: item.to_owned(),
            reason,
        }
    }

    pub(super) fn view(&self) -> Value {
        json!({ "item": self.item, "reason": self.reason.to_string() })
    }
}

impl<'a> Planner<'a> {
    pub(super) fn new(
        ctx: &'a Ctx,
        connection: &'a Connection,
        target: &'a Target,
        attempt: &'a str,
        findings: bool,
        only: &'a [String],
    ) -> Result<Self> {
        let review = target
            .review
            .as_ref()
            .ok_or_else(|| Error(format!("target {} has no synced PR/MR", target.key)))?;
        Ok(Self {
            ctx,
            attempt,
            findings,
            only,
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
        for thread in &target.threads {
            let Some(first) = thread.comments.first() else {
                continue;
            };
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
            // A finding judged stale or rejected before it was ever posted has nothing to tell
            // the host.
            if thread.finding.is_some()
                && thread
                    .resolution
                    .as_ref()
                    .is_some_and(Resolution::withdrawn)
            {
                continue;
            }
            if let Some(reason) = self.blocked(target, thread, first) {
                skipped.push(Skipped::new(&thread.id, reason));
                continue;
            }
            if let Some(draft) = &first.draft_id {
                writes.push(self.publish_draft(&first.id, draft));
                expected.push((thread.id.clone(), RemoteKind::Discussion));
                continue;
            }
            if let Some(approval) = &thread.approval {
                match self.approval(thread, first, approval, target) {
                    Ok((mut approve, kind, note)) => {
                        writes.append(&mut approve);
                        expected.push((thread.id.clone(), kind));
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
            match spot {
                Spot::InDiff(position) => {
                    let text = body(target, thread, first, None, self.attempt);
                    match self.caps().inline {
                        Inline::ReviewBatch => {
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
                        Inline::DraftThenPublish => {
                            let request =
                                self.service()
                                    .draft(self.connection.subject(), &text, &position);
                            writes.push(self.write(
                                Item::Draft {
                                    comment: first.id.clone(),
                                },
                                request,
                                &text,
                            ));
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
        let reviews = reviews.into_iter().map(
            |Batch {
                 commit,
                 ids,
                 comments,
             }| {
                let summary = format!(
                    "Review findings.\n{}",
                    markers::agent(&markers::review_id(&commit), None)
                );
                let request = self.service().review_batch(
                    self.connection.subject(),
                    &commit,
                    &summary,
                    &comments,
                );
                Write {
                    item: Item::Review { comments: ids },
                    request: Some(request),
                    sent: comments.into_iter().map(|(_, text)| Some(text)).collect(),
                }
            },
        );
        let writes = reviews.chain(writes).collect();
        (writes, skipped, expected)
    }

    /// An approval's writes: a comment, plus the host's approval when formal, and a note on
    /// a formal approval the host does not allow. Only for the head it approved, and only
    /// while no finding blocks approval.
    fn approval(
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
    fn pending_approve(
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
        }))
    }

    /// Whether an approval may go out now: at the head it approved, with no blocking finding.
    fn approvable(&self, approval: &Approval, target: &Target) -> std::result::Result<(), Skip> {
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

    /// Why a new thread is not posted: a finding of a pass still in progress, or another
    /// person's text.
    fn blocked(&self, target: &Target, thread: &Thread, first: &Comment) -> Option<Skip> {
        if let Some(reason) = self.held(thread) {
            return Some(reason);
        }
        if let Some(reason) = self.pre_pr(target, thread) {
            return Some(reason);
        }
        if let Some(finding) = &thread.finding {
            let passes: Vec<&Pass> = target
                .runs
                .iter()
                .filter(|run| run.id == finding.run)
                .flat_map(|run| &run.passes)
                .collect();
            let pass = passes.iter().find(|pass| pass.id == finding.pass);
            let Some(pass) = pass.filter(|pass| pass.ended_at.is_some()) else {
                return Some(Skip::PassOpen {
                    run: finding.run.clone(),
                    pass: finding.pass.clone(),
                });
            };
            // A blind pass's findings stay hidden from the host until its reveal ends, or its
            // run ends when the harness cannot continue reviewers for a reveal.
            let run_ended = target
                .runs
                .iter()
                .any(|run| run.id == finding.run && run.ended_at.is_some());
            let revealed = run_ended
                || passes.iter().any(|later| {
                    later.kind == PassKind::Reveal
                        && later.cycle == pass.cycle
                        && later.number > pass.number
                        && later.ended_at.is_some()
                });
            if pass.locked && !revealed {
                return Some(Skip::PassLocked {
                    run: finding.run.clone(),
                    pass: finding.pass.clone(),
                    cycle: pass.cycle,
                });
            }
        }
        if !allowed(&first.author, &self.connection.login) {
            return Some(Skip::NotAllowed {
                author: author_label(&first.author),
                login: self.connection.login.clone(),
            });
        }
        None
    }

    /// Why a finding is not the PR/MR's business: its run ended before the target was linked
    /// to it, so it reviewed history from before the PR/MR.
    fn pre_pr(&self, target: &Target, thread: &Thread) -> Option<Skip> {
        let run = &thread.finding.as_ref()?.run;
        self.run_before_pr(target, run)
    }

    fn run_before_pr(&self, target: &Target, run_id: &str) -> Option<Skip> {
        let attached = self.review.attached_at.as_deref()?;
        let ended = target
            .runs
            .iter()
            .find(|run| run.id == run_id)?
            .ended_at
            .as_deref()?;
        // Timestamps share one fixed-width UTC format, so they order as text.
        (ended < attached).then(|| Skip::BeforeLink {
            run: run_id.to_owned(),
            url: self.review.url.clone(),
        })
    }

    fn publish_draft(&self, comment: &str, draft: &str) -> Write {
        Write {
            item: Item::PublishDraft {
                comment: comment.to_owned(),
                draft: draft.to_owned(),
            },
            request: Some(
                self.service()
                    .publish_draft(self.connection.subject(), draft),
            ),
            sent: vec![None],
        }
    }

    /// The place of an anchored comment in the PR/MR diff. A file the PR/MR does not change,
    /// or a finding on a whole file, gets a general comment; lines of a changed file that fit
    /// no hunk are outside.
    fn spot(&self, anchor: &Anchor) -> Spot {
        let wanted = match anchor.side {
            Side::New => anchor.line,
            Side::Old => anchor.old_line,
        };
        let single;
        let anchor = if self.caps().ranges {
            anchor
        } else {
            single = Anchor {
                start_line: None,
                ..anchor.clone()
            };
            &single
        };
        let placed = match (&anchor.commit, wanted) {
            (Some(commit), Some(line)) => {
                self.service()
                    .place(self.repo, self.review, anchor, commit, line)
            }
            _ => None,
        };
        match placed {
            Some(Placement::InDiff(position)) => Spot::InDiff(position),
            Some(Placement::OtherFile) => Spot::OtherFile(location(anchor)),
            None if wanted.is_none() || !self.in_pr_diff(&anchor.path) => {
                Spot::OtherFile(location(anchor))
            }
            None => Spot::Outside,
        }
    }

    /// Whether the PR/MR diff changes this file; unknown counts as changed, so nothing
    /// unplaceable is posted.
    fn in_pr_diff(&self, path: &str) -> bool {
        let files = self.pr_files.get_or_init(|| {
            let head = &self.review.head_sha;
            let diff = self
                .review
                .base_sha
                .as_deref()
                .and_then(|base| self.repo.merge_base(base, head).ok().flatten())
                .and_then(|base| self.repo.diff_u0(&base, head).ok());
            diff.map(|diff| anchors::parse(&diff)).unwrap_or_default()
        });
        files.is_empty() || files.iter().any(|file| file.touches(path))
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
            if let Some(reason) = held.or_else(|| self.pre_pr(target, thread)) {
                skipped.push(Skipped::new(&thread.id, reason));
                continue;
            }
            for comment in thread.comments.iter().skip(1) {
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
                writes.push(self.reply(target, thread, remote, comment));
            }
        }
        for thread in &target.threads {
            let (Some(remote), Some(resolved)) = (&thread.remote, thread.resolve_pending) else {
                continue;
            };
            if thread.deleted_remote
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
    ) -> Write {
        let text = body(target, thread, comment, None, self.attempt);
        let item = Item::Reply {
            comment: comment.id.clone(),
        };
        let subject = self.connection.subject();
        if self.caps().replies_natively(remote.kind) {
            return self.write(item, self.service().reply(subject, remote, &text), &text);
        }
        // Such a thread has no replies: answer with a new comment naming it.
        let text = format!("{text}\nRe: {}", thread.id);
        self.write(item, self.service().note(subject, &text), &text)
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
        }
    }

    /// The final record of each ended run not posted yet, and the records held back.
    pub(super) fn records(&self, target: &Target) -> Result<(Vec<Write>, Vec<Skipped>)> {
        let due: Vec<&Run> = target
            .runs
            .iter()
            .filter(|run| run.ended_at.is_some() && !run.passes.is_empty())
            .filter(|run| self.run_before_pr(target, &run.id).is_none())
            .filter(|run| {
                run.record
                    .as_ref()
                    .is_none_or(|record| record.delivery.post != PostState::Posted)
            })
            .collect();
        let reason = self
            .foreign()
            .map(|whose| Skip::Foreign {
                whose,
                held: Held::Records,
            })
            .or_else(|| (!self.only.is_empty()).then_some(Skip::OnlyThreads));
        if let Some(reason) = reason {
            let held = due
                .iter()
                .map(|run| Skipped::new(&markers::record_id(&run.id), reason.clone()))
                .collect();
            return Ok((Vec::new(), held));
        }
        let writes = due
            .into_iter()
            .map(|run| {
                let head = record_head(run);
                let at_head = run_reviews_at(self.ctx, target, &run.id, &head)?;
                let text = record_body(target, run, &head, at_head, self.attempt);
                let request = self.service().note(self.connection.subject(), &text);
                Ok(self.write(
                    Item::Record {
                        run: run.id.clone(),
                        head,
                    },
                    request,
                    &text,
                ))
            })
            .collect::<Result<_>>()?;
        Ok((writes, Vec::new()))
    }

    /// Why a thread of ours is held: not named by `--only`, or on someone else's PR/MR
    /// without `--findings`. An approval the user named with `--only` may go there.
    fn held(&self, thread: &Thread) -> Option<Skip> {
        let named_approval = thread.approval.is_some() && self.only.contains(&thread.id);
        self.unnamed(thread)
            .or_else(|| self.foreign_threads().filter(|_| !named_approval))
    }

    fn unnamed(&self, thread: &Thread) -> Option<Skip> {
        (!self.only.is_empty() && !self.only.contains(&thread.id)).then_some(Skip::NotNamed)
    }

    /// Why our findings and local threads are held on someone else's PR/MR, unless
    /// `--findings` asks for them.
    fn foreign_threads(&self) -> Option<Skip> {
        self.foreign()
            .filter(|_| !self.findings)
            .map(|whose| Skip::Foreign {
                whose,
                held: Held::Threads,
            })
    }

    /// Whose PR/MR this is when it is not ours (`--not-own`): a final record there would
    /// speak for its author, and our findings and threads go out only with `--findings`.
    fn foreign(&self) -> Option<String> {
        let author = self.review.author.as_deref();
        (author != Some(self.connection.login.as_str())).then(|| {
            format!(
                "{} is authored by {}",
                self.review.url,
                author.unwrap_or("an unknown user")
            )
        })
    }
}

/// A thread imported from the host, started by someone else or outside this store.
pub(super) fn imported(thread: &Thread) -> bool {
    thread.finding.is_none() && thread.comments.first().is_some_and(Comment::is_imported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_reasons_read_as_they_are_posted() {
        let foreign = |held| Skip::Foreign {
            whose: "https://h/pull/1 is authored by bob".into(),
            held,
        };
        assert_eq!(
            foreign(Held::Records).to_string(),
            "https://h/pull/1 is authored by bob: final records are never posted there"
        );
        assert_eq!(
            foreign(Held::Threads).to_string(),
            "https://h/pull/1 is authored by bob: --not-own posts only replies and resolves on \
             others' threads (add --findings for ours)"
        );
        let head = Skip::HeadMoved {
            approved: "aaaaaaa".into(),
            head: "bbbbbbb".into(),
        };
        assert_eq!(
            head.to_string(),
            "it approves aaaaaaa, but the PR/MR head is now bbbbbbb"
        );
        let open = Skip::OpenFindings(vec!["f1".into(), "f2".into()]);
        assert_eq!(open.to_string(), "open findings block approval: f1, f2");
    }

    #[test]
    fn unsupported_capabilities_name_the_service() {
        let unsupported = |what| Skip::Unsupported {
            service: "Forgejo",
            what,
        };
        assert_eq!(
            unsupported(Unsupported::Resolve).to_string(),
            "Forgejo cannot resolve threads; cleared locally"
        );
        assert_eq!(
            unsupported(Unsupported::SelfApproval {
                posted_as_comment: true
            })
            .to_string(),
            "Forgejo does not let the author approve their own PR; posted as a comment"
        );
    }
}
