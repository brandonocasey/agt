use std::collections::HashSet;

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
    RemoteKind, Review, ReviewState, Severity, Side, Target, TargetKind, Thread, ThreadType,
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
    if pr.is_none()
        && let Some(key) = &ctx.selection.target
        && let Some(review) = ctx
            .store
            .load(&ctx.dir, key)?
            .and_then(|target| target.review)
    {
        // The PR/MR a `--target` was synced with, so a detached HEAD needs no `--pr`.
        let (client, login) = authenticate(review.forge, &review.host)?;
        return Ok(Connection {
            key: pr_key(review.forge, review.number),
            forge: review.forge,
            host: review.host,
            client,
            login,
            repo: review.repo,
            number: review.number,
        });
    }
    let pr_ref = pr.map(forge::parse_pr).transpose()?;
    let origin = ctx.repo.origin_url()?.and_then(|url| split_remote(&url));
    let location = pr_ref
        .as_ref()
        .and_then(|pr| pr.location.clone())
        .or(origin);
    let Some((host, origin_repo)) = location else {
        return fail("sync needs a GitHub or GitLab origin remote, or --pr <url>");
    };
    let forge = detect(&host)?;
    let (client, login) = authenticate(forge, &host)?;
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
    Ok(Connection {
        key: pr_key(forge, number),
        forge,
        host,
        client,
        login,
        repo,
        number,
    })
}

/// The forge of `host`, from the host name or the gh/glab configuration and logins; never
/// guessed.
fn detect(host: &str) -> Result<Forge> {
    if host == "github.com" {
        return Ok(Forge::Github);
    }
    let named = |value: &str| bare_host(value) == host;
    if host == "gitlab.com"
        || std::env::var("GITLAB_HOST").is_ok_and(|value| named(&value))
        || Client::glab_configured()
            .text(&["config", "get", "host"])
            .is_some_and(|value| named(&value))
    {
        return Ok(Forge::Gitlab);
    }
    let known = |client: Client| {
        client
            .text(&["auth", "status", "--hostname", host])
            .is_some()
    };
    if known(Client::gh(host)) {
        return Ok(Forge::Github);
    }
    // `glab config get host` names only the global default, not every host glab is logged in to.
    if known(Client::glab(host)) {
        return Ok(Forge::Gitlab);
    }
    fail(format!(
        "unsupported forge {host}: sync and publish need github.com or a host `gh auth status` \
         knows, or gitlab.com, GITLAB_HOST, glab's configured host or a host `glab auth status` \
         knows"
    ))
}

/// The lowercase host of a `GITLAB_HOST`-style value, which may be a URL.
fn bare_host(value: &str) -> String {
    let value = value.split_once("://").map_or(value, |(_, rest)| rest);
    value
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn authenticate(forge: Forge, host: &str) -> Result<(Client, String)> {
    let client = match forge {
        Forge::Github => Client::gh(host),
        Forge::Gitlab => Client::glab(host),
    };
    let login = match forge {
        Forge::Github => github::login(&client)?,
        Forge::Gitlab => gitlab::login(&client)?,
    };
    Ok((client, login))
}

fn pr_key(forge: Forge, number: u64) -> String {
    match forge {
        Forge::Github => format!("gh-{number}"),
        Forge::Gitlab => format!("gl-{number}"),
    }
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
        attached_at: Some(now.clone()),
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
    fn apply(mut self, target: &mut Target, connection: &Connection) -> (Summary, Vec<String>) {
        if let Some(old) = &target.review
            && old.forge == self.review.forge
            && old.number == self.review.number
        {
            self.review.attached_at.clone_from(&old.attached_at);
        }
        target.review = Some(self.review);
        if target.branch.is_none() {
            target.branch = Some(self.fetched.pr.head_branch.clone());
        }
        if target.branch_birth.is_none() {
            target.branch_birth.clone_from(&self.scope.birth);
        }
        claim_git_names(target, &connection.login);
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
    let now = plan.now.clone();
    let scope_for_init = Scope {
        key: key.clone(),
        kind: plan.scope.kind,
        branch: plan.scope.branch.clone(),
        birth: plan.scope.birth.clone(),
        base: plan.scope.base.clone(),
        head: plan.scope.head.clone(),
        head_known: true,
        existing: None,
    };
    let (summary, recorded, total, review) = ctx.store.update_renamed(
        &ctx.dir,
        &from,
        &key,
        &now,
        || ctx.new_target(&scope_for_init, &now),
        |target| {
            let (summary, recorded) = plan.apply(target, connection);
            Ok((
                summary,
                recorded,
                target.threads.len(),
                target.review.clone(),
            ))
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
    if ctx.selection.target.is_some() {
        return Ok((ctx.scope()?, true));
    }
    let own_branch = branch == Some(fetched.pr.head_branch.as_str());
    if own_branch && ctx.selection.range.is_none() {
        let scope = ctx.scope()?;
        if belongs(ctx, fetched, &scope.head)? {
            return Ok((scope, true));
        }
    }
    let local_head = ctx
        .repo
        .commit(&format!("refs/heads/{}", fetched.pr.head_branch))
        .ok();
    let branch_target = match (&ctx.selection.range, local_head) {
        (None, Some(head)) if belongs(ctx, fetched, &head)? => branch_target(ctx, key, fetched)?,
        (None, None) if fetched.pr.state == ReviewState::Open => branch_target(ctx, key, fetched)?,
        _ => None,
    };
    let (from, existing) = match branch_target {
        Some(target) => (target.key.clone(), Some(target)),
        None => (key.to_owned(), ctx.store.load(&ctx.dir, key)?),
    };
    Ok((
        Scope {
            key: from,
            kind: TargetKind::Branch,
            branch: Some(fetched.pr.head_branch.clone()),
            birth: existing
                .as_ref()
                .and_then(|target| target.branch_birth.clone()),
            base: fetched.pr.base_sha.clone(),
            head: fetched.pr.head_sha.clone(),
            head_known: true,
            existing,
        },
        false,
    ))
}

/// An open PR/MR belongs to its branch. A closed one only when its head is the branch head or an
/// ancestor of it, since a branch name is often reused after its PR/MR closed.
fn belongs(ctx: &Ctx, fetched: &Fetched, branch_head: &str) -> Result<bool> {
    let head = &fetched.pr.head_sha;
    if fetched.pr.state == ReviewState::Open || head == branch_head {
        return Ok(true);
    }
    Ok(ctx.repo.object_exists(&format!("{head}^{{commit}}"))?
        && ctx.repo.is_ancestor(head, branch_head)?)
}

/// The local branch target of the PR/MR's head branch, so a sync away from that branch (such
/// as on a detached HEAD) still joins branch and PR/MR reviews in one target. A target of an
/// older branch with the same name is skipped: one of its passes MUST be an ancestor of the
/// PR/MR head, or it has none.
fn branch_target(ctx: &Ctx, key: &str, fetched: &Fetched) -> Result<Option<Target>> {
    let head = &fetched.pr.head_sha;
    let head_here = ctx.repo.object_exists(&format!("{head}^{{commit}}"))?;
    let mut candidates: Vec<Target> = ctx
        .store
        .targets(&ctx.dir)?
        .into_iter()
        .filter(|target| {
            target.key != key
                && target.kind == TargetKind::Branch
                && target.branch.as_deref() == Some(fetched.pr.head_branch.as_str())
        })
        .collect();
    candidates.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    for candidate in candidates {
        let mut heads = candidate
            .runs
            .iter()
            .flat_map(|run| &run.passes)
            .map(|pass| &pass.head)
            .peekable();
        if heads.peek().is_none() || !head_here {
            return Ok(Some(candidate));
        }
        for pass_head in heads {
            if ctx.repo.is_ancestor(pass_head, head)? {
                return Ok(Some(candidate));
            }
        }
    }
    Ok(None)
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

/// The attempt id in an `<!-- agent-review id=<id> attempt=<attempt> -->` marker.
fn agent_attempt(body: &str) -> Option<&str> {
    let inner = body.split_once(AGENT_MARKER)?.1.split_once("-->")?.0;
    inner
        .split_whitespace()
        .find_map(|pair| pair.strip_prefix("attempt="))
}

/// What sync may believe of fetched comments. GitLab notes show no editor, so there only
/// text this store sent is trusted.
struct Trust<'a> {
    login: &'a str,
    forge: Forge,
    sent: HashSet<String>,
}

impl<'a> Trust<'a> {
    fn new(target: &Target, login: &'a str, forge: Forge) -> Self {
        let comments = target
            .threads
            .iter()
            .flat_map(|thread| &thread.comments)
            .filter_map(|comment| comment.sent.as_deref());
        let records = target
            .runs
            .iter()
            .filter_map(|run| run.record.as_ref()?.sent.as_deref());
        Self {
            login,
            forge,
            sent: comments
                .chain(records)
                .map(|text| text.trim().to_owned())
                .collect(),
        }
    }

    fn sent_text(&self, local: Option<&str>, comment: &CommentData) -> bool {
        self.forge == Forge::Github
            || local.is_some_and(|local| local.trim() == comment.body.trim())
    }

    /// A trusted comment, on GitLab only with a body this store sent.
    fn verified(&self, comment: &CommentData) -> bool {
        trusted(comment, self.login)
            && (self.forge == Forge::Github || self.sent.contains(comment.body.trim()))
    }

    /// The agent-review id of a verified comment.
    fn own_id(&self, comment: &CommentData) -> Option<String> {
        self.verified(comment)
            .then(|| agent_id(&comment.body))
            .flatten()
    }

    /// Whether `incoming` is the landed write of local `comment`: one this store is posting,
    /// marked with its id and the same attempt. Another store's ids repeat, so a pending
    /// comment is never claimed.
    fn claims(&self, comment: &Comment, incoming: &CommentData) -> bool {
        comment.remote.is_none()
            && comment.post == PostState::Posting
            && trusted(incoming, self.login)
            && agent_id(&incoming.body).as_deref() == Some(comment.id.as_str())
            && comment.attempt.is_some()
            && agent_attempt(&incoming.body) == comment.attempt.as_deref()
            && self.sent_text(comment.sent.as_deref(), incoming)
    }
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

/// The reason of a marked `Rejected: <reason>.` reply.
fn rejected_reason(body: &str) -> Option<String> {
    agent_id(body)?;
    let reason = body
        .lines()
        .next()?
        .strip_prefix("Rejected: ")?
        .trim()
        .trim_end_matches('.');
    (!reason.is_empty()).then(|| reason.to_owned())
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
        sent: None,
        local: false,
        deleted_remote: false,
        deleted_seen_at: None,
        author: Author::Human {
            login: comment.login.clone().unwrap_or_else(|| "ghost".to_owned()),
            git_name: false,
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
    let trust = Trust::new(target, login, forge);
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
    // Records are read again from the comments the host still returns.
    target
        .records
        .retain(|record| !seen_comments.contains(&record.comment_id));
    for data in fetched.threads {
        if let [comment] = data.comments.as_slice()
            && trust.verified(comment)
        {
            let own = trust.own_id(comment);
            if data.info.kind == RemoteKind::Review
                && own.as_ref().is_some_and(|id| id.starts_with(REVIEW_ID))
            {
                continue;
            }
            if data.info.kind == RemoteKind::General {
                if take_record(target, comment, &trust) {
                    summary.records += 1;
                    continue;
                }
                if let Some(id) = answered_id(&comment.body, own.is_some()) {
                    answers.push((id, data));
                    continue;
                }
            }
        }
        import(target, data, &trust, now, &mut summary);
    }
    for (id, data) in answers {
        match target.threads.iter_mut().find(|thread| thread.id == id) {
            Some(thread) => {
                answer(thread, data, &trust, now);
                summary.answered += 1;
            }
            None => import(target, data, &trust, now, &mut summary),
        }
    }
    // A draft that is gone was published or deleted, and a write that left no marker on the
    // host did not land or was rejected: unless a note matched above or a draft below, it goes
    // back to pending, to post again or for `finding kill`. The attempt id stays.
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
        if comment.draft_id.is_none() && comment.post == PostState::Posting {
            comment.post = PostState::Pending;
        }
    }
    for draft in fetched.drafts {
        let attempt = agent_attempt(&draft.body);
        let comment = match (agent_id(&draft.body), marker_id(&draft.body)) {
            (Some(local), _) => target
                .threads
                .iter_mut()
                .flat_map(|thread| thread.comments.iter_mut())
                .find(|comment| {
                    comment.id == local
                        && comment.attempt.is_some()
                        && comment.attempt.as_deref() == attempt
                }),
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

/// Comments written before the first sync name git `user.name`; they are the authenticated
/// user's.
fn claim_git_names(target: &mut Target, login: &str) {
    for comment in target
        .threads
        .iter_mut()
        .flat_map(|thread| thread.comments.iter_mut())
    {
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

/// Stores a verified record comment and marks our run's record posted when it is that run's
/// write; false when the comment is no record.
fn take_record(target: &mut Target, comment: &CommentData, trust: &Trust) -> bool {
    let Some(record) = parse_record(comment) else {
        return false;
    };
    let own = trust.own_id(comment);
    if let Some(run_id) = own
        .as_deref()
        .and_then(|local| local.strip_suffix("/record"))
        && let Some(run) = target.runs.iter_mut().find(|run| run.id == run_id)
        && let Some(post) = &mut run.record
        && (post.remote_id.as_deref() == Some(comment.id.as_str())
            || (post.remote_id.is_none()
                && post.post == PostState::Posting
                && post.attempt.is_some()
                && agent_attempt(&comment.body) == post.attempt.as_deref()))
    {
        post.post = PostState::Posted;
        post.remote_id = Some(comment.id.clone());
        post.url.clone_from(&comment.url);
    }
    target
        .records
        .retain(|known| known.comment_id != record.comment_id);
    target.records.push(record);
    true
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

fn import(target: &mut Target, data: ThreadData, trust: &Trust, now: &str, summary: &mut Summary) {
    let (login, forge) = (trust.login, trust.forge);
    let marker = data
        .comments
        .first()
        .filter(|first| trust.verified(first))
        .and_then(|first| marker_id(&first.body));
    let by_remote = target.threads.iter().position(|thread| {
        thread.remote.as_ref().is_some_and(|remote| {
            // A GitLab note that gets a reply turns from general into a discussion with the
            // same id; GitHub ids of different kinds come from separate sequences.
            remote.id == data.info.id && (remote.kind == data.info.kind || forge == Forge::Gitlab)
        })
    });
    let by_agent = || {
        let incoming = data.comments.first()?;
        target.threads.iter().position(|thread| {
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
            trust,
            now,
        );
        summary.updated += 1;
        return;
    }
    let taken = |id: &str| {
        target.threads.iter().any(|thread| thread.id == id)
            || target
                .runs
                .iter()
                .flat_map(|run| &run.passes)
                .flat_map(|pass| &pass.killed)
                .any(|killed| killed.id == id)
    };
    let id = match marker {
        Some(marker) if !taken(&marker) => marker,
        _ => {
            let base = imported_id(forge, &data);
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
        approval: None,
    });
    summary.imported += 1;
}

/// Remote wins for posted text and resolved state; local fields and unposted comments stay.
fn update(thread: &mut Thread, data: ThreadData, by_marker: bool, trust: &Trust, now: &str) {
    let login = trust.login;
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
            let rejected = rejected_reason(&comment.body);
            thread.resolved = fixed.is_some() || rejected.is_some();
            if thread.resolved {
                thread.held = false;
            }
            if thread.resolution.is_none() {
                thread.resolution = match (fixed, rejected) {
                    (Some(sha), _) => Some(format!("fixed {sha}")),
                    (None, Some(reason)) => Some(format!("rejected: {reason}")),
                    (None, None) => None,
                };
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
