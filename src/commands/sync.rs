use serde_json::{Value, json};

use super::{Adoption, Ctx, Scope, prune_dir};
use crate::anchors;
use crate::clock;
use crate::error::{Error, Result, fail};
use crate::forge::{self, Client, CommentData, Fetched, ThreadData};
use crate::git::{Repo, split_remote};
use crate::github;
use crate::gitlab;
use crate::model::{
    Anchor, Author, Comment, FindingInfo, Forge, ImportedRecord, PostState, RemoteComment,
    RemoteKind, Review, Severity, Side, Target, TargetKind, Thread, ThreadType,
};
use crate::store::Account;

const FINDING_MARKER: &str = "<!-- review-finding v1 id=";
const RECORD_MARKER: &str = "<!-- review-record v2 ";

#[derive(Default)]
struct Summary {
    imported: usize,
    updated: usize,
    records: usize,
    answered: usize,
    drafts: usize,
}

pub(super) fn sync(ctx: &Ctx, pr: Option<&str>) -> Result<Value> {
    let pr_ref = pr.map(forge::parse_pr).transpose()?;
    let origin = ctx.repo.origin_url()?.and_then(|url| split_remote(&url));
    let location = pr_ref
        .as_ref()
        .and_then(|pr| pr.location.clone())
        .or(origin);
    let Some((host, origin_repo)) = location else {
        return fail("sync needs a GitHub or GitLab origin remote, or --pr <url>");
    };
    let forge = if host == "github.com" {
        Forge::Github
    } else {
        Forge::Gitlab
    };
    let client = match forge {
        Forge::Github => Client::gh(),
        Forge::Gitlab => Client::glab(&host),
    };
    let login = match forge {
        Forge::Github => github::login(&client)?,
        Forge::Gitlab => gitlab::login(&client)?,
    };
    let branch = ctx.repo.current_branch()?;
    let (repo, number) = match &pr_ref {
        Some(pr) => (origin_repo, pr.number),
        None => {
            let branch = branch
                .as_deref()
                .ok_or_else(|| Error("HEAD is detached; pass --pr <number|url>".into()))?;
            let found = match forge {
                Forge::Github => github::find_pr(&client, &origin_repo, branch)?,
                Forge::Gitlab => gitlab::find_mr(&client, &origin_repo, branch)?,
            };
            found.ok_or_else(|| {
                Error(format!(
                    "no PR/MR found for branch {branch} in {origin_repo}"
                ))
            })?
        }
    };
    let mut fetched = match forge {
        Forge::Github => github::fetch(&client, &repo, number)?,
        Forge::Gitlab => gitlab::fetch(&client, &repo, number)?,
    };
    mark_outdated(&ctx.repo, &mut fetched);

    let key = match forge {
        Forge::Github => format!("gh-{number}"),
        Forge::Gitlab => format!("gl-{number}"),
    };
    let (scope, local) = sync_scope(ctx, &key, &fetched, branch.as_deref())?;
    let adoption = if local && scope.existing.is_some() {
        Some(ctx.adoption(&scope)?)
    } else {
        None
    };
    let from = scope.key.clone();
    let renamed = scope.existing.is_some() && from != key;
    let now = clock::now()?;
    let review = Review {
        forge,
        host: host.clone(),
        repo: fetched.pr.repo.clone(),
        number,
        url: fetched.pr.url.clone(),
        state: fetched.pr.state,
        closed_at: fetched.pr.closed_at.clone(),
        head_branch: fetched.pr.head_branch.clone(),
        head_sha: fetched.pr.head_sha.clone(),
        base_sha: fetched.pr.base_sha.clone(),
        start_sha: fetched.pr.start_sha.clone(),
        login: login.clone(),
        synced_at: now.clone(),
    };
    let (summary, recorded, total) = ctx.store.update_renamed(
        &ctx.dir,
        &from,
        &key,
        &now,
        || ctx.new_target(&scope, &now),
        |target| {
            target.review = Some(review.clone());
            if target.branch.is_none() {
                target.branch = Some(fetched.pr.head_branch.clone());
            }
            if target.branch_birth.is_none() {
                target.branch_birth.clone_from(&scope.birth);
            }
            let summary = merge(target, fetched, &login, forge, &now);
            let recorded = adoption
                .as_ref()
                .map(|adoption: &Adoption| adoption.apply(target))
                .unwrap_or_default();
            Ok((summary, recorded, target.threads.len()))
        },
    )?;
    ctx.store.save_account(
        &ctx.dir,
        &Account {
            forge,
            host,
            login: login.clone(),
        },
    )?;
    let pruned = prune_dir(&ctx.store, &ctx.dir, clock::now_secs()?, false)?;
    Ok(json!({
        "target": key,
        "renamed_from": renamed.then_some(from),
        "login": login,
        "review": review,
        "threads": {
            "imported": summary.imported,
            "updated": summary.updated,
            "total": total,
        },
        "records": summary.records,
        "answered": summary.answered,
        "drafts": summary.drafts,
        "recorded": recorded,
        "pruned": pruned,
    }))
}

/// The target to sync into: the current branch's target when the PR/MR is from this branch,
/// `--target` when given, else the PR/MR key itself. The flag says whether the scope's head is
/// a local commit, which patch-id adoption needs.
fn sync_scope(
    ctx: &Ctx,
    key: &str,
    fetched: &Fetched,
    branch: Option<&str>,
) -> Result<(Scope, bool)> {
    let own_branch = branch == Some(fetched.pr.head_branch.as_str());
    if ctx.selection.target.is_some() || (own_branch && ctx.selection.range.is_none()) {
        return Ok((ctx.scope()?, true));
    }
    Ok((
        Scope {
            key: key.to_owned(),
            kind: TargetKind::Branch,
            branch: Some(fetched.pr.head_branch.clone()),
            birth: None,
            base: fetched.pr.base_sha.clone(),
            head: fetched.pr.head_sha.clone(),
            existing: ctx.store.load(&ctx.dir, key)?,
        },
        false,
    ))
}

/// GitLab gives no outdated flag: a position is outdated when its head is not the latest diff
/// head and the line does not map to it. A position git cannot diff counts as outdated.
fn mark_outdated(repo: &Repo, fetched: &mut Fetched) {
    let latest = fetched.pr.head_sha.clone();
    for thread in &mut fetched.threads {
        let Some(head) = thread.position_head.clone() else {
            continue;
        };
        let Some(path) = thread
            .info
            .path
            .clone()
            .or_else(|| thread.info.old_path.clone())
        else {
            continue;
        };
        let outdated = head != latest && {
            let anchor = Anchor {
                path,
                line: thread.info.line,
                old_line: thread.info.old_line,
                side: thread.info.side.unwrap_or(Side::New),
                start_line: thread.info.start_line,
                commit: Some(head.clone()),
                tree: String::new(),
            };
            match repo.diff_u0(&head, &latest) {
                Ok(diff) => anchors::locate(&anchors::parse(&diff), &anchor).outdated,
                Err(_) => true,
            }
        };
        thread.info.outdated = Some(outdated);
    }
}

/// Trust rule: the author is the authenticated user and nobody else edited the comment.
fn trusted(comment: &CommentData, login: &str) -> bool {
    comment.login.as_deref() == Some(login)
        && comment
            .editor
            .as_deref()
            .is_none_or(|editor| editor == login)
}

fn marker_id(body: &str) -> Option<String> {
    let rest = body.split_once(FINDING_MARKER)?.1;
    let id = rest.split_whitespace().next()?.trim_end_matches("-->");
    (!id.is_empty()).then(|| id.to_owned())
}

fn parse_record(comment: &CommentData) -> Option<ImportedRecord> {
    let inner = comment
        .body
        .split_once(RECORD_MARKER)?
        .1
        .split_once("-->")?
        .0;
    let field = |name: &str| {
        inner
            .split_whitespace()
            .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
    };
    Some(ImportedRecord {
        run: field("id")?.to_owned(),
        head: field("head")?.to_owned(),
        reviews_at_head: field("reviews-at-head")?.parse().ok()?,
        open: field("open").and_then(|open| open.parse().ok()),
        comment_id: comment.id.clone(),
        url: comment.url.clone(),
        created_at: comment.created_at.clone(),
    })
}

/// The finding id a `Fixed in ... Re: <id>` general comment answers.
fn answered_id(body: &str) -> Option<String> {
    let body = body.trim_end();
    let (before, last) = body.rsplit_once('\n').unwrap_or(("", body));
    let id = last.trim().strip_prefix("Re: ")?.trim();
    (before.contains("Fixed in") && !id.is_empty()).then(|| id.to_owned())
}

fn fixed_sha(body: &str) -> Option<String> {
    let rest = body.split_once("Fixed in ")?.1;
    let sha = rest.split_whitespace().next()?.trim_end_matches(['.', ',']);
    (!sha.is_empty()).then(|| sha.to_owned())
}

/// Type and severity from a `**bug (medium):**` heading.
fn finding_kind(body: &str) -> (ThreadType, Option<Severity>) {
    let heading = body
        .trim_start()
        .strip_prefix("**")
        .and_then(|rest| rest.split_once(":**"))
        .map(|(heading, _)| heading);
    let Some(heading) = heading else {
        return (ThreadType::Comment, None);
    };
    let (kind, severity) = match heading.split_once(" (") {
        Some((kind, severity)) => (kind, severity.strip_suffix(')')),
        None => (heading, None),
    };
    let kind = match kind {
        "bug" => ThreadType::Bug,
        "question" => ThreadType::Question,
        "requirement" => ThreadType::Requirement,
        "nit" => ThreadType::Nit,
        _ => ThreadType::Comment,
    };
    let severity = match severity {
        Some("high") => Some(Severity::High),
        Some("medium") => Some(Severity::Medium),
        Some("low") => Some(Severity::Low),
        _ => None,
    };
    (kind, severity)
}

fn finding_info(id: &str) -> Option<FindingInfo> {
    let mut parts = id.split('.');
    let (run, pass, reviewer, number) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !number.starts_with('f') {
        return None;
    }
    Some(FindingInfo {
        run: run.to_owned(),
        pass: pass.to_owned(),
        reviewer: reviewer.to_owned(),
        evidence: None,
        tests_run: Vec::new(),
        post_reveal: false,
    })
}

fn remote_comment(comment: &CommentData, login: &str) -> RemoteComment {
    RemoteComment {
        id: comment.id.clone(),
        url: comment.url.clone(),
        login: comment.login.clone(),
        is_me: comment.login.as_deref() == Some(login),
        editor: comment.editor.clone(),
        review_id: comment.review_id.clone(),
    }
}

fn imported_comment(comment: CommentData, login: &str) -> Comment {
    Comment {
        author: Author::Human {
            login: comment.login.clone().unwrap_or_else(|| "ghost".to_owned()),
        },
        remote: Some(remote_comment(&comment, login)),
        body: comment.body,
        created_at: comment.created_at,
        post: PostState::Posted,
        draft_id: None,
    }
}

fn imported_id(forge: Forge, data: &ThreadData) -> String {
    let first = data
        .comments
        .first()
        .map_or(data.info.id.as_str(), |comment| comment.id.as_str());
    match (forge, data.info.kind) {
        (Forge::Github, RemoteKind::ReviewThread) => format!("gh-t{first}"),
        (Forge::Github, RemoteKind::Review) => format!("gh-r{first}"),
        (Forge::Github, _) => format!("gh-c{first}"),
        (Forge::Gitlab, _) => format!("gl-n{first}"),
    }
}

fn merge(target: &mut Target, fetched: Fetched, login: &str, forge: Forge, now: &str) -> Summary {
    let mut summary = Summary::default();
    let mut answers = Vec::new();
    for data in fetched.threads {
        if let [comment] = data.comments.as_slice()
            && data.info.kind == RemoteKind::General
            && trusted(comment, login)
        {
            if let Some(record) = parse_record(comment) {
                target
                    .records
                    .retain(|known| known.comment_id != record.comment_id);
                target.records.push(record);
                summary.records += 1;
                continue;
            }
            if let Some(id) = answered_id(&comment.body) {
                answers.push((id, data));
                continue;
            }
        }
        import(target, data, login, forge, now, &mut summary);
    }
    for (id, data) in answers {
        match target.threads.iter_mut().find(|thread| thread.id == id) {
            Some(thread) => {
                answer(thread, data, login, now);
                summary.answered += 1;
            }
            None => import(target, data, login, forge, now, &mut summary),
        }
    }
    for draft in fetched.drafts {
        let Some(id) = marker_id(&draft.body) else {
            continue;
        };
        let first = target
            .threads
            .iter_mut()
            .find(|thread| thread.id == id)
            .and_then(|thread| thread.comments.first_mut());
        if let Some(first) = first
            && first.remote.is_none()
        {
            first.post = PostState::Posting;
            first.draft_id = Some(draft.id);
            summary.drafts += 1;
        }
    }
    summary
}

fn import(
    target: &mut Target,
    data: ThreadData,
    login: &str,
    forge: Forge,
    now: &str,
    summary: &mut Summary,
) {
    let marker = data
        .comments
        .first()
        .filter(|first| trusted(first, login))
        .and_then(|first| marker_id(&first.body));
    let by_remote = target.threads.iter().position(|thread| {
        thread
            .remote
            .as_ref()
            .is_some_and(|remote| remote.id == data.info.id && remote.kind == data.info.kind)
    });
    let by_marker = || {
        let marker = marker.as_deref()?;
        target.threads.iter().position(|thread| {
            thread.id == marker
                && thread
                    .remote
                    .as_ref()
                    .is_none_or(|remote| remote.id == data.info.id)
        })
    };
    if let Some(index) = by_remote.or_else(by_marker) {
        update(
            &mut target.threads[index],
            data,
            marker.is_some(),
            login,
            now,
        );
        summary.updated += 1;
        return;
    }
    let taken = |id: &str| target.threads.iter().any(|thread| thread.id == id);
    let id = match marker {
        Some(marker) if !taken(&marker) => marker,
        _ => imported_id(forge, &data),
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
    target.threads.push(Thread {
        finding: finding_info(&id),
        id,
        kind,
        severity,
        anchor: None,
        resolved: data.info.resolvable && data.resolved,
        resolution: None,
        triage: None,
        held: false,
        comments: data
            .comments
            .into_iter()
            .map(|comment| imported_comment(comment, login))
            .collect(),
        remote: Some(data.info),
        created_at,
        updated_at: now.to_owned(),
    });
    summary.imported += 1;
}

/// Remote wins for posted text and resolved state; local fields and unposted comments stay.
fn update(thread: &mut Thread, data: ThreadData, by_marker: bool, login: &str, now: &str) {
    if data.info.resolvable {
        thread.resolved = data.resolved;
        if data.resolved {
            thread.held = false;
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
                comment.remote = Some(remote_comment(&incoming, login));
                comment.body = incoming.body;
                comment.post = PostState::Posted;
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
fn answer(thread: &mut Thread, data: ThreadData, login: &str, now: &str) {
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
        if !thread
            .remote
            .as_ref()
            .is_some_and(|remote| remote.resolvable)
        {
            thread.resolved = true;
            thread.held = false;
            if thread.resolution.is_none() {
                thread.resolution = fixed_sha(&comment.body).map(|sha| format!("fixed {sha}"));
            }
        }
        thread.comments.push(imported_comment(comment, login));
    }
    thread.updated_at = now.to_owned();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_markers_and_records() {
        let body = "**bug (medium):** x\n<!-- review-finding v1 id=rv-17kj.c1p0.r1.f1 sha=abc -->";
        assert_eq!(marker_id(body).as_deref(), Some("rv-17kj.c1p0.r1.f1"));
        assert!(matches!(
            finding_kind(body),
            (ThreadType::Bug, Some(Severity::Medium))
        ));
        let comment = CommentData {
            id: "1".into(),
            url: None,
            body: "**Review rv-7k2m:** ...\n<!-- review-record v2 id=rv-7k2m head=abc reviews-at-head=2 open=1 -->".into(),
            created_at: "2026-10-16T12:00:00Z".into(),
            login: Some("me".into()),
            editor: None,
            review_id: None,
        };
        let record = parse_record(&comment).unwrap();
        assert_eq!(
            (record.run.as_str(), record.reviews_at_head, record.open),
            ("rv-7k2m", 2, Some(1))
        );
        assert_eq!(
            answered_id("Fixed in 9e8d7c6.\n\nsig\nRe: rv-7k2m.c1p0.r1.f2").as_deref(),
            Some("rv-7k2m.c1p0.r1.f2")
        );
        assert_eq!(answered_id("Re: rv-7k2m.c1p0.r1.f2"), None);
        assert_eq!(
            fixed_sha("Fixed in 9e8d7c6.\nRe: x").as_deref(),
            Some("9e8d7c6")
        );
    }
}
