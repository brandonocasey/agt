mod all;
pub(super) mod merge;
mod records;
mod trust;

use serde_json::{Value, json};

use self::merge::{claim_git_names, merge};
use super::prune::prune_dir;
use super::scope::Adoption;
use super::{Ctx, Scope};
use crate::core::clock;
use crate::core::config::{Config, Kind};
use crate::core::error::{Error, Result, fail};
use crate::forge::select::{Selected, at_location, require_tracker, select_tracker};
use crate::forge::service::{self, ReviewService, ReviewSubject};
use crate::forge::{self, Fetched};
use crate::forge::{anchors, hosts};
use crate::git::Repo;
use crate::store::model::{Anchor, Forge, Review, ReviewState, Side, Target, TargetKind};
use crate::store::sidecars::Account;

#[derive(Default)]
pub(super) struct Summary {
    pub(super) imported: usize,
    pub(super) updated: usize,
    pub(super) deleted: usize,
    pub(super) records: usize,
    pub(super) answered: usize,
    pub(super) drafts: usize,
}

/// The service, authenticated login and PR/MR a `sync` or `publish` works on.
pub(super) struct Connection {
    pub service: Box<dyn ReviewService>,
    pub host: String,
    pub login: String,
    pub repo: String,
    pub number: u64,
    pub key: String,
}

impl Connection {
    /// Authenticates against `host` and names the PR/MR `number` of `repo`.
    fn open(forge: Forge, host: &str, repo: &str, number: u64) -> Result<Self> {
        let (service, login) = authenticate(forge, host)?;
        Ok(Self::with(service, login, host, repo, number))
    }

    fn with(
        service: Box<dyn ReviewService>,
        login: String,
        host: &str,
        repo: &str,
        number: u64,
    ) -> Self {
        Self {
            key: pr_key(service.forge(), number),
            service,
            host: host.to_owned(),
            login,
            repo: repo.to_owned(),
            number,
        }
    }

    pub fn forge(&self) -> Forge {
        self.service.forge()
    }

    pub fn subject(&self) -> ReviewSubject<'_> {
        ReviewSubject {
            repo: &self.repo,
            number: self.number,
        }
    }
}

/// The PR/MR a command works on, and the tracker and modes that govern it, found without calling
/// the forge.
pub(super) struct Chosen {
    pub selected: Selected,
    place: Place,
}

enum Place {
    /// The PR/MR a `--target` was synced with.
    Stored(Box<Review>),
    /// A PR/MR number, or the PR/MR of the current branch when `None`.
    Number(Option<u64>),
}

/// Picks the tracker for `pr` or `remote`, see [`select_tracker`]. A target already synced with a
/// PR/MR keeps it, so a detached HEAD needs no `--pr`.
pub(super) fn choose(
    ctx: &Ctx,
    config: &Config,
    pr: Option<&str>,
    remote: Option<&str>,
) -> Result<Chosen> {
    if pr.is_none()
        && remote.is_none()
        && let Some(key) = &ctx.selection.target
        && let Some(target) = ctx.store.load(&ctx.dir, key)?
    {
        // The PR/MR a `--target` was synced with, so a detached HEAD needs no `--pr`.
        if let Some(review) = target.review {
            let selected = at_location(
                &ctx.repo,
                config,
                Kind::PullRequests,
                &review.host,
                &review.repo,
            )?;
            return Ok(Chosen {
                selected,
                place: Place::Stored(Box::new(review)),
            });
        }
        // Without one, the current branch's PR/MR would be imported into another branch's
        // records.
        if target.branch.is_none() || target.branch != ctx.repo.current_branch()? {
            return fail(format!(
                "target {key} has no PR/MR and is not the current branch's; pass --pr"
            ));
        }
    }
    let pr_ref = pr.map(forge::parse_pr).transpose()?;
    let selected = require_tracker(
        &ctx.repo,
        config,
        Kind::PullRequests,
        remote,
        pr_ref.as_ref(),
    )?;
    Ok(Chosen {
        selected,
        place: Place::Number(pr_ref.map(|pr| pr.number)),
    })
}

/// Connects to the PR/MR of `chosen`.
pub(super) fn connect_chosen(ctx: &Ctx, chosen: Chosen) -> Result<Connection> {
    let Chosen { selected, place } = chosen;
    match place {
        Place::Stored(review) => connection_of(&review),
        Place::Number(number) => {
            let forge = match &selected.tracker {
                Some(tracker) => tracker.forge,
                None => detect(ctx, &selected.host)?,
            };
            let (service, login) = authenticate(forge, &selected.host)?;
            let (repo, number) = match number {
                Some(number) => (selected.repo.clone(), number),
                None => {
                    let branch = ctx
                        .repo
                        .current_branch()?
                        .ok_or_else(|| Error("HEAD is detached; pass --pr <number|url>".into()))?;
                    service
                        .find_review(&selected.repo, &branch)?
                        .ok_or_else(|| {
                            Error(format!(
                                "no PR/MR found for branch {branch} in {}",
                                selected.repo
                            ))
                        })?
                }
            };
            Ok(Connection::with(
                service,
                login,
                &selected.host,
                &repo,
                number,
            ))
        }
    }
}

pub(super) fn connect(
    ctx: &Ctx,
    config: &Config,
    pr: Option<&str>,
    remote: Option<&str>,
) -> Result<Connection> {
    connect_chosen(ctx, choose(ctx, config, pr, remote)?)
}

/// The store key of the PR/MR `review` is.
pub(super) fn key_of(review: &Review) -> String {
    pr_key(review.forge, review.number)
}

/// The connection to the PR/MR a target was synced with.
pub(super) fn connection_of(review: &Review) -> Result<Connection> {
    Connection::open(review.forge, &review.host, &review.repo, review.number)
}

/// The forge, host and repository of the tracker `agt` picks without being told, whether it
/// pulls on its own, or `None` when there is none or its host is not a forge this build knows.
/// Neither is an error to a caller that only wants to try.
pub(super) fn locate(ctx: &Ctx, config: &Config) -> Result<Option<(Forge, Selected)>> {
    let Some(selected) = select_tracker(&ctx.repo, config, Kind::PullRequests, None, None)? else {
        return Ok(None);
    };
    let forge = match &selected.tracker {
        Some(tracker) => Some(tracker.forge),
        None => known_forge(ctx, &selected.host)?,
    };
    Ok(forge.map(|forge| (forge, selected)))
}

/// The connection to the PR/MR of `branch` in `repo` on `host`, or `None` when it has none.
pub(super) fn find_for_branch(
    forge: Forge,
    host: &str,
    repo: &str,
    branch: &str,
) -> Result<Option<Connection>> {
    let (service, login) = authenticate(forge, host)?;
    let Some((repo, number)) = service.find_review(repo, branch)? else {
        return Ok(None);
    };
    Ok(Some(Connection::with(service, login, host, &repo, number)))
}

/// The forge of `host` by [`service::detect`], with the hosts file and the forge this repository
/// last synced the host with as its first hints.
fn known_forge(ctx: &Ctx, host: &str) -> Result<Option<Forge>> {
    let cached = ctx
        .store
        .load_account(&ctx.dir)?
        .filter(|account| account.host == host)
        .map(|account| account.forge);
    Ok(service::detect(host, hosts::named(host)?, cached))
}

/// The forge of `host`, never guessed; see [`known_forge`].
fn detect(ctx: &Ctx, host: &str) -> Result<Forge> {
    known_forge(ctx, host)?.ok_or_else(|| {
        Error(format!(
            "unsupported forge {host}: sync and publish need github.com or a host `gh auth \
             status` knows, or gitlab.com, GITLAB_HOST, glab's configured host, a host \
             `glab auth status` knows, or an entry for it in $XDG_CONFIG_HOME/agt/hosts.json"
        ))
    })
}

fn authenticate(forge: Forge, host: &str) -> Result<(Box<dyn ReviewService>, String)> {
    let service = (service::kind(forge).connect)(host);
    let login = service.login()?;
    Ok((service, login))
}

fn pr_key(forge: Forge, number: u64) -> String {
    format!("{}-{number}", service::kind(forge).key_prefix)
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
    let mut fetched = connection.service.fetch(connection.subject())?;
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
        forge: connection.forge(),
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
        created_at: fetched.pr.created_at.clone(),
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
            connection.service.as_ref(),
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

/// `agt review sync`: see [`all::run`].
pub(super) fn sync_command(ctx: &Ctx, pr: Option<&str>, remote: Option<&str>) -> Result<Value> {
    all::run(ctx, pr, remote)
}

pub(super) fn sync(ctx: &Ctx, pr: Option<&str>, remote: Option<&str>) -> Result<Value> {
    let config = Config::load_for_repo(&ctx.repo)?;
    let connection = connect(ctx, &config, pr, remote)?;
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
        default_branch: plan.scope.default_branch.clone(),
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
    crate::forge::auto::clear_failure(&ctx.store, &ctx.dir, &format!("pr-{key}"));
    if let Some(branch) = &scope_for_init.branch {
        crate::forge::auto::clear_failure(&ctx.store, &ctx.dir, &format!("branch-{branch}"));
    }
    ctx.store.save_account(
        &ctx.dir,
        &Account {
            forge: connection.forge(),
            host: connection.host.clone(),
            login: connection.login.clone(),
        },
    )?;
    let pruned = prune_dir(&ctx.store, &ctx.dir, clock::now_secs()?)?;
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
            default_branch: existing
                .as_ref()
                .and_then(|target| target.default_branch.clone()),
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
