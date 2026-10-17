use std::fmt::Write as _;

use serde_json::{Value, json};

use super::sync::{self, Connection};
use super::{Ctx, author_label, blocking_threads, is_open, random_id, run_reviews_at};
use crate::anchors;
use crate::clock;
use crate::error::{Error, Result, fail};
use crate::git::Repo;
use crate::markers::{self, agent_id};
use crate::model::{
    Anchor, Approval, Author, Comment, Pass, PassKind, PostState, RecordPost, RemoteComment,
    RemoteKind, RemoteThread, Resolution, Review, Run, Side, Target, Thread,
};
use crate::service::{ApproveCap, Capabilities, Inline, Placement, Position, Request, Service};

/// Stands for the id of the draft a dry run has not created yet.
const NEW_DRAFT: &str = "<draft id>";

/// What one write call posts, which says how to mark and save it.
enum Item {
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

struct Write {
    item: Item,
    /// `None` for a change that is only recorded locally.
    request: Option<Request>,
    /// The text each item of the write sends, in the order of its ids.
    sent: Vec<Option<String>>,
}

impl Write {
    fn kind(&self) -> &'static str {
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

    fn ids(&self) -> Vec<String> {
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
    fn views(&self, connection: &Connection) -> Vec<Value> {
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
struct Batch {
    commit: String,
    ids: Vec<String>,
    comments: Vec<(Position, String)>,
}

/// Where a comment goes on the host.
enum Spot {
    InDiff(Position),
    /// No anchor: a general comment.
    General,
    /// A file the PR/MR does not change: a general comment naming the location.
    OtherFile(String),
    /// Lines of a changed file that are not placeable in the PR/MR diff; never posted.
    Outside,
}

/// The posting plan for one target state; built the same way for a dry run and a real one.
struct Planner<'a> {
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

/// `publish` flags.
pub(super) struct Options {
    pub dry_run: bool,
    pub not_own: bool,
    pub findings: bool,
    pub only: Vec<String>,
}

pub(super) fn publish(ctx: &Ctx, pr: Option<&str>, options: &Options) -> Result<Value> {
    let Options {
        dry_run, not_own, ..
    } = *options;
    if options.findings && options.only.is_empty() {
        return fail("--findings posts on someone else's PR/MR only what --only <id,...> names");
    }
    let connection = sync::connect(ctx, pr)?;
    let _publish_lock = if dry_run {
        None
    } else {
        Some(ctx.store.publish_lock(&ctx.dir, &connection.key)?)
    };
    let plan = sync::plan(ctx, &connection)?;
    let author = plan.review.author.clone();
    if !not_own && author.as_deref() != Some(connection.login.as_str()) {
        return fail(format!(
            "{} is authored by {}, not {}; pass --not-own to post anyway",
            plan.review.url,
            author.as_deref().unwrap_or("an unknown user"),
            connection.login
        ));
    }
    if dry_run {
        let target = plan.preview(ctx, &connection);
        return dry(ctx, &connection, target, options);
    }
    sync::sync_planned(ctx, &connection, plan)?;
    // One attempt id per publish: sync claims a marked note only for a write of that attempt.
    let attempt = random_id().replacen("rv-", "at-", 1);
    let mut posted = Vec::new();
    let mut skipped = Vec::new();
    let target = load(ctx, &connection)?;
    let (writes, skips, _) =
        planner(ctx, &connection, &target, &attempt, options)?.threads(&target);
    skipped.extend(skips);
    run_writes(ctx, &connection, &target, writes, &attempt, &mut posted)?;
    sync::sync_with(ctx, &connection)?;
    let target = load(ctx, &connection)?;
    let (writes, skips) = planner(ctx, &connection, &target, &attempt, options)?.replies(&target);
    skipped.extend(skips);
    run_writes(ctx, &connection, &target, writes, &attempt, &mut posted)?;
    let target = load(ctx, &connection)?;
    let (writes, skips) =
        planner(ctx, &connection, &target, &attempt, options)?.records(&target)?;
    skipped.extend(skips);
    run_writes(ctx, &connection, &target, writes, &attempt, &mut posted)?;
    let synced = sync::sync_with(ctx, &connection)?;
    Ok(json!({
        "target": connection.key,
        "dry_run": false,
        "posted": posted,
        "skipped": skipped,
        "sync": synced,
    }))
}

fn load(ctx: &Ctx, connection: &Connection) -> Result<Target> {
    ctx.store
        .load(&ctx.dir, &connection.key)?
        .ok_or_else(|| Error(format!("target {} vanished during publish", connection.key)))
}

fn planner<'a>(
    ctx: &'a Ctx,
    connection: &'a Connection,
    target: &'a Target,
    attempt: &'a str,
    options: &'a Options,
) -> Result<Planner<'a>> {
    let review = target
        .review
        .as_ref()
        .ok_or_else(|| Error(format!("target {} has no synced PR/MR", target.key)))?;
    Ok(Planner {
        ctx,
        attempt,
        findings: options.findings,
        only: &options.only,
        repo: &ctx.repo,
        connection,
        review,
        pr_files: std::cell::OnceCell::new(),
    })
}

/// Every payload a publish would send, from an in-memory sync; nothing is written.
fn dry(ctx: &Ctx, connection: &Connection, mut target: Target, options: &Options) -> Result<Value> {
    // A dry run sends nothing, so its markers name no real attempt.
    let attempt = "<attempt>";
    let (threads, skipped, expected) =
        planner(ctx, connection, &target, attempt, options)?.threads(&target);
    // Treat the threads above as posted so their replies and resolves show too.
    for (id, kind) in expected {
        if let Some(thread) = target.threads.iter_mut().find(|thread| thread.id == id) {
            thread.remote = Some(placeholder(&id, kind, connection.service.caps()));
            if let Some(first) = thread.comments.first_mut() {
                first.post = PostState::Posted;
            }
        }
    }
    let planner = planner(ctx, connection, &target, attempt, options)?;
    let (replies, more) = planner.replies(&target);
    let (records, held) = planner.records(&target)?;
    let writes: Vec<Value> = threads
        .iter()
        .chain(&replies)
        .chain(&records)
        .flat_map(|write| write.views(connection))
        .collect();
    let skipped: Vec<Value> = skipped.into_iter().chain(more).chain(held).collect();
    Ok(json!({
        "target": connection.key,
        "dry_run": true,
        "writes": writes,
        "skipped": skipped,
    }))
}

fn placeholder(id: &str, kind: RemoteKind, caps: &Capabilities) -> RemoteThread {
    RemoteThread::new(
        kind,
        format!("<remote id of {id}>"),
        kind != RemoteKind::General || caps.general_resolvable,
    )
}

/// Only agents and the authenticated user speak through `publish`.
fn allowed(author: &Author, login: &str) -> bool {
    match author {
        Author::Agent { .. } => true,
        Author::Human { login: name, .. } => name == login,
    }
}

fn skip(id: &str, reason: String) -> Value {
    json!({ "item": id, "reason": reason })
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

impl Planner<'_> {
    fn service(&self) -> &dyn Service {
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
    fn threads(&self, target: &Target) -> (Vec<Write>, Vec<Value>, Vec<(String, RemoteKind)>) {
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
                || first.post == PostState::Posted
            {
                // An approval posted as a comment, then made formal or refused and retried.
                match self.pending_approve(thread, target) {
                    Some(Ok(write)) => writes.push(write),
                    Some(Err(reason)) => {
                        skipped.push(skip(&format!("{}/formal", thread.id), reason))
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
                skipped.push(skip(&thread.id, reason));
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
                            skipped.push(skip(&format!("{}/formal", thread.id), note));
                        }
                    }
                    Err(reason) => skipped.push(skip(&thread.id, reason)),
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
                    skipped.push(skip(
                        &thread.id,
                        "its lines are not in the PR/MR diff of the reviewed range".to_owned(),
                    ));
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
    ) -> std::result::Result<(Vec<Write>, RemoteKind, Option<String>), String> {
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
                let reason = approval.formal.then(|| {
                    format!(
                        "{} does not let the author approve their own PR; posted as a comment",
                        self.service().name()
                    )
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
                let reason = format!(
                    "{} has no formal approval; posted as a comment",
                    self.service().name()
                );
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
    ) -> Option<std::result::Result<Write, String>> {
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
            return Some(Err(format!(
                "{} does not let the author approve their own PR",
                self.service().name()
            )));
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
    fn approvable(&self, approval: &Approval, target: &Target) -> std::result::Result<(), String> {
        let head = &self.review.head_sha;
        if &approval.head != head {
            return Err(format!(
                "it approves {}, but the PR/MR head is now {}",
                short(&approval.head),
                short(head)
            ));
        }
        let blocking: Vec<&str> = blocking_threads(target).collect();
        if blocking.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "open findings block approval: {}",
                blocking.join(", ")
            ))
        }
    }

    /// Why a new thread is not posted: a finding of a pass still in progress, or another
    /// person's text.
    fn blocked(&self, target: &Target, thread: &Thread, first: &Comment) -> Option<String> {
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
                return Some(format!(
                    "pass {}.{} has not ended",
                    finding.run, finding.pass
                ));
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
                return Some(format!(
                    "pass {}.{} is locked until a reveal pass in cycle {} or the run ends",
                    finding.run, finding.pass, pass.cycle
                ));
            }
        }
        if !allowed(&first.author, &self.connection.login) {
            return Some(format!(
                "written by {}, not {}",
                author_label(&first.author),
                self.connection.login
            ));
        }
        None
    }

    /// Why a finding is not the PR/MR's business: its run ended before the target was linked
    /// to it, so it reviewed history from before the PR/MR.
    fn pre_pr(&self, target: &Target, thread: &Thread) -> Option<String> {
        let run = &thread.finding.as_ref()?.run;
        self.run_before_pr(target, run)
    }

    fn run_before_pr(&self, target: &Target, run_id: &str) -> Option<String> {
        let attached = self.review.attached_at.as_deref()?;
        let ended = target
            .runs
            .iter()
            .find(|run| run.id == run_id)?
            .ended_at
            .as_deref()?;
        // Timestamps share one fixed-width UTC format, so they order as text.
        (ended < attached).then(|| {
            format!(
                "run {run_id} ended before the target was linked to {}",
                self.review.url
            )
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
    fn replies(&self, target: &Target) -> (Vec<Write>, Vec<Value>) {
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
                skipped.push(skip(&thread.id, reason));
                continue;
            }
            for comment in thread.comments.iter().skip(1) {
                if comment.local || comment.remote.is_some() || comment.post == PostState::Posted {
                    continue;
                }
                if !allowed(&comment.author, &self.connection.login) {
                    skipped.push(skip(
                        &comment.id,
                        format!(
                            "written by {}, not {}",
                            author_label(&comment.author),
                            self.connection.login
                        ),
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
    fn records(&self, target: &Target) -> Result<(Vec<Write>, Vec<Value>)> {
        let due: Vec<&Run> = target
            .runs
            .iter()
            .filter(|run| run.ended_at.is_some() && !run.passes.is_empty())
            .filter(|run| self.run_before_pr(target, &run.id).is_none())
            .filter(|run| {
                run.record
                    .as_ref()
                    .is_none_or(|record| record.post != PostState::Posted)
            })
            .collect();
        let reason = self
            .foreign()
            .map(|reason| format!("{reason}: final records are never posted there"));
        let reason = reason.or_else(|| {
            (!self.only.is_empty())
                .then(|| "--only posts named threads, not final records".to_owned())
        });
        if let Some(reason) = reason {
            let held = due
                .iter()
                .map(|run| skip(&markers::record_id(&run.id), reason.clone()))
                .collect();
            return Ok((Vec::new(), held));
        }
        let writes = due
            .into_iter()
            .map(|run| {
                let (head, text) = record_body(self.ctx, target, run, self.attempt)?;
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
    fn held(&self, thread: &Thread) -> Option<String> {
        let named_approval = thread.approval.is_some() && self.only.contains(&thread.id);
        self.unnamed(thread)
            .or_else(|| self.foreign_threads().filter(|_| !named_approval))
    }

    fn unnamed(&self, thread: &Thread) -> Option<String> {
        (!self.only.is_empty() && !self.only.contains(&thread.id))
            .then(|| "not named by --only".to_owned())
    }

    /// Why our findings and local threads are held on someone else's PR/MR, unless
    /// `--findings` asks for them.
    fn foreign_threads(&self) -> Option<String> {
        self.foreign().filter(|_| !self.findings).map(|reason| {
            format!(
                "{reason}: --not-own posts only replies and resolves on others' threads \
                     (add --findings for ours)"
            )
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
fn imported(thread: &Thread) -> bool {
    thread.finding.is_none() && thread.comments.first().is_some_and(Comment::is_imported)
}

/// `2m30s`, `45s`, `14m`: exact, so the parts add up to the total.
fn duration(secs: i64) -> String {
    match (secs / 60, secs % 60) {
        (0, secs) => format!("{secs}s"),
        (minutes, 0) => format!("{minutes}m"),
        (minutes, secs) => format!("{minutes}m{secs}s"),
    }
}

/// `Time: 14m (reviewing 9m, fixing 5m). Tokens: reviewers 120k, main session 80k.`
fn spend_line(spent: &Value) -> Option<String> {
    let minutes = |key: &str| spent[key].as_i64().map(duration);
    let thousands = |key: &str| {
        spent[key]
            .as_u64()
            .map(|tokens| format!("{}k", (tokens + 500) / 1000))
    };
    let mut line = String::new();
    if let Some(total) = minutes("total_secs") {
        let _ = write!(line, "Time: {total}");
        if let (Some(review), Some(fix)) = (minutes("reviewing_secs"), minutes("fixing_secs")) {
            let _ = write!(line, " (reviewing {review}, fixing {fix})");
        }
        line.push('.');
    }
    let tokens: Vec<String> = [
        ("reviewers", "reviewer_tokens"),
        ("main session", "main_tokens"),
    ]
    .iter()
    .filter_map(|(label, key)| thousands(key).map(|value| format!("{label} {value}")))
    .collect();
    if !tokens.is_empty() {
        if !line.is_empty() {
            line.push(' ');
        }
        let _ = write!(line, "Tokens: {}.", tokens.join(", "));
    }
    (!line.is_empty()).then_some(line)
}

/// `path:line`, `path (old):line`, or the path of a whole-file finding.
fn location(anchor: &Anchor) -> String {
    match (anchor.side, anchor.line, anchor.old_line) {
        (Side::New, Some(line), _) => format!("{}:{line}", anchor.path),
        (Side::Old, _, Some(line)) => format!("{} (old):{line}", anchor.path),
        _ => anchor.path.clone(),
    }
}

/// The posted text: a finding heading, the text, the signature and the markers.
fn body(
    target: &Target,
    thread: &Thread,
    comment: &Comment,
    location: Option<&str>,
    attempt: &str,
) -> String {
    let first = thread
        .comments
        .first()
        .is_some_and(|first| first.id == comment.id);
    let finding = thread.finding.as_ref().filter(|_| first);
    let mut text = String::new();
    if let Some(location) = location {
        let _ = write!(text, "`{location}`\n\n");
    }
    if finding.is_some() {
        let kind = thread.kind.as_str();
        match thread.severity {
            Some(severity) => {
                let _ = write!(text, "**{kind} ({}):** ", severity.as_str());
            }
            None => {
                let _ = write!(text, "**{kind}:** ");
            }
        }
    }
    text.push_str(comment.body.trim_end());
    text.push_str("\n\n");
    text.push_str(&signature(&comment.author));
    if let Some(finding) = finding {
        if finding.post_reveal {
            text.push_str(" post-reveal");
        }
        let sha = thread
            .anchor
            .as_ref()
            .and_then(|anchor| anchor.commit.clone())
            .or_else(|| {
                target
                    .runs
                    .iter()
                    .filter(|run| run.id == finding.run)
                    .flat_map(|run| &run.passes)
                    .find(|pass| pass.id == finding.pass)
                    .map(|pass| pass.head.clone())
            })
            .unwrap_or_default();
        let _ = write!(text, "\n{}", markers::finding(&thread.id, &sha));
    }
    let _ = write!(text, "\n{}", markers::agent(&comment.id, Some(attempt)));
    text
}

fn signature(author: &Author) -> String {
    match author {
        Author::Agent { effort, .. } if effort.trim().is_empty() || effort == "unknown" => {
            let mut author = author.clone();
            if let Author::Agent { effort, .. } = &mut author {
                "default".clone_into(effort);
            }
            author_label(&author)
        }
        _ => author_label(author),
    }
}

fn harness_name(harness: &str) -> &str {
    match harness {
        "claude-code" => "Claude Code",
        "codex" | "codex-cli" => "codex-cli",
        other => other,
    }
}

fn tool(model: &str, effort: &str, harness: &str, version: Option<&str>) -> String {
    let mut text = format!("{model} {effort} · {}", harness_name(harness));
    if let Some(version) = version {
        let _ = write!(text, " {version}");
    }
    text
}

/// The run's final record per the review skill, and the head it records.
fn record_body(ctx: &Ctx, target: &Target, run: &Run, attempt: &str) -> Result<(String, String)> {
    let last = run
        .passes
        .last()
        .map(|pass| pass.head.clone())
        .unwrap_or_default();
    let head = run.repaired_head.clone().unwrap_or(last);
    let findings: Vec<&Thread> = target
        .threads
        .iter()
        .filter(|thread| {
            thread
                .finding
                .as_ref()
                .is_some_and(|finding| finding.run == run.id)
        })
        .collect();
    let fixed = findings
        .iter()
        .filter(|thread| {
            thread.resolved && matches!(thread.resolution, Some(Resolution::Fixed { .. }))
        })
        .count();
    let rejected = findings
        .iter()
        .filter(|thread| {
            thread.resolved && matches!(thread.resolution, Some(Resolution::Rejected { .. }))
        })
        .count();
    let rejected = match rejected {
        0 => String::new(),
        count => format!(", {count} rejected"),
    };
    let open = findings.iter().filter(|thread| is_open(thread)).count();
    let mut cycles: Vec<u32> = run.passes.iter().map(|pass| pass.cycle).collect();
    cycles.dedup();
    let at_head = run_reviews_at(ctx, target, &run.id, &head)?;
    // Fixes after the last pass make the head "review-fixed"; a repair a pass still covers,
    // such as committing the reviewed dirty tree, leaves it "reviewed".
    let verdict = if run.repair.is_empty() || at_head > 0 {
        "reviewed"
    } else {
        "review-fixed"
    };
    let mut text = format!(
        "**Review {}:** {verdict} at {}. {} cycles, {} found, {fixed} fixed{rejected}, {open} open.\n\n",
        run.id,
        short(&head),
        cycles.len(),
        findings.len(),
    );
    text.push_str("| Pass | Reviewed | Starting context | Reviewer |\n|---|---|---|---|\n");
    let mut seen: Vec<&str> = Vec::new();
    for pass in &run.passes {
        let kind = match pass.kind {
            PassKind::Full => "full",
            PassKind::FixCheck => "fix check",
            PassKind::Reveal => "reveal",
        };
        let reviewed = match (&pass.base, pass.kind) {
            (Some(base), PassKind::FixCheck) => format!("{}..{}", short(base), short(&pass.head)),
            _ => short(&pass.head).to_owned(),
        };
        // The fixer reviews nothing; it shows on the `Fixed by:` line.
        for reviewer in pass
            .reviewers
            .iter()
            .filter(|reviewer| reviewer.id != "fix")
        {
            let label = if seen.contains(&reviewer.id.as_str()) {
                reviewer.id.clone()
            } else {
                seen.push(&reviewer.id);
                format!(
                    "{} {}",
                    reviewer.id,
                    tool(
                        &reviewer.model,
                        &reviewer.effort,
                        &reviewer.harness,
                        reviewer.harness_version.as_deref()
                    )
                )
            };
            let _ = writeln!(
                text,
                "| {} {kind} | {reviewed} | {} | {label} |",
                pass.id, reviewer.starting_context
            );
        }
    }
    text.push('\n');
    if let Some(reveal) = run.reveal {
        let _ = writeln!(
            text,
            "Reveal: {} agree, {} disagree.",
            reveal.agree, reveal.disagree
        );
    }
    let mut fixers: Vec<String> = Vec::new();
    for comment in findings.iter().flat_map(|thread| &thread.comments) {
        if let Author::Agent {
            model,
            effort,
            harness,
            harness_version,
            reviewer: Some(reviewer),
            ..
        } = &comment.author
            && reviewer == "fix"
        {
            let fixer = tool(model, effort, harness, harness_version.as_deref());
            if !fixers.contains(&fixer) {
                fixers.push(fixer);
            }
        }
    }
    if !fixers.is_empty() {
        let _ = writeln!(text, "Fixed by: {}", fixers.join("; "));
    }
    if let Some(line) = spend_line(&super::run_spend(run)) {
        let _ = writeln!(text, "{line}");
    }
    let _ = writeln!(text, "{}", markers::record(&run.id, &head, at_head, open));
    let _ = write!(
        text,
        "{}",
        markers::agent(&markers::record_id(&run.id), Some(attempt))
    );
    Ok((head, text))
}

/// Runs writes in order. Each item is marked `posting` and saved before its call and gets its
/// remote id right after, so a rerun can match a write whose response was lost.
fn run_writes(
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
            // the refusal is reported instead of blocking every later write.
            Err(error) if matches!(write.item, Item::Approve { .. }) => {
                let reason = error.to_string();
                change(ctx, target, |target| {
                    set_approval(target, &write.ids()[0], |approval| {
                        approval.refused = Some(reason.clone());
                    });
                    Ok(())
                })?;
                posted.push(json!({ "kind": write.kind(), "items": write.ids(), "error": reason }));
                continue;
            }
            Err(error) => return Err(error),
        };
        let remote = save(ctx, connection, target, &write.item, &response)?;
        if let Item::ApprovalReview { comment } = &write.item {
            let thread = comment.split('/').next().unwrap_or_default().to_owned();
            change(ctx, target, |target| {
                set_approval(target, &thread, |approval| approval.approved = true);
                Ok(())
            })?;
        }
        posted.push(json!({ "kind": write.kind(), "items": write.ids(), "remote_id": remote }));
        if let (Item::Draft { comment }, Some(draft)) = (&write.item, remote) {
            let publish = Write {
                item: Item::PublishDraft {
                    comment: comment.clone(),
                    draft: draft.clone(),
                },
                request: Some(
                    connection
                        .service
                        .publish_draft(connection.subject(), &draft),
                ),
                sent: vec![None],
            };
            run_writes(ctx, connection, target, vec![publish], attempt, posted)?;
        }
    }
    Ok(())
}

fn change<R>(ctx: &Ctx, base: &Target, edit: impl FnOnce(&mut Target) -> Result<R>) -> Result<R> {
    let now = clock::now()?;
    ctx.store
        .update(&ctx.dir, &base.key, &now, || base.clone(), edit)
}

fn comment_mut<'a>(target: &'a mut Target, id: &str) -> Option<&'a mut Comment> {
    target
        .threads
        .iter_mut()
        .flat_map(|thread| thread.comments.iter_mut())
        .find(|comment| comment.id == id)
}

fn set_approval(target: &mut Target, thread: &str, edit: impl FnOnce(&mut Approval)) {
    if let Some(approval) = target
        .threads
        .iter_mut()
        .find(|local| local.id == thread)
        .and_then(|local| local.approval.as_mut())
    {
        edit(approval);
    }
}

fn clear_pending(target: &mut Target, id: &str) {
    if let Some(thread) = target.threads.iter_mut().find(|thread| thread.id == id) {
        thread.resolve_pending = None;
    }
}

fn mark(target: &mut Target, write: &Write, attempt: &str) {
    let mut texts = write.sent.iter().cloned();
    let ids = match &write.item {
        Item::Review { comments } => comments.clone(),
        Item::Thread { comment }
        | Item::ApprovalReview { comment }
        | Item::Draft { comment }
        | Item::Reply { comment } => vec![comment.clone()],
        Item::Record { run, head } => {
            if let Some(run) = target
                .runs
                .iter_mut()
                .find(|candidate| candidate.id == *run)
            {
                run.record = Some(RecordPost {
                    post: PostState::Posting,
                    attempt: Some(attempt.to_owned()),
                    sent: texts.next().flatten(),
                    head: head.clone(),
                    remote_id: None,
                    url: None,
                });
            }
            return;
        }
        Item::PublishDraft { .. } | Item::Resolve { .. } | Item::Approve { .. } => return,
    };
    for id in ids {
        let text = texts.next().flatten();
        if let Some(comment) = comment_mut(target, &id) {
            comment.post = PostState::Posting;
            comment.attempt = Some(attempt.to_owned());
            comment.sent = text;
        }
    }
}

fn own(id: String, url: Option<String>, login: &str, review_id: Option<String>) -> RemoteComment {
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
fn save(
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
                        comment.post = PostState::Posted;
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
                    local.post = PostState::Posted;
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
        Item::Record { run, .. } => {
            let (id, url) = service
                .comment_created(review_url, response)
                .ok_or_else(|| Error(format!("no comment id returned for the record of {run}")))?;
            change(ctx, target, |target| {
                if let Some(record) = target
                    .runs
                    .iter_mut()
                    .find(|candidate| candidate.id == *run)
                    .and_then(|run| run.record.as_mut())
                {
                    record.post = PostState::Posted;
                    record.remote_id = Some(id.clone());
                    record.url = url;
                }
                Ok(())
            })?;
            Ok(Some(id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_name_the_side() {
        let anchor = Anchor {
            path: "a.txt".into(),
            line: None,
            old_line: Some(4),
            side: Side::Old,
            start_line: None,
            commit: None,
            tree: String::new(),
        };
        assert_eq!(location(&anchor), "a.txt (old):4");
    }

    #[test]
    fn short_names() {
        assert_eq!(short("0123456789"), "0123456");
        assert_eq!(harness_name("claude-code"), "Claude Code");
    }
}
