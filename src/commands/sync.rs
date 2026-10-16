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

pub(super) const FINDING_MARKER: &str = "<!-- review-finding v1 id=";
pub(super) const RECORD_MARKER: &str = "<!-- review-record v2 ";
pub(super) const AGENT_MARKER: &str = "<!-- agent-review id=";
/// Agent id prefix of the body of a GitHub review that `publish` posts; sync skips those.
pub(super) const REVIEW_ID: &str = "review/";

#[derive(Default)]
struct Summary {
    imported: usize,
    updated: usize,
    deleted: usize,
    records: usize,
    answered: usize,
    drafts: usize,
}

/// The forge, authenticated login and PR/MR a `sync` or `publish` works on.
pub(super) struct Connection {
    pub forge: Forge,
    pub host: String,
    pub client: Client,
    pub login: String,
    pub repo: String,
    pub number: u64,
    pub key: String,
}

pub(super) fn connect(ctx: &Ctx, pr: Option<&str>) -> Result<Connection> {
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
    let (repo, number) = match &pr_ref {
        Some(pr) => (origin_repo, pr.number),
        None => {
            let branch = ctx
                .repo
                .current_branch()?
                .ok_or_else(|| Error("HEAD is detached; pass --pr <number|url>".into()))?;
            let found = match forge {
                Forge::Github => github::find_pr(&client, &origin_repo, &branch)?,
                Forge::Gitlab => gitlab::find_mr(&client, &origin_repo, &branch)?,
            };
            found.ok_or_else(|| {
                Error(format!(
                    "no PR/MR found for branch {branch} in {origin_repo}"
                ))
            })?
        }
    };
    let key = match forge {
        Forge::Github => format!("gh-{number}"),
        Forge::Gitlab => format!("gl-{number}"),
    };
    Ok(Connection {
        forge,
        host,
        client,
        login,
        repo,
        number,
        key,
    })
}

/// Everything fetched and computed before the lock: the target to merge into and the merge
/// inputs.
pub(super) struct Plan {
    pub scope: Scope,
    fetched: Fetched,
    adoption: Option<Adoption>,
    pub review: Review,
    now: String,
}

pub(super) fn plan(ctx: &Ctx, connection: &Connection) -> Result<Plan> {
    let mut fetched = match connection.forge {
        Forge::Github => github::fetch(&connection.client, &connection.repo, connection.number)?,
        Forge::Gitlab => gitlab::fetch(&connection.client, &connection.repo, connection.number)?,
    };
    mark_outdated(&ctx.repo, &mut fetched);
    let branch = ctx.repo.current_branch()?;
    let (scope, local) = sync_scope(ctx, &connection.key, &fetched, branch.as_deref())?;
    let adoption = if local && scope.existing.is_some() {
        Some(ctx.adoption(&scope)?)
    } else {
        None
    };
    let now = clock::now()?;
    let review = Review {
        forge: connection.forge,
        host: connection.host.clone(),
        repo: fetched.pr.repo.clone(),
        number: connection.number,
        url: fetched.pr.url.clone(),
        state: fetched.pr.state,
        closed_at: fetched.pr.closed_at.clone(),
        head_branch: fetched.pr.head_branch.clone(),
        head_sha: fetched.pr.head_sha.clone(),
        base_sha: fetched.pr.base_sha.clone(),
        start_sha: fetched.pr.start_sha.clone(),
        author: fetched.pr.author.clone(),
        login: connection.login.clone(),
        synced_at: now.clone(),
    };
    Ok(Plan {
        scope,
        fetched,
        adoption,
        review,
        now,
    })
}

impl Plan {
    /// Merges the fetched PR/MR into `target`; returns the merge summary and adopted passes.
    fn apply(self, target: &mut Target, connection: &Connection) -> (Summary, Vec<String>) {
        target.review = Some(self.review);
        if target.branch.is_none() {
            target.branch = Some(self.fetched.pr.head_branch.clone());
        }
        if target.branch_birth.is_none() {
            target.branch_birth.clone_from(&self.scope.birth);
        }
        let summary = merge(
            target,
            self.fetched,
            &connection.login,
            connection.forge,
            &self.now,
        );
        let recorded = self
            .adoption
            .as_ref()
            .map(|adoption| adoption.apply(target))
            .unwrap_or_default();
        (summary, recorded)
    }

    /// The target as a sync would leave it, without writing anything.
    pub(super) fn preview(self, ctx: &Ctx, connection: &Connection) -> Target {
        let mut target = self
            .scope
            .existing
            .clone()
            .unwrap_or_else(|| ctx.new_target(&self.scope, &self.now));
        target.key.clone_from(&connection.key);
        self.apply(&mut target, connection);
        target
    }
}

pub(super) fn sync(ctx: &Ctx, pr: Option<&str>) -> Result<Value> {
    let connection = connect(ctx, pr)?;
    sync_with(ctx, &connection)
}

pub(super) fn sync_with(ctx: &Ctx, connection: &Connection) -> Result<Value> {
    let plan = plan(ctx, connection)?;
    sync_planned(ctx, connection, plan)
}

/// Writes a planned sync to the store under the PR/MR key.
pub(super) fn sync_planned(ctx: &Ctx, connection: &Connection, plan: Plan) -> Result<Value> {
    let from = plan.scope.key.clone();
    let key = connection.key.clone();
    let renamed = plan.scope.existing.is_some() && from != key;
    let review = plan.review.clone();
    let now = plan.now.clone();
    let scope_for_init = Scope {
        key: key.clone(),
        kind: plan.scope.kind,
        branch: plan.scope.branch.clone(),
        birth: plan.scope.birth.clone(),
        base: plan.scope.base.clone(),
        head: plan.scope.head.clone(),
        existing: None,
    };
    let (summary, recorded, total) = ctx.store.update_renamed(
        &ctx.dir,
        &from,
        &key,
        &now,
        || ctx.new_target(&scope_for_init, &now),
        |target| {
            let (summary, recorded) = plan.apply(target, connection);
            Ok((summary, recorded, target.threads.len()))
        },
    )?;
    ctx.store.save_account(
        &ctx.dir,
        &Account {
            forge: connection.forge,
            host: connection.host.clone(),
            login: connection.login.clone(),
        },
    )?;
    let pruned = prune_dir(&ctx.store, &ctx.dir, clock::now_secs()?, false)?;
    Ok(json!({
        "target": key,
        "renamed_from": renamed.then_some(from),
        "login": connection.login,
        "review": review,
        "threads": {
            "imported": summary.imported,
            "updated": summary.updated,
            "deleted": summary.deleted,
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
/// head and the line does not map to it. Without the commits locally the state is unknown.
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
        if head == latest {
            thread.info.outdated = Some(false);
            continue;
        }
        let anchor = Anchor {
            path,
            line: thread.info.line,
            old_line: thread.info.old_line,
            side: thread.info.side.unwrap_or(Side::New),
            start_line: thread.info.start_line,
            commit: Some(head.clone()),
            tree: String::new(),
        };
        thread.info.outdated = repo
            .diff_u0(&head, &latest)
            .ok()
            .map(|diff| anchors::locate(&anchors::parse(&diff), &anchor).outdated);
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

/// The local id in an `<!-- agent-review id=<id> -->` marker, which `publish` adds to every body.
pub(super) fn agent_id(body: &str) -> Option<String> {
    let rest = body.split_once(AGENT_MARKER)?.1;
    let id = rest.split_whitespace().next()?.trim_end_matches("-->");
    (!id.is_empty()).then(|| id.to_owned())
}

/// The agent-review id of a trusted comment.
fn own_id(comment: &CommentData, login: &str) -> Option<String> {
    trusted(comment, login)
        .then(|| agent_id(&comment.body))
        .flatten()
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

/// The thread id a general comment answers: a `Fixed in ... Re: <id>` comment, or any
/// `... Re: <id>` reply that `publish` marked.
fn answered_id(body: &str, marked: bool) -> Option<String> {
    let body = body.trim_end();
    let (before, last) = body.rsplit_once('\n').unwrap_or(("", body));
    let id = last.trim().strip_prefix("Re: ")?.trim();
    ((marked || before.contains("Fixed in")) && !id.is_empty()).then(|| id.to_owned())
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
        id: format!("remote-{}", comment.id),
        attempt: None,
        deleted_remote: false,
        deleted_seen_at: None,
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
    let seen_threads: Vec<String> = fetched
        .threads
        .iter()
        .map(|data| data.info.id.clone())
        .collect();
    let seen_comments: Vec<String> = fetched
        .threads
        .iter()
        .flat_map(|data| data.comments.iter().map(|comment| comment.id.clone()))
        .collect();
    for data in fetched.threads {
        if let [comment] = data.comments.as_slice()
            && data.info.kind == RemoteKind::General
            && trusted(comment, login)
        {
            if let Some(record) = parse_record(comment) {
                if let Some(local) = own_id(comment, login)
                    && let Some(run_id) = local.strip_suffix("/record")
                    && let Some(run) = target.runs.iter_mut().find(|run| run.id == run_id)
                    && let Some(post) = &mut run.record
                {
                    post.post = PostState::Posted;
                    post.remote_id = Some(comment.id.clone());
                    post.url.clone_from(&comment.url);
                }
                target
                    .records
                    .retain(|known| known.comment_id != record.comment_id);
                target.records.push(record);
                summary.records += 1;
                continue;
            }
            let own = own_id(comment, login);
            if let Some(id) = answered_id(&comment.body, own.is_some()) {
                answers.push((id, data));
                continue;
            }
            if data.info.kind == RemoteKind::Review
                && own.is_some_and(|id| id.starts_with(REVIEW_ID))
            {
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
    // A draft that is gone was published or deleted; the comment is posted again unless a
    // published note matched it above.
    for comment in target
        .threads
        .iter_mut()
        .flat_map(|thread| thread.comments.iter_mut())
        .filter(|comment| comment.remote.is_none())
    {
        if comment
            .draft_id
            .as_ref()
            .is_some_and(|id| !fetched.drafts.iter().any(|draft| &draft.id == id))
        {
            comment.draft_id = None;
        }
    }
    for draft in fetched.drafts {
        let comment = match (agent_id(&draft.body), marker_id(&draft.body)) {
            (Some(local), _) => target
                .threads
                .iter_mut()
                .flat_map(|thread| thread.comments.iter_mut())
                .find(|comment| comment.id == local),
            (None, Some(id)) => target
                .threads
                .iter_mut()
                .find(|thread| thread.id == id)
                .and_then(|thread| thread.comments.first_mut()),
            (None, None) => None,
        };
        if let Some(comment) = comment
            && comment.remote.is_none()
        {
            comment.post = PostState::Posting;
            comment.draft_id = Some(draft.id);
            summary.drafts += 1;
        }
    }
    summary.deleted = flag_deleted(target, &seen_threads, &seen_comments, now);
    summary
}

/// Marks threads and comments the host no longer returns. They are kept, never dropped.
fn flag_deleted(target: &mut Target, threads: &[String], comments: &[String], now: &str) -> usize {
    let mut flagged = 0;
    for thread in &mut target.threads {
        if let Some(remote) = &thread.remote {
            let gone = !threads.contains(&remote.id);
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
    let by_agent = || {
        let local = own_id(data.comments.first()?, login)?;
        target.threads.iter().position(|thread| {
            thread
                .comments
                .first()
                .is_some_and(|first| first.id == local)
                && thread
                    .remote
                    .as_ref()
                    .is_none_or(|remote| remote.id == data.info.id)
        })
    };
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
    if let Some(index) = by_remote.or_else(by_agent).or_else(by_marker) {
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
        resolve_pending: None,
        deleted_remote: false,
        deleted_seen_at: None,
        created_at,
        updated_at: now.to_owned(),
    });
    summary.imported += 1;
}

/// Remote wins for posted text and resolved state; local fields and unposted comments stay.
fn update(thread: &mut Thread, data: ThreadData, by_marker: bool, login: &str, now: &str) {
    if data.info.resolvable {
        match thread.resolve_pending {
            Some(pending) if pending == data.resolved => thread.resolve_pending = None,
            Some(_) => {}
            None => {
                thread.resolved = data.resolved;
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
                let local = own_id(&incoming, login)?;
                thread
                    .comments
                    .iter()
                    .position(|comment| comment.id == local && comment.remote.is_none())
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
        let local = own_id(&comment, login);
        if let Some(existing) = thread
            .comments
            .iter_mut()
            .find(|existing| Some(&existing.id) == local.as_ref() && existing.remote.is_none())
        {
            existing.remote = Some(remote_comment(&comment, login));
            existing.post = PostState::Posted;
            existing.draft_id = None;
            continue;
        }
        let resolvable = thread
            .remote
            .as_ref()
            .is_some_and(|remote| remote.resolvable);
        if !resolvable && thread.resolve_pending.is_none() {
            let fixed = fixed_sha(&comment.body);
            thread.resolved = fixed.is_some();
            if let Some(sha) = fixed {
                thread.held = false;
                if thread.resolution.is_none() {
                    thread.resolution = Some(format!("fixed {sha}"));
                }
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
            answered_id("Fixed in 9e8d7c6.\n\nsig\nRe: rv-7k2m.c1p0.r1.f2", false).as_deref(),
            Some("rv-7k2m.c1p0.r1.f2")
        );
        assert_eq!(answered_id("Re: rv-7k2m.c1p0.r1.f2", false), None);
        assert_eq!(
            answered_id("Not fixed: x\n<!-- agent-review id=h1/c2 -->\nRe: h1", true).as_deref(),
            Some("h1")
        );
        assert_eq!(
            fixed_sha("Fixed in 9e8d7c6.\nRe: x").as_deref(),
            Some("9e8d7c6")
        );
    }
}
