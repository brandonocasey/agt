//! Merging fetched forge threads into the stored target.

use super::Summary;
use super::records::{
    answered_id, finding_info, finding_kind, fixed_sha, parse_records, rejected_reason,
    reply_thread,
};
use super::trust::Trust;
use crate::forge::markers::{self, agent_attempt, agent_id};
use crate::forge::service::ReviewService;
use crate::forge::{CommentData, Draft, Fetched, ThreadData};
use crate::store::model::{
    Author, Comment, ImportedRecord, PostState, QaRun, RecordPost, RemoteKind, Resolution, Run,
    Target, Thread, ThreadType,
};

fn imported_comment(comment: CommentData, login: &str) -> Comment {
    comment.imported(login, comment.body.clone(), "ghost")
}

fn imported_id(service: &dyn ReviewService, data: &ThreadData) -> String {
    let first = data
        .comments
        .first()
        .map_or(data.info.id.as_str(), |comment| comment.id.as_str());
    service.imported_id(data.info.kind, first)
}

pub(super) fn merge(
    target: &mut Target,
    fetched: Fetched,
    login: &str,
    service: &dyn ReviewService,
    now: &str,
) -> Summary {
    let trust = Trust::for_target(target, login, service);
    // Records are read again from the comments the host still returns.
    let seen = seen_comments(&fetched.threads);
    target
        .records
        .retain(|record| !seen.contains(&record.comment_id));
    let Target {
        threads,
        runs,
        records,
        qa_runs,
        ..
    } = target;
    merge_threads(
        threads,
        fetched.threads,
        fetched.drafts,
        &trust,
        now,
        &mut |comment, trust| take_record(runs, qa_runs, records, comment, trust),
    )
}

fn seen_comments(threads: &[ThreadData]) -> Vec<String> {
    threads
        .iter()
        .flat_map(|data| data.comments.iter().map(|comment| comment.id.clone()))
        .collect()
}

/// Merges the host's threads and drafts into `threads`. `take_record` claims a verified
/// general comment as a run record, which only a review has; the rest needs nothing from the
/// subject, so issue threads merge the same way.
fn merge_threads(
    threads: &mut Vec<Thread>,
    fetched: Vec<ThreadData>,
    drafts: Vec<Draft>,
    trust: &Trust,
    now: &str,
    take_record: &mut dyn FnMut(&CommentData, &Trust) -> bool,
) -> Summary {
    let mut summary = Summary::default();
    let mut answers = Vec::new();
    let seen_threads: Vec<String> = fetched.iter().map(|data| data.info.id.clone()).collect();
    let seen_comments = seen_comments(&fetched);
    for data in fetched {
        if let [comment] = data.comments.as_slice()
            && trust.verified(comment)
        {
            let own = trust.own_id(comment);
            if data.info.kind == RemoteKind::Review
                && own.as_ref().is_some_and(|id| markers::is_review_id(id))
            {
                claim_paragraphs(threads, comment, trust);
                continue;
            }
            if data.info.kind == RemoteKind::General {
                if take_record(comment, trust) {
                    summary.records += 1;
                    continue;
                }
                if let Some(id) = answered_id(&comment.body, own.is_some())
                    .or_else(|| quoted_reply(threads, own.as_deref()?))
                {
                    answers.push((id, data));
                    continue;
                }
            }
        }
        import(threads, data, trust, now, &mut summary);
    }
    for (id, data) in answers {
        match threads.iter_mut().find(|thread| thread.id == id) {
            Some(thread) => {
                answer(thread, data, trust, now);
                summary.answered += 1;
            }
            None => import(threads, data, trust, now, &mut summary),
        }
    }
    // A draft that is gone was published or deleted, and a write that left no marker on the
    // host did not land or was rejected: unless a note matched above or a draft below, it goes
    // back to pending, to post again or for `finding kill`. The attempt id stays.
    for comment in threads
        .iter_mut()
        .flat_map(|thread| thread.comments.iter_mut())
        .filter(|comment| comment.remote.is_none())
    {
        if comment
            .draft_id
            .as_ref()
            .is_some_and(|id| !drafts.iter().any(|draft| &draft.id == id))
        {
            comment.draft_id = None;
        }
        if comment.draft_id.is_none() && comment.delivery.is_posting() {
            comment.delivery.post = PostState::Pending;
        }
    }
    for draft in drafts {
        let attempt = agent_attempt(&draft.body);
        let comment = match (agent_id(&draft.body), markers::finding_id(&draft.body)) {
            (Some(local), _) => threads
                .iter_mut()
                .flat_map(|thread| thread.comments.iter_mut())
                .find(|comment| comment.id == local && comment.delivery.is_attempt(attempt)),
            (None, Some(id)) => threads
                .iter_mut()
                .find(|thread| thread.id == id)
                .and_then(|thread| thread.comments.first_mut()),
            (None, None) => None,
        };
        if let Some(comment) = comment
            && comment.remote.is_none()
        {
            comment.delivery.post = PostState::Posting;
            comment.draft_id = Some(draft.id);
            summary.drafts += 1;
        }
    }
    summary.deleted = flag_deleted(threads, &seen_threads, &seen_comments, now);
    summary
}

/// Comments written before the first sync name git `user.name`; they are the authenticated
/// user's.
pub(super) fn claim_git_names(target: &mut Target, login: &str) {
    for comment in target.comments_mut() {
        if let Author::Human {
            login: name,
            git_name,
        } = &mut comment.author
            && *git_name
        {
            login.clone_into(name);
            *git_name = false;
        }
    }
}

/// Stores the records of a verified record or summary comment and marks our runs' records
/// posted when it is their write; false when the comment holds no record.
fn take_record(
    runs: &mut [Run],
    qa_runs: &mut [QaRun],
    records: &mut Vec<ImportedRecord>,
    comment: &CommentData,
    trust: &Trust,
) -> bool {
    let own = trust.own_id(comment);
    // A continuation of the summary holds no record, but is ours and is not a finding.
    if let Some(number) = own.as_deref().and_then(markers::summary_part) {
        let posts = runs
            .iter_mut()
            .filter_map(|run| run.record.as_mut())
            .chain(qa_runs.iter_mut().filter_map(|run| run.record.as_mut()));
        for part in posts.filter_map(|record| record.parts.get_mut(number - 2)) {
            if (part.remote_id.as_deref() == Some(comment.id.as_str())
                || (part.remote_id.is_none() && part.delivery.is_posting()))
                && (!part.delivery.is_posting()
                    || part.delivery.is_attempt(agent_attempt(&comment.body)))
            {
                part.delivery.post = PostState::Posted;
                part.remote_id = Some(comment.id.clone());
                part.url.clone_from(&comment.url);
            }
        }
        return true;
    }
    let found = parse_records(comment);
    let qa_found = markers::parse_qa_records(&comment.body);
    if found.is_empty() && qa_found.is_empty() {
        return false;
    }
    // A run's own record names the run in its id; the summary holds every run it lists.
    let claimed: Vec<&str> = match own.as_deref() {
        Some(markers::SUMMARY_ID) => found
            .iter()
            .map(|record| record.run.as_str())
            .chain(qa_found.iter().map(String::as_str))
            .collect(),
        Some(id) => markers::record_run(id).into_iter().collect(),
        None => Vec::new(),
    };
    let posts = runs
        .iter_mut()
        .filter(|run| claimed.contains(&run.id.as_str()))
        .filter_map(|run| run.record.as_mut())
        .chain(
            qa_runs
                .iter_mut()
                .filter(|run| claimed.contains(&run.id.as_str()))
                .filter_map(|run| run.record.as_mut()),
        );
    for post in posts {
        // While an edit of the comment is out, the host shows the old text until the edit's
        // attempt appears.
        if (post.remote_id.as_deref() == Some(comment.id.as_str())
            || (post.remote_id.is_none() && post.delivery.is_posting()))
            && (!post.delivery.is_posting()
                || post.delivery.is_attempt(agent_attempt(&comment.body)))
        {
            landed(post, comment.id.clone(), comment.url.clone());
        }
    }
    // One record per run: a later comment, such as the summary after a run's own record,
    // replaces an earlier one.
    records.retain(|known| {
        known.comment_id != comment.id && !found.iter().any(|record| record.run == known.run)
    });
    records.extend(found);
    true
}

/// The thread a verified own comment answers by its marker alone, as a quote reply does:
/// the thread has the comment's id, and not as its first comment.
fn quoted_reply(threads: &[Thread], own: &str) -> Option<String> {
    let id = reply_thread(own)?;
    let thread = threads.iter().find(|thread| thread.id == id)?;
    let first = thread.comments.first()?;
    (first.id != own).then(|| id.to_owned())
}

/// Marks a run's record landed in comment `id`. The summary texts it replaced are gone from
/// the host; a run's own record comment from before the summary stays.
pub(crate) fn landed(record: &mut RecordPost, id: String, url: Option<String>) {
    record.delivery.post = PostState::Posted;
    record.remote_id = Some(id);
    record.url = url;
    record
        .earlier
        .retain(|body| agent_id(body).as_deref() != Some(markers::SUMMARY_ID));
}

/// Marks threads and comments the host no longer returns. They are kept, never dropped.
fn flag_deleted(
    threads: &mut [Thread],
    seen_threads: &[String],
    comments: &[String],
    now: &str,
) -> usize {
    let mut flagged = 0;
    for thread in threads {
        if let Some(remote) = &thread.remote {
            let gone = !seen_threads.contains(&remote.id);
            if gone && !thread.deleted_remote {
                thread.deleted_seen_at = Some(now.to_owned());
                flagged += 1;
            }
            if !gone {
                thread.deleted_seen_at = None;
            }
            thread.deleted_remote = gone;
        }
        for comment in &mut thread.comments {
            let Some(remote) = &comment.remote else {
                continue;
            };
            let gone = !comments.contains(&remote.id);
            if gone && !comment.deleted_remote {
                comment.deleted_seen_at = Some(now.to_owned());
                flagged += 1;
            }
            if !gone {
                comment.deleted_seen_at = None;
            }
            comment.deleted_remote = gone;
        }
    }
    flagged
}

fn import(
    threads: &mut Vec<Thread>,
    data: ThreadData,
    trust: &Trust,
    now: &str,
    summary: &mut Summary,
) {
    let login = trust.login;
    let marker = data
        .comments
        .first()
        .filter(|first| trust.verified(first))
        .and_then(|first| markers::finding_id(&first.body));
    let by_remote = threads.iter().position(|thread| {
        thread.remote.as_ref().is_some_and(|remote| {
            // A GitLab note that gets a reply turns from general into a discussion with the
            // same id; GitHub ids of different kinds come from separate sequences.
            remote.id == data.info.id
                && (remote.kind == data.info.kind || trust.caps.notes_become_discussions)
        })
    });
    let by_agent = || {
        let incoming = data.comments.first()?;
        threads.iter().position(|thread| {
            thread.comments.first().is_some_and(|first| {
                // `publish` saves the remote ids of a review's comments before their
                // threads are known.
                first
                    .remote
                    .as_ref()
                    .is_some_and(|remote| remote.id == incoming.id)
                    || trust.claims(first, incoming)
            }) && thread
                .remote
                .as_ref()
                .is_none_or(|remote| remote.id == data.info.id)
        })
    };
    let by_marker = || {
        let marker = marker.as_deref()?;
        threads.iter().position(|thread| {
            thread.id == marker
                && thread
                    .remote
                    .as_ref()
                    .is_none_or(|remote| remote.id == data.info.id)
        })
    };
    if let Some(index) = by_remote.or_else(by_agent).or_else(by_marker) {
        update(&mut threads[index], data, marker.is_some(), trust, now);
        summary.updated += 1;
        return;
    }
    let taken =
        |id: &str| threads.iter().any(|thread| thread.id == id) || trust.killed.contains(id);
    let id = match marker {
        Some(marker) if !taken(&marker) => marker,
        _ => {
            let base = imported_id(trust.service, &data);
            let mut id = base.clone();
            let mut number = 1;
            while taken(&id) {
                number += 1;
                id = format!("{base}-{number}");
            }
            id
        }
    };
    let finding = data
        .comments
        .first()
        .filter(|_| finding_info(&id).is_some())
        .map(|first| finding_kind(&first.body));
    let (kind, severity) = finding.unwrap_or((ThreadType::Comment, None));
    let created_at = data
        .comments
        .first()
        .map_or_else(|| now.to_owned(), |first| first.created_at.clone());
    let comments = data
        .comments
        .into_iter()
        .map(|comment| imported_comment(comment, login))
        .collect();
    threads.push(Thread {
        finding: finding_info(&id),
        severity,
        resolved: data.info.resolvable && data.resolved,
        remote: Some(data.info),
        ..Thread::new(id, kind, comments, created_at, now.to_owned())
    });
    summary.imported += 1;
}

/// Claims the comments a review body of mine carries, each under its own marker: a response lost
/// after the review landed leaves them pending, and posting them again would repeat the review.
fn claim_paragraphs(threads: &mut [Thread], review: &CommentData, trust: &Trust) {
    let marks = markers::agent_markers(&review.body);
    for comment in threads
        .iter_mut()
        .flat_map(|thread| thread.comments.iter_mut())
        .filter(|comment| comment.remote.is_none() && comment.delivery.is_posting())
    {
        let landed = marks.iter().any(|(id, attempt)| {
            *id == comment.id && comment.delivery.is_attempt(attempt.as_deref())
        });
        if landed {
            comment.remote = Some(review.remote(trust.login));
            comment.delivery.post = PostState::Posted;
            comment.draft_id = None;
        }
    }
}

/// Remote wins for posted text and resolved state; local fields and unposted comments stay.
fn update(thread: &mut Thread, data: ThreadData, by_marker: bool, trust: &Trust, now: &str) {
    let login = trust.login;
    if data.info.resolvable {
        match thread.resolve_pending {
            Some(pending) if pending == data.resolved => thread.resolve_pending = None,
            Some(_) => {}
            None => {
                thread.set_remote_resolved(data.resolved, now);
                if data.resolved {
                    thread.held = false;
                }
            }
        }
    }
    thread.remote = Some(data.info);
    let mut used = vec![false; thread.comments.len()];
    let mut merged = Vec::new();
    for (index, incoming) in data.comments.into_iter().enumerate() {
        let found = thread
            .comments
            .iter()
            .position(|comment| {
                comment
                    .remote
                    .as_ref()
                    .is_some_and(|remote| remote.id == incoming.id)
            })
            .or_else(|| {
                thread
                    .comments
                    .iter()
                    .position(|comment| trust.claims(comment, &incoming))
            })
            .or_else(|| {
                let first_unposted = thread
                    .comments
                    .first()
                    .is_some_and(|first| first.remote.is_none());
                (index == 0 && by_marker && first_unposted).then_some(0)
            })
            .filter(|found| !used[*found]);
        match found {
            Some(found) => {
                used[found] = true;
                let mut comment = thread.comments[found].clone();
                comment.remote = Some(incoming.remote(login));
                // The review that carries an approval holds only its marker; the approval text
                // stays in the store.
                let marker_only =
                    thread.approval.is_some() && markers::strip_agent(&incoming.body).is_empty();
                if !marker_only {
                    comment.body = incoming.body;
                }
                comment.delivery.post = PostState::Posted;
                comment.draft_id = None;
                merged.push(comment);
            }
            None => merged.push(imported_comment(incoming, login)),
        }
    }
    merged.extend(
        thread
            .comments
            .iter()
            .zip(used)
            .filter(|(_, used)| !used)
            .map(|(comment, _)| comment.clone()),
    );
    thread.comments = merged;
    thread.updated_at = now.to_owned();
}

/// A general finding is answered by a later `Fixed in <sha>. ... Re: <id>` comment by me.
fn answer(thread: &mut Thread, data: ThreadData, trust: &Trust, now: &str) {
    let login = trust.login;
    for comment in data.comments {
        let known = thread.comments.iter().any(|existing| {
            existing
                .remote
                .as_ref()
                .is_some_and(|remote| remote.id == comment.id)
        });
        if known {
            continue;
        }
        if let Some(existing) = thread
            .comments
            .iter_mut()
            .find(|existing| trust.claims(existing, &comment))
        {
            existing.remote = Some(comment.remote(login));
            existing.delivery.post = PostState::Posted;
            existing.draft_id = None;
            continue;
        }
        let resolvable = thread
            .remote
            .as_ref()
            .is_some_and(|remote| remote.resolvable);
        if !resolvable && thread.resolve_pending.is_none() {
            let fixed = fixed_sha(&comment.body);
            let rejected = rejected_reason(&comment.body);
            thread.set_remote_resolved(fixed.is_some() || rejected.is_some(), now);
            if thread.resolved {
                thread.held = false;
            }
            if thread.resolution.is_none() {
                thread.resolution = match (fixed, rejected) {
                    (Some(sha), _) => Some(Resolution::Fixed { sha }),
                    (None, Some(reason)) => Some(Resolution::Rejected { reason }),
                    (None, None) => None,
                };
            }
        }
        thread.comments.push(imported_comment(comment, login));
    }
    thread.updated_at = now.to_owned();
}
