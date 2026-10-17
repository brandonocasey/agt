use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::anchors::{self, FileDiff, Location};
use crate::cli::{
    ApproveArgs, AuthorArgs, Cli, Command, CommentArgs, FindingAction, Format, ListArgs,
    PassAction, ReplyArgs, ResolveArgs, RunAction,
};
use crate::clock;
use crate::error::{Error, Result, ctx, fail};
use crate::git::{CommitInfo, LogEntry, Repo};
use crate::model::{
    Anchor, Approval, Author, Comment, CommitPatch, FindingInfo, ImportedRecord, KilledFinding,
    Pass, PassKind, PostState, Reveal, ReviewState, Reviewer, RoleUsage, Run, Severity, Side,
    Target, TargetKind, Thread, ThreadType, Triage, Usage, VERSION,
};
use crate::store::Store;

mod publish;
mod sync;

const PRUNE_AFTER_SECS: i64 = 90 * 24 * 60 * 60;
const MAX_TEXT: usize = 65_536;

pub enum Output {
    Json(Value),
    Text(String),
}

#[derive(Clone)]
struct Selection {
    target: Option<String>,
    range: Option<String>,
    default_branch: Option<String>,
}

struct Ctx {
    repo: Repo,
    read_only: bool,
    store: Store,
    repo_key: String,
    dir: PathBuf,
    selection: Selection,
}

/// The target a command works on, with the base and head it is evaluated at.
struct Scope {
    key: String,
    kind: TargetKind,
    branch: Option<String>,
    birth: Option<String>,
    base: Option<String>,
    head: String,
    /// False only for a reading command on a branch target with no branch, PR/MR head or
    /// pass; `head` is then empty.
    head_known: bool,
    existing: Option<Target>,
}

pub fn run(cli: Cli) -> Result<Output> {
    let store = Store::open()?;
    let selection = Selection {
        target: cli.target,
        range: cli.range,
        default_branch: cli.default_branch,
    };
    let read_only = matches!(
        cli.command,
        Command::Status { .. }
            | Command::List(_)
            | Command::Show { .. }
            | Command::Publish { dry_run: true, .. }
    );
    if let Command::Prune { all: true, dry_run } = cli.command {
        return prune_all(&store, dry_run).map(Output::Json);
    }
    // Publish writes no git objects and needs `merge-tree` for its records' coverage.
    let scratch_git = read_only || matches!(cli.command, Command::Publish { .. });
    let mut ctx = Ctx::new(store, selection, read_only, scratch_git)?;
    if let Some(run) = run_hint(&cli.command) {
        ctx.select_run_target(&run)?;
    }
    let status_selection = ctx.selection.clone();
    let value = match cli.command {
        Command::Run {
            action:
                RunAction::Start {
                    kind,
                    base,
                    reviewers,
                    head,
                },
        } => {
            let started = run_start(&ctx)?;
            match kind {
                Some(kind) => {
                    let run = started["run"].as_str().unwrap_or_default().to_owned();
                    let pass = pass_start(
                        &ctx,
                        &run,
                        kind,
                        base.as_deref(),
                        reviewers.as_deref(),
                        head.as_deref(),
                    );
                    // A first pass that fails leaves no empty run behind.
                    if pass.is_err() {
                        let scope = ctx.scope()?;
                        ctx.update(&scope, |target, _| {
                            target
                                .runs
                                .retain(|known| known.id != run || !known.passes.is_empty());
                            Ok(())
                        })?;
                    }
                    pass?
                }
                None => started,
            }
        }
        Command::Run {
            action:
                RunAction::End {
                    run,
                    reveal_agree,
                    reveal_disagree,
                    head,
                    usage,
                },
        } => {
            let reveal = reveal_agree
                .zip(reveal_disagree)
                .map(|(agree, disagree)| Reveal { agree, disagree });
            let usage = match usage {
                Some(source) => {
                    let inputs: Vec<UsageInput> =
                        serde_json::from_str(&read_source(&source, "usage")?)
                            .map_err(crate::error::ctx("parse usage (expected a JSON array)"))?;
                    inputs
                        .iter()
                        .map(|input| {
                            Ok(RoleUsage {
                                role: input.who("role")?,
                                usage: input.usage(),
                            })
                        })
                        .collect::<Result<_>>()?
                }
                None => Vec::new(),
            };
            let mut ended = run_end(&ctx, &run, reveal, head.as_deref(), usage)?;
            // The status the caller would check next, from a read-only repository so
            // merge checks write no objects.
            let reader = Ctx::new(Store::open()?, status_selection, true, true)?;
            ended["status"] = brief_status(status(&reader, head.as_deref())?);
            ended
        }
        Command::Pass {
            action:
                PassAction::Start {
                    run,
                    kind,
                    base,
                    reviewers,
                    head,
                },
        } => pass_start(
            &ctx,
            &run,
            kind,
            base.as_deref(),
            reviewers.as_deref(),
            head.as_deref(),
        )?,
        Command::Pass {
            action:
                PassAction::End {
                    run,
                    pass,
                    lock,
                    findings,
                    reviewer,
                },
        } => pass_end(
            &ctx,
            &run,
            &pass,
            lock,
            findings.as_deref(),
            reviewer.as_deref(),
        )?,
        Command::Finding {
            action:
                FindingAction::Add {
                    run,
                    pass,
                    reviewer,
                },
        } => finding_add(&ctx, &run, &pass, &reviewer)?,
        Command::Finding {
            action: FindingAction::Kill { id, reason },
        } => finding_kill(&ctx, &id, &reason)?,
        Command::Comment(args) => comment(&ctx, &args)?,
        Command::Reply(args) => reply(&ctx, &args)?,
        Command::Resolve(args) => resolve(&ctx, &args)?,
        Command::Approve(args) => approve(&ctx, &args)?,
        Command::List(args) => return list(&ctx, &args),
        Command::Show { id } => show(&ctx, &id)?,
        Command::Status { head, full } => {
            let status = status(&ctx, head.as_deref())?;
            if full { status } else { brief_status(status) }
        }
        Command::Sync { pr } => sync::sync(&ctx, pr.as_deref())?,
        Command::Publish {
            dry_run,
            not_own,
            findings,
            only,
            pr,
        } => publish::publish(
            &ctx,
            pr.as_deref(),
            &publish::Options {
                dry_run,
                not_own,
                findings,
                only,
            },
        )?,
        Command::Prune { dry_run, .. } => {
            let pruned = prune_dir(&ctx.store, &ctx.dir, clock::now_secs()?, dry_run)?;
            json!({ "dry_run": dry_run, "pruned": pruned })
        }
    };
    Ok(Output::Json(value))
}

/// The run a command names, through `--run` or an id that starts with one (`rv-xxxx...`).
fn run_hint(command: &Command) -> Option<String> {
    let from_id = |id: &str| {
        let run = id.split('.').next()?;
        (run.len() == 7 && run.starts_with("rv-")).then(|| run.to_owned())
    };
    match command {
        Command::Run {
            action: RunAction::End { run, .. },
        }
        | Command::Pass {
            action: PassAction::Start { run, .. } | PassAction::End { run, .. },
        }
        | Command::Finding {
            action: FindingAction::Add { run, .. },
        } => Some(run.clone()),
        Command::Finding {
            action: FindingAction::Kill { id, .. },
        }
        | Command::Show { id } => from_id(id),
        Command::Reply(args) => args.author.run.clone().or_else(|| from_id(&args.id)),
        Command::Resolve(args) => args
            .author
            .run
            .clone()
            .or_else(|| args.ids.first().and_then(|id| from_id(id))),
        Command::List(args) => args.run.clone(),
        _ => None,
    }
}

impl Ctx {
    /// With no `--target` or `--range`, a command naming a run works on the target that holds
    /// it, so callers need not repeat a range or key. The current branch's own target wins.
    fn select_run_target(&mut self, run: &str) -> Result<()> {
        if self.selection.target.is_some() || self.selection.range.is_some() {
            return Ok(());
        }
        let has_run = |target: &Target| target.runs.iter().any(|known| known.id == run);
        let scope = self.scope();
        if let Ok(scope) = &scope
            && scope.existing.as_ref().is_some_and(has_run)
        {
            return Ok(());
        }
        // Only a range, commit or PR/MR target: another branch's target would evaluate that
        // branch's tip, not the checked-out fix.
        let holders: Vec<String> = self
            .store
            .targets(&self.dir)?
            .into_iter()
            .filter(|target| has_run(target))
            .filter(|target| {
                target.kind != TargetKind::Branch
                    || target.key.starts_with("gh-")
                    || target.key.starts_with("gl-")
            })
            .map(|target| target.key)
            .collect();
        match (holders.as_slice(), scope) {
            ([key], _) => self.selection.target = Some(key.clone()),
            (_, Ok(_)) => {}
            ([], Err(why)) => {
                return fail(format!(
                    "run {run} is in no range or PR/MR target here, and {why}"
                ));
            }
            (keys, Err(_)) => {
                return fail(format!(
                    "run {run} is in several targets ({}); pass --target <key>",
                    keys.join(", ")
                ));
            }
        }
        Ok(())
    }

    fn new(store: Store, selection: Selection, read_only: bool, scratch_git: bool) -> Result<Self> {
        let mut repo = Repo::discover()?;
        if scratch_git {
            repo = repo.read_only()?;
        }
        let repo_key = repo.repo_key()?;
        let dir = store.repo_dir(&repo_key)?;
        Ok(Self {
            repo,
            read_only,
            store,
            repo_key,
            dir,
            selection,
        })
    }

    /// Author default: the forge login cached by `sync`, else git `user.name`, which the next
    /// sync replaces with the forge login.
    fn default_author(&self) -> Result<Option<Author>> {
        if let Some(account) = self.store.load_account(&self.dir)? {
            return Ok(Some(Author::Human {
                login: account.login,
                git_name: false,
            }));
        }
        Ok(self.repo.user_name()?.map(|login| Author::Human {
            login,
            git_name: true,
        }))
    }

    fn scope(&self) -> Result<Scope> {
        if let Some(key) = &self.selection.target {
            return self.scope_for_key(key);
        }
        if let Some(range) = &self.selection.range {
            return self.scope_for_range(range);
        }
        self.scope_for_branch()
    }

    fn scope_for_key(&self, key: &str) -> Result<Scope> {
        let target = self
            .store
            .load(&self.dir, key)?
            .ok_or_else(|| Error(format!("target {key} does not exist")))?;
        let mut headless = false;
        let (base, head) = match target.kind {
            TargetKind::Branch => {
                let branch = target.branch.clone().unwrap_or_default();
                let found = match self.branch_head(&target, &branch)? {
                    Ok(head) => Some(head),
                    // Reading commands still work: anchors map to the last reviewed head, or
                    // nowhere when nothing was reviewed.
                    Err(_) if self.read_only => target
                        .runs
                        .iter()
                        .flat_map(|run| &run.passes)
                        .max_by(|a, b| a.started_at.cmp(&b.started_at))
                        .map(|pass| pass.head.clone()),
                    Err(why) => {
                        return fail(format!(
                            "branch {branch} of target {key} is not here and {why}"
                        ));
                    }
                };
                match found {
                    Some(head) => (Some(self.branch_base(&branch, &head)?.0), head),
                    None => {
                        headless = true;
                        (target.base.clone(), String::new())
                    }
                }
            }
            TargetKind::Range | TargetKind::Commit => {
                let head = target
                    .head
                    .clone()
                    .ok_or_else(|| Error(format!("target {key} has no head")))?;
                (target.base.clone(), head)
            }
        };
        Ok(Scope {
            key: key.to_owned(),
            kind: target.kind,
            branch: target.branch.clone(),
            birth: target.branch_birth.clone(),
            base,
            head,
            head_known: !headless,
            existing: Some(target),
        })
    }

    fn scope_for_range(&self, range: &str) -> Result<Scope> {
        let (kind, key, base, head) = match range.split_once("..") {
            Some((_, to)) if to.starts_with('.') => {
                return fail("symmetric ranges (a...b) are not supported");
            }
            Some((from, to)) => {
                let from = self.repo.commit(from)?;
                let to = self.repo.commit(to)?;
                (
                    TargetKind::Range,
                    format!("range-{from}-{to}"),
                    Some(from),
                    to,
                )
            }
            None => {
                let commit = self.repo.commit(range)?;
                let parent = self.repo.parent(&commit)?;
                (
                    TargetKind::Commit,
                    format!("commit-{commit}"),
                    parent,
                    commit,
                )
            }
        };
        let existing = self.store.load(&self.dir, &key)?;
        Ok(Scope {
            key,
            kind,
            branch: None,
            birth: None,
            base,
            head,
            head_known: true,
            existing,
        })
    }

    /// Finds the branch's target: first one with the same reflog birth, then the exact key, then
    /// an older target this branch continues; otherwise a new target.
    fn scope_for_branch(&self) -> Result<Scope> {
        let branch = self.repo.current_branch()?.ok_or_else(|| {
            Error("HEAD is detached; pass --range <rev> or --target <key>".into())
        })?;
        let head = self.repo.head()?;
        let (base, key_base) = self.branch_base(&branch, &head)?;
        let birth = self.repo.branch_birth(&branch)?;
        let mut key = format!("branch-{}-{key_base}", self.branch_key(&branch)?);
        if let Some((time, sha)) = birth.as_ref().and_then(|birth| birth.split_once(' ')) {
            let short = sha.get(..12).unwrap_or(sha);
            let _ = write!(key, "-{time}-{short}");
        }
        let mut candidates: Vec<Target> = self
            .store
            .targets(&self.dir)?
            .into_iter()
            .filter(|target| {
                target.kind == TargetKind::Branch && target.branch.as_deref() == Some(&branch)
            })
            .collect();
        candidates.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let same_birth = birth.as_ref().and_then(|birth| {
            candidates
                .iter()
                .position(|target| target.branch_birth.as_ref() == Some(birth))
        });
        let existing = match same_birth {
            Some(index) => Some(candidates.swap_remove(index)),
            None => match candidates.iter().position(|target| target.key == key) {
                Some(index) => Some(candidates.swap_remove(index)),
                None => self.adopt(candidates, &base, &head)?,
            },
        };
        Ok(Scope {
            key: existing.as_ref().map_or(key, |target| target.key.clone()),
            kind: TargetKind::Branch,
            branch: Some(branch),
            birth,
            base: Some(base),
            head,
            head_known: true,
            existing,
        })
    }

    /// The encoded branch name, shortened with a hash of the full name so the target key stays
    /// under the store's length limit.
    fn branch_key(&self, branch: &str) -> Result<String> {
        const KEEP: usize = 87;
        let encoded = encode_key(branch);
        if encoded.len() <= KEEP + 13 {
            return Ok(encoded);
        }
        let mut cut = KEEP;
        // MUST NOT split a `%XX` escape.
        while encoded[..cut].ends_with('%') || encoded[..cut - 1].ends_with('%') {
            cut -= 1;
        }
        let hash = self.repo.hash_text(branch)?;
        Ok(format!(
            "{}-{}",
            &encoded[..cut],
            hash.get(..12).unwrap_or(&hash)
        ))
    }

    /// The head of a branch target: `HEAD` when on its branch, or when detached on top of the
    /// local branch or the synced PR/MR head (a review worktree with fix commits, also after a
    /// force-push); else the local branch, else the PR/MR head. An unrelated `HEAD` MUST NOT
    /// stand in. The error says why there is none.
    fn branch_head(&self, target: &Target, branch: &str) -> Result<Result<String, String>> {
        let current = self.repo.current_branch()?;
        let head = self.repo.head()?;
        if current.as_deref() == Some(branch) {
            return Ok(Ok(head));
        }
        let local = self.repo.commit(&format!("refs/heads/{branch}")).ok();
        let pr_head = target.review.as_ref().map(|review| &review.head_sha);
        let pr_local = match pr_head {
            Some(sha) if self.repo.object_exists(&format!("{sha}^{{commit}}"))? => Some(sha),
            _ => None,
        };
        if current.is_none() {
            for known in local.iter().chain(pr_local) {
                if self.repo.is_ancestor(known, &head)? {
                    return Ok(Ok(head));
                }
            }
        }
        Ok(match (local, pr_local, pr_head) {
            (Some(local), _, _) => Ok(local),
            (None, Some(sha), _) => Ok(sha.clone()),
            (None, None, Some(sha)) => Err(format!(
                "its PR/MR head {sha} is not in this repository; fetch it or check out the branch"
            )),
            (None, None, None) => Err("no PR/MR head is known; check out the branch".to_owned()),
        })
    }

    /// Returns the review base and the commit that names the target. On the default branch
    /// itself the merge-base is the head, so the root commit names the target instead.
    /// The scope at `--head`: a range or commit target's later head, such as its fix, which
    /// MUST descend from the target's end.
    fn scope_at(&self, rev: Option<&str>) -> Result<Scope> {
        let mut scope = self.scope()?;
        let Some(rev) = rev else {
            return Ok(scope);
        };
        if scope.kind == TargetKind::Branch {
            return fail("--head is for range and commit targets; a branch target uses its head");
        }
        let head = self.repo.commit(rev)?;
        if !self.repo.is_ancestor(&scope.head, &head)? {
            return fail(format!(
                "--head {rev} does not descend from the end {} of target {}",
                scope.head, scope.key
            ));
        }
        scope.head = head;
        Ok(scope)
    }

    fn is_default_branch(&self, scope: &Scope) -> Result<bool> {
        let Some(branch) = scope
            .branch
            .as_deref()
            .filter(|_| scope.kind == TargetKind::Branch)
        else {
            return Ok(false);
        };
        let default = self
            .repo
            .default_branch(self.selection.default_branch.as_deref())?;
        Ok(default == format!("refs/heads/{branch}"))
    }

    fn branch_base(&self, branch: &str, head: &str) -> Result<(String, String)> {
        let default = self
            .repo
            .default_branch(self.selection.default_branch.as_deref())?;
        if default == format!("refs/heads/{branch}") {
            return Ok((head.to_owned(), self.repo.root_commit(head)?));
        }
        let base = self
            .repo
            .merge_base(head, &default)?
            .ok_or_else(|| Error(format!("{branch} has no merge-base with {default}")))?;
        Ok((base.clone(), base))
    }

    /// Finds an older target of the same branch that this branch continues after a rebase or a
    /// merge from the default branch moved its merge-base. A reused branch name shares no
    /// reviewed commits or patch-ids with the old target, so it starts fresh.
    fn adopt(&self, candidates: Vec<Target>, base: &str, head: &str) -> Result<Option<Target>> {
        if candidates.is_empty() {
            return Ok(None);
        }
        let current: HashSet<String> = self
            .repo
            .commits(Some(base), head)?
            .into_iter()
            .filter_map(|commit| commit.patch_id)
            .collect();
        for target in candidates {
            if self.continues(&target, base, head, &current)? {
                return Ok(Some(target));
            }
        }
        Ok(None)
    }

    fn continues(
        &self,
        target: &Target,
        base: &str,
        head: &str,
        current: &HashSet<String>,
    ) -> Result<bool> {
        for pass in passes(target) {
            let reachable = self.repo.is_ancestor(&pass.head, head)?
                && !self.repo.is_ancestor(&pass.head, base)?;
            if reachable || pass_patch_ids(pass).any(|id| current.contains(id)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn new_target(&self, scope: &Scope, now: &str) -> Target {
        Target {
            version: VERSION,
            revision: 0,
            key: scope.key.clone(),
            repo: self.repo_key.clone(),
            kind: scope.kind,
            branch: scope.branch.clone(),
            branch_birth: scope.birth.clone(),
            base: scope.base.clone(),
            head: (scope.kind != TargetKind::Branch).then(|| scope.head.clone()),
            created_at: now.to_owned(),
            updated_at: now.to_owned(),
            review: None,
            runs: Vec::new(),
            records: Vec::new(),
            threads: Vec::new(),
        }
    }

    fn update<R>(
        &self,
        scope: &Scope,
        change: impl FnOnce(&mut Target, &str) -> Result<R>,
    ) -> Result<R> {
        let now = clock::now()?;
        self.store.update(
            &self.dir,
            &scope.key,
            &now,
            || self.new_target(scope, &now),
            |target| change(target, &now),
        )
    }

    /// The reviewed state: the work tree for a branch whose head is checked out (possibly
    /// uncommitted), else the head commit, such as for `--target` while elsewhere.
    fn reviewed_state(&self, scope: &Scope) -> Result<(Option<String>, String)> {
        let head_tree = self.repo.tree_of(&scope.head)?;
        if scope.kind != TargetKind::Branch || self.repo.head()? != scope.head {
            return Ok((Some(scope.head.clone()), head_tree));
        }
        let tree = self.repo.worktree_tree()?;
        if tree == head_tree {
            Ok((Some(scope.head.clone()), tree))
        } else {
            Ok((None, tree))
        }
    }
}

fn encode_key(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn passes(target: &Target) -> impl Iterator<Item = &Pass> {
    target.runs.iter().flat_map(|run| run.passes.iter())
}

fn pass_patch_ids(pass: &Pass) -> impl Iterator<Item = &String> {
    pass.commits
        .iter()
        .filter_map(|commit| commit.patch_id.as_ref())
        .chain(pass.range_patch_id.as_ref())
        .chain(pass.head_patch_id.as_ref())
}

/// Names the open runs, so a wrong id costs no second lookup.
fn run_missing(target: &Target, id: &str) -> Error {
    let open: Vec<&str> = target
        .runs
        .iter()
        .filter(|run| run.ended_at.is_none())
        .map(|run| run.id.as_str())
        .collect();
    Error(format!(
        "run {id} not found in target {}; open runs: {}",
        target.key,
        if open.is_empty() {
            "none".to_owned()
        } else {
            open.join(", ")
        }
    ))
}

/// Names the run's passes, so a wrong id costs no second lookup.
fn pass_missing(run: &Run, pass: &str) -> Error {
    let known: Vec<&str> = run.passes.iter().map(|pass| pass.id.as_str()).collect();
    Error(format!(
        "pass {}.{pass} not found; its passes: {}",
        run.id,
        if known.is_empty() {
            "none".to_owned()
        } else {
            known.join(", ")
        }
    ))
}

fn find_run_mut<'a>(target: &'a mut Target, id: &str) -> Result<&'a mut Run> {
    match target.runs.iter().position(|run| run.id == id) {
        Some(index) => Ok(&mut target.runs[index]),
        None => Err(run_missing(target, id)),
    }
}

fn find_pass<'a>(target: &'a Target, run: &str, pass: &str) -> Result<&'a Pass> {
    let run = target
        .runs
        .iter()
        .find(|candidate| candidate.id == run)
        .ok_or_else(|| run_missing(target, run))?;
    run.passes
        .iter()
        .find(|candidate| candidate.id == pass)
        .ok_or_else(|| pass_missing(run, pass))
}

fn find_pass_mut<'a>(target: &'a mut Target, run: &str, pass: &str) -> Result<&'a mut Pass> {
    let run = find_run_mut(target, run)?;
    match run.passes.iter().position(|candidate| candidate.id == pass) {
        Some(index) => Ok(&mut run.passes[index]),
        None => Err(pass_missing(run, pass)),
    }
}

fn find_thread_mut<'a>(target: &'a mut Target, id: &str) -> Result<&'a mut Thread> {
    target
        .threads
        .iter_mut()
        .find(|thread| thread.id == id)
        .ok_or_else(|| Error(format!("thread {id} not found")))
}

fn is_major(kind: ThreadType, severity: Option<Severity>) -> bool {
    kind == ThreadType::Bug && matches!(severity, Some(Severity::High | Severity::Medium))
}

fn major_found(target: &Target, run: &str, pass: &str) -> bool {
    target.threads.iter().any(|thread| {
        thread
            .finding
            .as_ref()
            .is_some_and(|finding| finding.run == run && finding.pass == pass)
            && is_major(thread.kind, thread.severity)
    })
}

fn random_id() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    let mut bits = hasher.finish();
    let mut id = String::from("rv-");
    for _ in 0..4 {
        let index = usize::try_from(bits % 36).unwrap_or(0);
        id.push(char::from(ALPHABET[index]));
        bits /= 36;
    }
    id
}

fn limited(text: String, name: &str) -> Result<String> {
    if text.trim().is_empty() {
        return fail(format!("{name} must not be empty"));
    }
    if text.len() > MAX_TEXT {
        return fail(format!("{name} exceeds {MAX_TEXT} bytes"));
    }
    Ok(text)
}

fn read_stdin(what: &str) -> Result<String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(ctx(format!("read {what} from stdin")))?;
    Ok(text)
}

fn read_body(body: &str) -> Result<String> {
    if body == "-" {
        return limited(read_stdin("body")?, "body");
    }
    limited(body.to_owned(), "body")
}

fn validate_path(path: &str) -> Result<()> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..");
    if bad {
        return fail(format!(
            "path must be relative to the repository root: {path}"
        ));
    }
    Ok(())
}

struct AnchorInput<'a> {
    path: Option<&'a str>,
    line: Option<u32>,
    old_line: Option<u32>,
    side: Option<Side>,
    start_line: Option<u32>,
}

fn build_anchor(
    input: &AnchorInput,
    commit: Option<String>,
    tree: String,
) -> Result<Option<Anchor>> {
    let Some(path) = input.path else {
        if input.line.is_some()
            || input.old_line.is_some()
            || input.side.is_some()
            || input.start_line.is_some()
        {
            return fail("line, old_line, side and start_line need a path");
        }
        return Ok(None);
    };
    validate_path(path)?;
    if [input.line, input.old_line, input.start_line].contains(&Some(0)) {
        return fail("line numbers start at 1");
    }
    let side = input
        .side
        .unwrap_or(if input.line.is_none() && input.old_line.is_some() {
            Side::Old
        } else {
            Side::New
        });
    match side {
        Side::Old if input.old_line.is_none() => return fail("side old needs old_line"),
        Side::Old if input.start_line.is_some() => {
            return fail("start_line is for new-side ranges; side old takes one old_line");
        }
        Side::New if input.line.is_none() && input.start_line.is_some() => {
            return fail("start_line needs line");
        }
        _ => {}
    }
    if let (Some(start), Some(line)) = (input.start_line, input.line)
        && start > line
    {
        return fail("start_line must not be after line");
    }
    Ok(Some(Anchor {
        path: path.to_owned(),
        line: input.line,
        old_line: input.old_line,
        side,
        start_line: input.start_line,
        commit,
        tree,
    }))
}

/// A comment written here, numbered after the thread's other local comments.
fn local_comment(
    thread: &str,
    existing: &[Comment],
    author: Author,
    body: String,
    now: &str,
) -> Comment {
    let prefix = format!("{thread}/c");
    let number = next_number(existing.iter().map(|comment| comment.id.clone()), &prefix) + 1;
    Comment {
        id: format!("{prefix}{number}"),
        author,
        body,
        created_at: now.to_owned(),
        post: PostState::Pending,
        remote: None,
        draft_id: None,
        attempt: None,
        sent: None,
        local: false,
        deleted_remote: false,
        deleted_seen_at: None,
    }
}

struct NewThread {
    id: String,
    kind: ThreadType,
    severity: Option<Severity>,
    anchor: Option<Anchor>,
    finding: Option<FindingInfo>,
}

fn local_thread(new: NewThread, author: Author, body: String, now: &str) -> Thread {
    let first = local_comment(&new.id, &[], author, body, now);
    Thread {
        id: new.id,
        kind: new.kind,
        severity: new.severity,
        anchor: new.anchor,
        resolved: false,
        resolution: None,
        triage: None,
        held: false,
        finding: new.finding,
        comments: vec![first],
        remote: None,
        resolve_pending: None,
        deleted_remote: false,
        deleted_seen_at: None,
        created_at: now.to_owned(),
        updated_at: now.to_owned(),
        approval: None,
    }
}

fn author(args: &AuthorArgs, target: &Target, default: Option<Author>) -> Result<Author> {
    let agent = args.model.is_some()
        || args.effort.is_some()
        || args.harness.is_some()
        || args.harness_version.is_some()
        || args.run.is_some()
        || args.pass.is_some()
        || args.reviewer.is_some();
    if let Some(login) = &args.author {
        if agent {
            return fail("--author names a human; drop the agent flags");
        }
        return Ok(Author::Human {
            login: limited(login.clone(), "author")?,
            git_name: false,
        });
    }
    if !agent {
        return default
            .ok_or_else(|| Error("no author: pass --author or set git user.name".into()));
    }
    let stored = match (&args.run, &args.pass, &args.reviewer) {
        (Some(run), Some(pass), Some(id)) => find_pass(target, run, pass)?
            .reviewers
            .iter()
            .find(|reviewer| reviewer.id == *id),
        (_, _, Some(_)) => return fail("--reviewer needs --run and --pass"),
        _ => None,
    };
    let model = args
        .model
        .clone()
        .or_else(|| stored.map(|reviewer| reviewer.model.clone()))
        .ok_or_else(|| Error("an agent author needs --model, or a stored --reviewer".into()))?;
    let harness = args
        .harness
        .clone()
        .or_else(|| stored.map(|reviewer| reviewer.harness.clone()))
        .ok_or_else(|| Error("an agent author needs --harness, or a stored --reviewer".into()))?;
    Ok(Author::Agent {
        model,
        effort: args
            .effort
            .clone()
            .or_else(|| stored.map(|reviewer| reviewer.effort.clone()))
            .unwrap_or_else(|| "default".to_owned()),
        harness,
        harness_version: args
            .harness_version
            .clone()
            .or_else(|| stored.and_then(|reviewer| reviewer.harness_version.clone())),
        run: args.run.clone(),
        pass: args.pass.clone(),
        reviewer: args.reviewer.clone(),
    })
}

fn run_start(ctx: &Ctx) -> Result<Value> {
    let scope = ctx.scope()?;
    // Run ids are unique in the repository, so `--run` alone finds the target.
    let taken: HashSet<String> = ctx
        .store
        .targets(&ctx.dir)?
        .into_iter()
        .flat_map(|target| target.runs.into_iter().map(|run| run.id))
        .collect();
    let run = ctx.update(&scope, |target, now| {
        let id = loop {
            let id = random_id();
            if !taken.contains(&id) && !target.runs.iter().any(|run| run.id == id) {
                break id;
            }
        };
        target.runs.push(Run {
            id: id.clone(),
            started_at: now.to_owned(),
            ended_at: None,
            passes: Vec::new(),
            repaired_head: None,
            repair: Vec::new(),
            reveal: None,
            record: None,
            usage: Vec::new(),
        });
        Ok(id)
    })?;
    Ok(json!({ "target": scope.key, "run": run }))
}

struct Recorded {
    commits: Vec<CommitInfo>,
    head_patch_id: Option<String>,
}

/// Patch-ids to adopt for passes whose reviewed tree now equals the head's tree, computed
/// before taking the lock, which only covers the read-modify-write.
struct Adoption {
    head: String,
    head_tree: String,
    passes: HashMap<String, Recorded>,
}

impl Ctx {
    fn adoption(&self, scope: &Scope) -> Result<Adoption> {
        let head_tree = self.repo.tree_of(&scope.head)?;
        let mut passes = HashMap::new();
        for run in scope.existing.iter().flat_map(|target| &target.runs) {
            for pass in &run.passes {
                if pass.tree != head_tree || pass.head == scope.head {
                    continue;
                }
                let commits = if self.repo.is_ancestor(&pass.head, &scope.head)? {
                    self.repo.commits(Some(&pass.head), &scope.head)?
                } else if pass.partial {
                    Vec::new()
                } else {
                    self.repo.commits(scope.base.as_deref(), &scope.head)?
                };
                let head_patch_id = match commits.iter().find(|commit| commit.sha == scope.head) {
                    Some(commit) => commit.patch_id.clone(),
                    None => self.repo.commit_patch_id(&scope.head)?,
                };
                passes.insert(
                    format!("{}.{}", run.id, pass.id),
                    Recorded {
                        commits,
                        head_patch_id,
                    },
                );
            }
        }
        Ok(Adoption {
            head: scope.head.clone(),
            head_tree,
            passes,
        })
    }
}

impl Adoption {
    /// Records the patch-ids and points tree-only anchors at the head; returns the pass ids.
    fn apply(&self, target: &mut Target) -> Vec<String> {
        let mut recorded = Vec::new();
        for run in &mut target.runs {
            for pass in &mut run.passes {
                let id = format!("{}.{}", run.id, pass.id);
                let Some(record) = self.passes.get(&id) else {
                    continue;
                };
                for commit in &record.commits {
                    if !pass.commits.iter().any(|known| known.sha == commit.sha) {
                        pass.commits.push(CommitPatch {
                            sha: commit.sha.clone(),
                            patch_id: commit.patch_id.clone(),
                        });
                    }
                }
                if pass.head_patch_id.is_none() {
                    pass.head_patch_id.clone_from(&record.head_patch_id);
                }
                recorded.push(id);
            }
        }
        for thread in &mut target.threads {
            if let Some(anchor) = &mut thread.anchor
                && anchor.commit.is_none()
                && anchor.tree == self.head_tree
            {
                anchor.commit = Some(self.head.clone());
            }
        }
        recorded
    }
}

fn run_end(
    ctx: &Ctx,
    run_id: &str,
    reveal: Option<Reveal>,
    head: Option<&str>,
    usage: Vec<RoleUsage>,
) -> Result<Value> {
    let scope = ctx.scope_at(head)?;
    let Some(existing) = &scope.existing else {
        return fail(format!("run {run_id} not found in target {}", scope.key));
    };
    let adoption = ctx.adoption(&scope)?;
    // Commits after the run's last reviewed head (the fix commits) are recorded as repaired.
    let last_head = existing
        .runs
        .iter()
        .find(|run| run.id == run_id)
        .and_then(|run| run.passes.last())
        .map(|pass| pass.head.clone());
    let repair = match &last_head {
        Some(last) if ctx.repo.is_ancestor(last, &scope.head)? => ctx
            .repo
            .commits(Some(last), &scope.head)?
            .into_iter()
            .map(|commit| CommitPatch {
                sha: commit.sha,
                patch_id: commit.patch_id,
            })
            .collect(),
        _ => Vec::new(),
    };
    let (ended_at, recorded, spent) = ctx.update(&scope, |target, now| {
        let run = find_run_mut(target, run_id)?;
        if run.ended_at.is_some() {
            return fail(format!("run {run_id} already ended"));
        }
        if let Some(open) = run.passes.iter().find(|pass| pass.ended_at.is_none()) {
            return fail(format!(
                "pass {run_id}.{} is still open; end it first",
                open.id
            ));
        }
        run.ended_at = Some(now.to_owned());
        run.repaired_head = Some(scope.head.clone());
        run.repair = repair;
        run.reveal = reveal;
        run.usage = usage;
        let spent = run_spend(run);
        Ok((now.to_owned(), adoption.apply(target), spent))
    })?;
    let pruned = prune_dir(&ctx.store, &ctx.dir, clock::now_secs()?, false)?;
    Ok(json!({
        "target": scope.key,
        "run": run_id,
        "ended_at": ended_at,
        "recorded": recorded,
        "repaired_head": scope.head,
        "pruned": pruned,
        "spent": spent,
    }))
}

/// Time and tokens of a run: wall time, time in passes (reviewing), time between passes and
/// after the last (fixing), and the tokens reviewers and the main session reported.
pub fn run_spend(run: &Run) -> Value {
    let secs = |text: &str| clock::parse(text).ok();
    let span = |from: &str, to: Option<&str>| match (secs(from), to.and_then(secs)) {
        (Some(from), Some(to)) => Some((to - from).max(0)),
        _ => None,
    };
    let reviewing: i64 = run
        .passes
        .iter()
        .filter_map(|pass| span(&pass.started_at, pass.ended_at.as_deref()))
        .sum();
    let total = span(&run.started_at, run.ended_at.as_deref());
    let reviewer_tokens: u64 = run
        .passes
        .iter()
        .flat_map(|pass| &pass.reviewers)
        .filter_map(|reviewer| reviewer.usage.as_ref()?.tokens)
        .sum();
    let main_tokens: u64 = run.usage.iter().filter_map(|role| role.usage.tokens).sum();
    json!({
        "total_secs": total,
        "reviewing_secs": reviewing,
        "fixing_secs": total.map(|total| (total - reviewing).max(0)),
        "reviewer_tokens": (reviewer_tokens > 0).then_some(reviewer_tokens),
        "main_tokens": (main_tokens > 0).then_some(main_tokens),
        "roles": run.usage,
    })
}

/// Moves a surviving finding to its pass's killed list. A locked pass allows this: the lock
/// freezes what reviewers reported, not the verdict.
fn finding_kill(ctx: &Ctx, id: &str, reason: &str) -> Result<Value> {
    let scope = ctx.scope()?;
    if scope.existing.is_none() {
        return fail(format!("finding {id} not found"));
    }
    let reason = limited(reason.to_owned(), "reason")?;
    let killed = ctx.update(&scope, |target, now| {
        let index = target
            .threads
            .iter()
            .position(|thread| thread.id == id && thread.finding.is_some())
            .ok_or_else(|| Error(format!("finding {id} not found")))?;
        let thread = &target.threads[index];
        let sent = thread.remote.is_some()
            || thread
                .comments
                .iter()
                .any(|comment| comment.post != PostState::Pending);
        if sent {
            return fail(format!(
                "finding {id} is posted or being posted; resolve it with --triage stale instead"
            ));
        }
        let thread = target.threads.remove(index);
        let finding = thread
            .finding
            .ok_or_else(|| Error(format!("finding {id} not found")))?;
        let killed = KilledFinding {
            id: thread.id,
            kind: thread.kind,
            severity: thread.severity,
            anchor: thread.anchor,
            scenario: thread
                .comments
                .first()
                .map(|first| first.body.clone())
                .unwrap_or_default(),
            evidence: finding.evidence,
            tests_run: finding.tests_run,
            post_reveal: finding.post_reveal,
            reason,
            created_at: now.to_owned(),
        };
        let major = major_found(target, &finding.run, &finding.pass);
        let pass = find_pass_mut(target, &finding.run, &finding.pass)?;
        pass.killed.push(killed.clone());
        pass.major_found = major;
        Ok(killed)
    })?;
    Ok(json!({ "target": scope.key, "killed": killed }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewerInput {
    id: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    harness: Option<String>,
    harness_version: Option<String>,
    starting_context: Option<String>,
}

fn read_reviewers(source: Option<&Path>) -> Result<Vec<ReviewerInput>> {
    let Some(source) = source else {
        return Ok(Vec::new());
    };
    let text = if source == Path::new("-") {
        read_stdin("reviewers")?
    } else {
        std::fs::read_to_string(source).map_err(ctx(format!("read {}", source.display())))?
    };
    serde_json::from_str(&text).map_err(ctx("parse reviewers (expected a JSON array)"))
}

/// `manual` is the main session's own pass and `fix` the fixer, which signs fix replies.
const SPECIAL_REVIEWERS: [&str; 2] = ["manual", "fix"];

fn reviewer_number(id: &str) -> Option<u32> {
    id.strip_prefix('r')?.parse().ok()
}

fn assign_reviewers(run: &Run, inputs: Vec<ReviewerInput>) -> Result<Vec<Reviewer>> {
    let known: Vec<&Reviewer> = run.passes.iter().flat_map(|pass| &pass.reviewers).collect();
    let mut next = known
        .iter()
        .filter_map(|reviewer| reviewer_number(&reviewer.id))
        .chain(
            inputs
                .iter()
                .filter_map(|input| input.id.as_deref().and_then(reviewer_number)),
        )
        .max()
        .unwrap_or(0);
    let mut reviewers: Vec<Reviewer> = Vec::new();
    for input in inputs {
        let id = match input.id {
            Some(id) => {
                if reviewer_number(&id).is_none() && !SPECIAL_REVIEWERS.contains(&id.as_str()) {
                    return fail(format!(
                        "reviewer id must look like r<n>, or be manual or fix: {id}"
                    ));
                }
                id
            }
            None => {
                next += 1;
                format!("r{next}")
            }
        };
        if reviewers.iter().any(|reviewer| reviewer.id == id) {
            return fail(format!("reviewer {id} is listed twice"));
        }
        let earlier = known.iter().rev().find(|reviewer| reviewer.id == id);
        let model = input
            .model
            .or_else(|| earlier.map(|reviewer| reviewer.model.clone()))
            .ok_or_else(|| Error(format!("reviewer {id} needs a model")))?;
        let harness = input
            .harness
            .or_else(|| earlier.map(|reviewer| reviewer.harness.clone()))
            .ok_or_else(|| Error(format!("reviewer {id} needs a harness")))?;
        reviewers.push(Reviewer {
            model,
            harness,
            effort: input
                .effort
                .or_else(|| earlier.map(|reviewer| reviewer.effort.clone()))
                .unwrap_or_else(|| "default".to_owned()),
            harness_version: input
                .harness_version
                .or_else(|| earlier.and_then(|reviewer| reviewer.harness_version.clone())),
            starting_context: input.starting_context.unwrap_or_else(|| "fresh".to_owned()),
            usage: None,
            id,
        });
    }
    Ok(reviewers)
}

fn pass_start(
    ctx: &Ctx,
    run_id: &str,
    kind: PassKind,
    base: Option<&str>,
    reviewers: Option<&Path>,
    head: Option<&str>,
) -> Result<Value> {
    let scope = ctx.scope_at(head)?;
    let inputs = read_reviewers(reviewers)?;
    let (commit, tree) = ctx.reviewed_state(&scope)?;
    let previous = scope.existing.as_ref().and_then(|target| {
        target
            .runs
            .iter()
            .find(|run| run.id == run_id)
            .and_then(|run| run.passes.last())
    });
    let previous_head = previous.map(|pass| pass.head.clone());
    let base = match (base, kind) {
        (Some(rev), _) => Some(ctx.repo.commit(rev)?),
        (None, PassKind::Full) => scope.base.clone(),
        (None, PassKind::FixCheck | PassKind::Reveal) => {
            previous_head.clone().or_else(|| scope.base.clone())
        }
    };
    // A pass that starts after the merge-base reviews only its own delta, unless it continues a
    // whole-branch pass from that pass's head. The default branch has no merge-base: its
    // reviews always start at an explicit or earlier head and cover the branch.
    let continues = previous.is_some_and(|pass| !pass.partial) && base == previous_head;
    let partial = base != scope.base && !continues && !ctx.is_default_branch(&scope)?;
    let range_base = if partial { &base } else { &scope.base };
    let commits = ctx.repo.commits(base.as_deref(), &scope.head)?;
    let range_patch_id = ctx.repo.range_patch_id(range_base.as_deref(), &tree)?;
    let head_patch_id = match (&commit, commits.last()) {
        (Some(_), Some(last)) if last.sha == scope.head => last.patch_id.clone(),
        _ => None,
    };
    let pass = ctx.update(&scope, |target, now| {
        let run = find_run_mut(target, run_id)?;
        if run.ended_at.is_some() {
            return fail(format!("run {run_id} already ended"));
        }
        if let Some(open) = run.passes.iter().find(|pass| pass.ended_at.is_none()) {
            return fail(format!(
                "pass {run_id}.{} is still open; end it first",
                open.id
            ));
        }
        let (cycle, number) = match (kind, run.passes.last()) {
            (PassKind::Full, last) => (last.map_or(0, |pass| pass.cycle) + 1, 0),
            (_, Some(last)) => {
                let number = run
                    .passes
                    .iter()
                    .filter(|pass| pass.cycle == last.cycle)
                    .map(|pass| pass.number)
                    .max()
                    .unwrap_or(0);
                (last.cycle, number + 1)
            }
            (_, None) => return fail("a fix-check or reveal pass needs a full pass first"),
        };
        let reviewers = assign_reviewers(run, inputs)?;
        let pass = Pass {
            id: format!("c{cycle}p{number}"),
            cycle,
            number,
            kind,
            base,
            head: scope.head.clone(),
            tree,
            dirty: commit.is_none(),
            commits: commits
                .iter()
                .map(|commit| CommitPatch {
                    sha: commit.sha.clone(),
                    patch_id: commit.patch_id.clone(),
                })
                .collect(),
            range_patch_id,
            head_patch_id,
            partial,
            reviewers,
            started_at: now.to_owned(),
            ended_at: None,
            locked: false,
            major_found: false,
            killed: Vec::new(),
        };
        run.passes.push(pass.clone());
        Ok(pass)
    })?;
    Ok(json!({ "target": scope.key, "run": run_id, "pass": pass_view(&pass) }))
}

/// A pass without its per-commit patch-ids, which only coverage needs; `show` prints them.
fn pass_view(pass: &Pass) -> Value {
    let mut view = json!(pass);
    if let Some(fields) = view.as_object_mut() {
        fields.remove("commits");
        fields.insert("commit_count".to_owned(), json!(pass.commits.len()));
    }
    view
}

fn pass_end(
    ctx: &Ctx,
    run_id: &str,
    pass_id: &str,
    lock: bool,
    findings: Option<&Path>,
    reviewer: Option<&str>,
) -> Result<Value> {
    let scope = ctx.scope()?;
    let (findings, usage) = match findings {
        Some(source) => read_pass_input(source)?,
        None => (Vec::new(), Vec::new()),
    };
    // Group by reviewer, keeping input order, so each reviewer's ids number in order; the
    // output lists ids in input order.
    let mut groups: Vec<(String, Vec<usize>, Vec<FindingInput>)> = Vec::new();
    let mut is_killed = Vec::new();
    for (index, mut finding) in findings.into_iter().enumerate() {
        let id = finding
            .reviewer
            .take()
            .or_else(|| reviewer.map(str::to_owned))
            .ok_or_else(|| {
                Error(format!(
                    "finding {index}: no reviewer; add one or --reviewer"
                ))
            })?;
        is_killed.push(finding.reason.is_some());
        match groups.iter_mut().find(|(known, _, _)| *known == id) {
            Some((_, indexes, group)) => {
                indexes.push(index);
                group.push(finding);
            }
            None => groups.push((id, vec![index], vec![finding])),
        }
    }
    let (pass, added, killed) = ctx.update(&scope, |target, now| {
        let mut ids = vec![String::new(); is_killed.len()];
        for (reviewer, indexes, group) in groups {
            let (new, dead) = add_findings(target, run_id, pass_id, &reviewer, group, now)?;
            let (mut new, mut dead) = (new.into_iter(), dead.into_iter());
            for index in indexes {
                let next = if is_killed[index] {
                    dead.next()
                } else {
                    new.next()
                };
                ids[index] = next.unwrap_or_default();
            }
        }
        let (killed, added): (Vec<_>, Vec<_>) = ids
            .into_iter()
            .enumerate()
            .partition(|(index, _)| is_killed[*index]);
        let added: Vec<String> = added.into_iter().map(|(_, id)| id).collect();
        let killed: Vec<String> = killed.into_iter().map(|(_, id)| id).collect();
        let major = major_found(target, run_id, pass_id);
        let pass = find_pass_mut(target, run_id, pass_id)?;
        if pass.ended_at.is_some() {
            return fail(format!("pass {run_id}.{pass_id} already ended"));
        }
        for spent in &usage {
            let id = spent.who("reviewer")?;
            let reviewer = pass
                .reviewers
                .iter_mut()
                .find(|reviewer| reviewer.id == id)
                .ok_or_else(|| Error(format!("usage: reviewer {id} is not in pass {pass_id}")))?;
            reviewer.usage = Some(spent.usage());
        }
        pass.ended_at = Some(now.to_owned());
        pass.locked = lock;
        pass.major_found = major;
        Ok((pass.clone(), added, killed))
    })?;
    Ok(json!({
        "target": scope.key,
        "run": run_id,
        "pass": pass_view(&pass),
        "added": added,
        "killed": killed,
    }))
}

/// One reviewer finding as `finding add` reads it; see README "Finding input".
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindingInput {
    #[serde(rename = "type")]
    kind: ThreadType,
    severity: Option<Severity>,
    path: Option<String>,
    line: Option<u32>,
    old_line: Option<u32>,
    side: Option<Side>,
    start_line: Option<u32>,
    #[serde(alias = "body")]
    scenario: String,
    evidence: Option<String>,
    #[serde(default)]
    tests_run: Vec<String>,
    #[serde(default)]
    post_reveal: bool,
    #[serde(default)]
    killed: bool,
    reason: Option<String>,
    /// Reviewer id, for `pass end --findings`; `finding add` takes `--reviewer`.
    reviewer: Option<String>,
}

/// One agent's spend as `pass end` and `run end` read it: `reviewer` (pass) or `role` (run).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageInput {
    reviewer: Option<String>,
    role: Option<String>,
    tokens: Option<u64>,
    duration_ms: Option<u64>,
    tool_uses: Option<u64>,
}

impl UsageInput {
    /// The `reviewer` (pass end) or `role` (run end), whichever `field` names.
    fn who(&self, field: &str) -> Result<String> {
        let (wanted, other) = match field {
            "reviewer" => (&self.reviewer, &self.role),
            _ => (&self.role, &self.reviewer),
        };
        match (wanted, other) {
            (Some(name), None) => Ok(name.clone()),
            (_, Some(_)) if field == "reviewer" => fail("usage: pass end takes reviewer, not role"),
            (_, Some(_)) => fail("usage: run end takes role, not reviewer"),
            (None, None) => fail(format!("usage: every entry needs {field}")),
        }
    }

    fn usage(&self) -> Usage {
        Usage {
            tokens: self.tokens,
            duration_ms: self.duration_ms,
            tool_uses: self.tool_uses,
        }
    }
}

fn read_source(source: &Path, what: &str) -> Result<String> {
    if source == Path::new("-") {
        read_stdin(what)
    } else {
        std::fs::read_to_string(source).map_err(ctx(format!("read {}", source.display())))
    }
}

/// `pass end --findings` input: a findings array, or `{"findings": [...], "usage": [...]}`.
fn read_pass_input(source: &Path) -> Result<(Vec<FindingInput>, Vec<UsageInput>)> {
    let value: Value = serde_json::from_str(&read_source(source, "findings")?)
        .map_err(ctx("parse findings JSON"))?;
    match value {
        Value::Object(mut fields)
            if fields.contains_key("findings") || fields.contains_key("usage") =>
        {
            if let Some(other) = fields
                .keys()
                .find(|key| *key != "findings" && *key != "usage")
            {
                return fail(format!(
                    "unknown field {other}; expected findings and usage"
                ));
            }
            let findings = parse_findings(fields.remove("findings").unwrap_or(json!([])), true)?;
            let usage = serde_json::from_value(fields.remove("usage").unwrap_or(json!([])))
                .map_err(ctx("parse usage"))?;
            Ok((findings, usage))
        }
        value => Ok((parse_findings(value, true)?, Vec::new())),
    }
}

/// Findings from `source` (`-` for stdin); `allow_empty` accepts `[]` (a clean pass).
fn read_findings(source: &Path, allow_empty: bool) -> Result<Vec<FindingInput>> {
    let value: Value = serde_json::from_str(&read_source(source, "findings")?)
        .map_err(ctx("parse findings JSON"))?;
    parse_findings(value, allow_empty)
}

fn parse_findings(value: Value, allow_empty: bool) -> Result<Vec<FindingInput>> {
    let items = match value {
        Value::Array(items) => items,
        item @ Value::Object(_) => vec![item],
        _ => return fail("findings must be a JSON object or array"),
    };
    if items.is_empty() && !allow_empty {
        return fail("no findings in input");
    }
    let mut findings = Vec::new();
    for (index, item) in items.into_iter().enumerate() {
        let finding: FindingInput =
            serde_json::from_value(item).map_err(ctx(format!("finding {index}")))?;
        if finding.scenario.trim().is_empty() {
            return fail(format!("finding {index}: scenario must not be empty"));
        }
        if finding.kind == ThreadType::Bug && finding.severity.is_none() {
            return fail(format!("finding {index}: a bug needs a severity"));
        }
        match (&finding.reason, finding.killed) {
            (None, true) => {
                return fail(format!("finding {index}: a killed finding needs a reason"));
            }
            (Some(_), false) => {
                return fail(format!(
                    "finding {index}: reason is only for killed findings"
                ));
            }
            (Some(reason), true) if reason.trim().is_empty() => {
                return fail(format!("finding {index}: reason must not be empty"));
            }
            _ => {}
        }
        findings.push(finding);
    }
    Ok(findings)
}

fn next_number(ids: impl Iterator<Item = String>, prefix: &str) -> u32 {
    ids.filter_map(|id| id.strip_prefix(prefix)?.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
}

fn finding_add(ctx: &Ctx, run_id: &str, pass_id: &str, reviewer_id: &str) -> Result<Value> {
    let scope = ctx.scope()?;
    let findings = read_findings(Path::new("-"), false)?;
    if let Some(index) = findings.iter().position(|finding| {
        finding
            .reviewer
            .as_deref()
            .is_some_and(|id| id != reviewer_id)
    }) {
        return fail(format!(
            "finding {index}: its reviewer differs from --reviewer {reviewer_id}"
        ));
    }
    let (added, killed) = ctx.update(&scope, |target, now| {
        add_findings(target, run_id, pass_id, reviewer_id, findings, now)
    })?;
    Ok(json!({ "target": scope.key, "added": added, "killed": killed }))
}

/// Stores one reviewer's findings on a pass; returns the new thread and killed ids.
fn add_findings(
    target: &mut Target,
    run_id: &str,
    pass_id: &str,
    reviewer_id: &str,
    findings: Vec<FindingInput>,
    now: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    {
        let run_ended = target
            .runs
            .iter()
            .any(|run| run.id == run_id && run.ended_at.is_some());
        let pass = find_pass(target, run_id, pass_id)?;
        if run_ended {
            return fail(format!("run {run_id} already ended"));
        }
        if pass.locked {
            return fail(format!("pass {run_id}.{pass_id} is locked"));
        }
        let reviewer = pass
            .reviewers
            .iter()
            .find(|reviewer| reviewer.id == reviewer_id)
            .ok_or_else(|| {
                let known: Vec<&str> = pass.reviewers.iter().map(|r| r.id.as_str()).collect();
                Error(format!(
                    "reviewer {reviewer_id} is not in pass {pass_id}; its reviewers: {}",
                    known.join(", ")
                ))
            })?;
        let commit = (!pass.dirty).then(|| pass.head.clone());
        let tree = pass.tree.clone();
        let agent = Author::Agent {
            model: reviewer.model.clone(),
            effort: reviewer.effort.clone(),
            harness: reviewer.harness.clone(),
            harness_version: reviewer.harness_version.clone(),
            run: Some(run_id.to_owned()),
            pass: Some(pass_id.to_owned()),
            reviewer: Some(reviewer_id.to_owned()),
        };
        let prefix = format!("{run_id}.{pass_id}.{reviewer_id}.f");
        let ids = target
            .threads
            .iter()
            .map(|thread| thread.id.clone())
            .chain(passes(target).flat_map(|pass| pass.killed.iter().map(|f| f.id.clone())));
        let mut number = next_number(ids, &prefix);
        let mut threads = Vec::new();
        let mut killed = Vec::new();
        for finding in findings {
            number += 1;
            let id = format!("{prefix}{number}");
            let anchor = build_anchor(
                &AnchorInput {
                    path: finding.path.as_deref(),
                    line: finding.line,
                    old_line: finding.old_line,
                    side: finding.side,
                    start_line: finding.start_line,
                },
                commit.clone(),
                tree.clone(),
            )
            .map_err(|error| Error(format!("finding {id}: {error}")))?;
            if let Some(reason) = finding.reason {
                killed.push(KilledFinding {
                    id,
                    kind: finding.kind,
                    severity: finding.severity,
                    anchor,
                    scenario: finding.scenario,
                    evidence: finding.evidence,
                    tests_run: finding.tests_run,
                    post_reveal: finding.post_reveal,
                    reason,
                    created_at: now.to_owned(),
                });
                continue;
            }
            threads.push(local_thread(
                NewThread {
                    id,
                    kind: finding.kind,
                    severity: finding.severity,
                    anchor,
                    finding: Some(FindingInfo {
                        run: run_id.to_owned(),
                        pass: pass_id.to_owned(),
                        reviewer: reviewer_id.to_owned(),
                        evidence: finding.evidence,
                        tests_run: finding.tests_run,
                        post_reveal: finding.post_reveal,
                    }),
                },
                agent.clone(),
                finding.scenario,
                now,
            ));
        }
        let added: Vec<String> = threads.iter().map(|thread| thread.id.clone()).collect();
        let killed_ids: Vec<String> = killed.iter().map(|finding| finding.id.clone()).collect();
        target.threads.extend(threads);
        let major = major_found(target, run_id, pass_id);
        let pass = find_pass_mut(target, run_id, pass_id)?;
        pass.killed.extend(killed);
        pass.major_found = major;
        Ok((added, killed_ids))
    }
}

fn comment(ctx: &Ctx, args: &CommentArgs) -> Result<Value> {
    let scope = ctx.scope()?;
    let body = read_body(&args.body)?;
    let anchor_input = AnchorInput {
        path: args.path.as_deref(),
        line: args.line,
        old_line: args.old_line,
        side: args.side,
        start_line: args.start_line,
    };
    let anchor = if anchor_input.path.is_some() {
        let (commit, tree) = match &args.commit {
            Some(rev) => {
                let commit = ctx.repo.commit(rev)?;
                let tree = ctx.repo.tree_of(&commit)?;
                (Some(commit), tree)
            }
            None => ctx.reviewed_state(&scope)?,
        };
        let anchor = build_anchor(&anchor_input, commit, tree)?;
        if let Some(anchor) = &anchor
            && anchor.side == Side::New
        {
            let state = anchor.commit.as_deref().unwrap_or(&anchor.tree);
            if !ctx
                .repo
                .object_exists(&format!("{state}:{}", anchor.path))?
            {
                return fail(format!("{} does not exist at {state}", anchor.path));
            }
        }
        anchor
    } else {
        build_anchor(&anchor_input, None, String::new())?
    };
    let default = ctx.default_author()?;
    let thread = ctx.update(&scope, |target, now| {
        let author = author(&args.author, target, default)?;
        let number = next_number(target.threads.iter().map(|thread| thread.id.clone()), "h") + 1;
        let thread = local_thread(
            NewThread {
                id: format!("h{number}"),
                kind: args.kind,
                severity: args.severity,
                anchor,
                finding: None,
            },
            author,
            body,
            now,
        );
        target.threads.push(thread.clone());
        Ok(thread)
    })?;
    Ok(json!({ "target": scope.key, "thread": thread }))
}

fn reply(ctx: &Ctx, args: &ReplyArgs) -> Result<Value> {
    let scope = ctx.scope()?;
    if scope.existing.is_none() {
        return fail(format!("thread {} not found", args.id));
    }
    let body = read_body(&args.body)?;
    let default = ctx.default_author()?;
    let thread = ctx.update(&scope, |target, now| {
        let author = author(&args.author, target, default)?;
        let thread = find_thread_mut(target, &args.id)?;
        let mut comment = local_comment(&thread.id, &thread.comments, author, body, now);
        comment.local = args.local;
        thread.comments.push(comment);
        thread.updated_at = now.to_owned();
        Ok(thread.clone())
    })?;
    Ok(json!({ "target": scope.key, "thread": thread }))
}

/// Records an approval of the head as a general thread for `publish`.
fn approve(ctx: &Ctx, args: &ApproveArgs) -> Result<Value> {
    let scope = ctx.scope()?;
    if !scope.head_known {
        return fail("approve needs a head: check out the branch or pass --target");
    }
    let head = scope.head.clone();
    let blocking: Vec<&str> = scope
        .existing
        .iter()
        .flat_map(|target| &target.threads)
        .filter(|thread| {
            is_open(thread)
                && matches!(
                    thread.kind,
                    ThreadType::Bug | ThreadType::Requirement | ThreadType::Question
                )
        })
        .map(|thread| thread.id.as_str())
        .collect();
    if !blocking.is_empty() {
        return fail(format!(
            "open findings block approval: {}",
            blocking.join(", ")
        ));
    }
    let body = match &args.body {
        Some(body) => read_body(body)?,
        None => format!("Approved at {}.", &head[..head.len().min(12)]),
    };
    let default = ctx.default_author()?;
    let formal = args.formal;
    let thread = ctx.update(&scope, |target, now| {
        // Approving the same head again reuses its approval, so it posts once; `--formal`
        // upgrades a comment approval and retries a refused formal one.
        if let Some(existing) = target.threads.iter_mut().rev().find(|thread| {
            thread
                .approval
                .as_ref()
                .is_some_and(|approval| approval.head == head)
        }) {
            if formal && let Some(approval) = existing.approval.as_mut() {
                approval.formal = true;
                approval.refused = None;
            }
            return Ok(existing.clone());
        }
        let author = author(&args.author, target, default)?;
        let number = next_number(target.threads.iter().map(|thread| thread.id.clone()), "h") + 1;
        let mut thread = local_thread(
            NewThread {
                id: format!("h{number}"),
                kind: ThreadType::Comment,
                severity: None,
                anchor: None,
                finding: None,
            },
            author,
            body,
            now,
        );
        thread.resolved = true;
        thread.approval = Some(Approval {
            head: head.clone(),
            formal,
            approved: false,
            refused: None,
        });
        target.threads.push(thread.clone());
        Ok(thread)
    })?;
    Ok(json!({ "target": scope.key, "thread": thread }))
}

fn normalize_resolution(ctx: &Ctx, resolution: &str) -> Result<String> {
    if resolution == "stale" || resolution == "answered" {
        return Ok(resolution.to_owned());
    }
    if let Some(reason) = resolution.strip_prefix("not fixed:") {
        let reason = limited(reason.trim().to_owned(), "not fixed reason")?;
        return Ok(format!("not fixed: {reason}"));
    }
    if let Some(rev) = resolution.strip_prefix("fixed ") {
        return Ok(format!("fixed {}", ctx.repo.commit(rev.trim())?));
    }
    fail("resolution must be `fixed <sha>`, `not fixed: <reason>`, `stale` or `answered`")
}

fn resolve(ctx: &Ctx, args: &ResolveArgs) -> Result<Value> {
    let scope = ctx.scope()?;
    if scope.existing.is_none() {
        return fail(format!("thread {} not found", args.ids[0]));
    }
    // `--fixed` and `--not-fixed` are shorthands for the triage, resolution and reply of a
    // fix and of a failed fix check.
    let (triage, resolution, reopen, default_reply) = match (&args.fixed, &args.not_fixed) {
        (Some(sha), _) => {
            let sha = ctx.repo.commit(sha)?;
            let reply = format!("Fixed in {}.", &sha[..sha.len().min(12)]);
            (
                Some(Triage::Fixed),
                Some(format!("fixed {sha}")),
                false,
                Some(reply),
            )
        }
        (None, Some(reason)) => {
            let reason = reason.trim().trim_end_matches('.');
            (
                None,
                Some(format!("not fixed: {reason}")),
                true,
                Some(format!("Not fixed: {reason}.")),
            )
        }
        (None, None) => (args.triage, args.resolution.clone(), args.reopen, None),
    };
    let resolution = resolution
        .as_deref()
        .map(|text| normalize_resolution(ctx, text))
        .transpose()?;
    let records_only = matches!(triage, Some(Triage::Valid | Triage::Question));
    if !reopen && records_only && resolution.is_some() {
        return fail("--resolution needs a resolving triage (fixed or stale)");
    }
    let resolution = match (triage, resolution) {
        (Some(Triage::Stale), None) if !reopen => Some("stale".to_owned()),
        (Some(Triage::Fixed), None) if !reopen => {
            return fail("--triage fixed needs --resolution 'fixed <sha>'");
        }
        (_, resolution) => resolution,
    };
    let reply = match (&args.reply, default_reply) {
        (Some(body), _) => Some(read_body(body)?),
        (None, reply) => reply,
    };
    let default = match reply {
        Some(_) => ctx.default_author()?,
        None => None,
    };
    let threads = ctx.update(&scope, |target, now| {
        let author = match reply {
            Some(_) => Some(author(&args.author, target, default)?),
            None => None,
        };
        let mut changed = Vec::new();
        for id in &args.ids {
            let thread = find_thread_mut(target, id)?;
            if let (Some(body), Some(author)) = (&reply, &author) {
                let comment = local_comment(
                    &thread.id,
                    &thread.comments,
                    author.clone(),
                    body.clone(),
                    now,
                );
                thread.comments.push(comment);
            }
            if triage.is_some() {
                thread.triage = triage;
            }
            let was = thread.resolved;
            if reopen {
                thread.resolved = false;
                thread.resolution.clone_from(&resolution);
                thread.held = triage == Some(Triage::Question);
            } else if records_only {
                thread.held = triage == Some(Triage::Question);
            } else {
                thread.resolved = true;
                thread.held = false;
                if resolution.is_some() {
                    thread.resolution.clone_from(&resolution);
                }
            }
            if thread.resolved != was {
                thread.resolve_pending = Some(thread.resolved);
            }
            thread.updated_at = now.to_owned();
            changed.push(thread.clone());
        }
        Ok(changed)
    })?;
    if let [thread] = threads.as_slice() {
        return Ok(json!({ "target": scope.key, "thread": thread }));
    }
    let threads: Vec<Value> = threads
        .iter()
        .map(|thread| {
            json!({
                "id": thread.id,
                "resolved": thread.resolved,
                "resolution": thread.resolution,
                "held": thread.held,
            })
        })
        .collect();
    Ok(json!({ "target": scope.key, "threads": threads }))
}

/// Maps anchors to the current head, caching one diff per anchor commit or tree.
struct Locator<'a> {
    repo: &'a Repo,
    /// `None` when no head is known; local anchors then have no current place.
    head: Option<String>,
    diffs: HashMap<String, Option<Vec<FileDiff>>>,
}

impl Locator<'_> {
    /// An anchor whose commit or tree is gone (collected) counts as outdated.
    fn locate(&mut self, anchor: &Anchor, head: &str) -> Location {
        let from = anchor.commit.clone().unwrap_or_else(|| anchor.tree.clone());
        if from == head {
            return anchors::unchanged(anchor);
        }
        let repo = self.repo;
        let files = self.diffs.entry(from).or_insert_with_key(|from| {
            repo.diff_u0(from, head)
                .ok()
                .map(|diff| anchors::parse(&diff))
        });
        match files {
            Some(files) => anchors::locate(files, anchor),
            None => Location {
                outdated: true,
                ..anchors::unchanged(anchor)
            },
        }
    }

    /// Local anchors map through git; imported threads use the host's path, line and outdated
    /// state.
    fn current(&mut self, thread: &Thread) -> Value {
        if let Some(anchor) = &thread.anchor {
            let Some(head) = self.head.clone() else {
                return Value::Null;
            };
            let location = self.locate(anchor, &head);
            return json!({ "path": location.path, "line": location.line, "outdated": location.outdated });
        }
        match &thread.remote {
            Some(remote) if remote.path.is_some() => json!({
                "path": remote.path,
                "line": remote.line.or(remote.old_line).or(remote.original_line),
                "outdated": remote.outdated,
            }),
            _ => Value::Null,
        }
    }

    fn view(&mut self, thread: &Thread) -> Result<Value> {
        let mut value = serde_json::to_value(thread).map_err(ctx("serialize thread"))?;
        value["current"] = self.current(thread);
        Ok(value)
    }
}

fn list(ctx: &Ctx, args: &ListArgs) -> Result<Output> {
    let scope = ctx.scope()?;
    let (no_threads, no_runs) = (Vec::new(), Vec::new());
    let (threads, runs) = scope
        .existing
        .as_ref()
        .map_or((&no_threads, &no_runs), |target| {
            (&target.threads, &target.runs)
        });
    let in_run = |id: &str| {
        let Some(run) = &args.run else {
            return true;
        };
        let Some(rest) = id.strip_prefix(&format!("{run}.c")) else {
            return false;
        };
        args.cycle.is_none_or(|cycle| {
            rest.split_once('p')
                .is_some_and(|(found, _)| found == cycle.to_string())
        })
    };
    let selected: Vec<&Thread> = threads
        .iter()
        .filter(|thread| !args.open || is_open(thread))
        .filter(|thread| in_run(&thread.id))
        .collect();
    let killed: Vec<&KilledFinding> = runs
        .iter()
        .flat_map(|run| &run.passes)
        .flat_map(|pass| &pass.killed)
        .filter(|finding| in_run(&finding.id))
        .collect();
    let mut locator = Locator {
        repo: &ctx.repo,
        head: match &args.head {
            Some(rev) => Some(ctx.repo.commit(rev)?),
            None => scope.head_known.then(|| scope.head.clone()),
        },
        diffs: HashMap::new(),
    };
    if let Format::Prompt = args.format {
        return Ok(Output::Text(prompt(
            &scope.key,
            &selected,
            &killed,
            &mut locator,
        )));
    }
    let views = selected
        .iter()
        .map(|thread| locator.view(thread))
        .collect::<Result<Vec<_>>>()?;
    Ok(Output::Json(json!({
        "target": scope.existing.as_ref().map(|_| &scope.key),
        "threads": views,
        "killed": killed,
    })))
}

fn author_label(author: &Author) -> String {
    match author {
        Author::Human { login, .. } => login.clone(),
        Author::Agent {
            model,
            effort,
            harness,
            run,
            pass,
            reviewer,
            ..
        } => {
            let mut label = format!("{model} {effort} {harness}");
            if let (Some(run), Some(pass), Some(reviewer)) = (run, pass, reviewer) {
                let _ = write!(label, " {run}.{pass}.{reviewer}");
            }
            label
        }
    }
}

fn kind_label(kind: ThreadType, severity: Option<Severity>) -> String {
    let kind = serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    match severity.and_then(|severity| serde_json::to_value(severity).ok()) {
        Some(Value::String(severity)) => format!("{kind}/{severity}"),
        _ => kind,
    }
}

const HIDDEN: &str = "  (comment by another author, text hidden)\n";

/// Comments written here or by the authenticated forge user; reviewers never see other text.
fn mine(comment: &Comment) -> bool {
    comment.remote.as_ref().is_none_or(|remote| remote.is_me)
}

fn place(current: &Value) -> String {
    let Some(path) = current["path"].as_str() else {
        return "general".to_owned();
    };
    let mut text = match current["line"].as_u64() {
        Some(line) => format!("{path}:{line}"),
        None => path.to_owned(),
    };
    if current["outdated"] == true {
        text.push_str(" (outdated)");
    }
    text
}

/// Plain-text input for fix-check and reveal prompts.
fn prompt(
    key: &str,
    threads: &[&Thread],
    killed: &[&KilledFinding],
    locator: &mut Locator,
) -> String {
    let mut out = format!("Review items for {key}\n");
    let _ = writeln!(out, "\nThreads ({}):", threads.len());
    if threads.is_empty() {
        out.push_str("- none\n");
    }
    for thread in threads {
        let current = locator.current(thread);
        let state = match (&thread.resolution, thread.resolved, thread.held) {
            (_, false, true) => "held question".to_owned(),
            (Some(resolution), true, _) => format!("resolved: {resolution}"),
            (None, true, _) => "resolved".to_owned(),
            (Some(resolution), false, _) => format!("open: {resolution}"),
            (None, false, false) => "open".to_owned(),
        };
        let _ = writeln!(
            out,
            "- {} [{}] {} ({state})",
            thread.id,
            kind_label(thread.kind, thread.severity),
            place(&current)
        );
        let url = thread
            .remote
            .as_ref()
            .and_then(|remote| remote.url.as_deref())
            .or_else(|| {
                thread
                    .comments
                    .first()
                    .and_then(|first| first.remote.as_ref()?.url.as_deref())
            });
        if let Some(url) = url {
            let _ = writeln!(out, "  URL: {url}");
        }
        let mut comments = thread.comments.iter();
        if let Some(first) = comments.next() {
            if mine(first) {
                let _ = writeln!(
                    out,
                    "  {}: {}",
                    author_label(&first.author),
                    indent(&first.body)
                );
            } else {
                out.push_str(HIDDEN);
            }
        }
        if let Some(finding) = &thread.finding {
            if let Some(evidence) = &finding.evidence {
                let _ = writeln!(out, "  Evidence: {}", indent(evidence));
            }
            if !finding.tests_run.is_empty() {
                let _ = writeln!(out, "  Tests run: {}", finding.tests_run.join("; "));
            }
            if finding.post_reveal {
                out.push_str("  post-reveal\n");
            }
        }
        for reply in comments {
            if !mine(reply) {
                out.push_str(HIDDEN);
                continue;
            }
            let _ = writeln!(
                out,
                "  Reply from {}: {}",
                author_label(&reply.author),
                indent(&reply.body)
            );
        }
    }
    let _ = writeln!(out, "\nKilled findings ({}):", killed.len());
    if killed.is_empty() {
        out.push_str("- none\n");
    }
    for finding in killed {
        let _ = writeln!(
            out,
            "- {} [{}] {}: {}\n  Killed: {}",
            finding.id,
            kind_label(finding.kind, finding.severity),
            finding.anchor.as_ref().map_or_else(
                || "general".to_owned(),
                |anchor| place(&json!({
                    "path": anchor.path,
                    "line": anchor.line.or(anchor.old_line),
                    "outdated": false,
                }))
            ),
            indent(&finding.scenario),
            indent(&finding.reason)
        );
    }
    out
}

fn indent(text: &str) -> String {
    text.trim_end().replace('\n', "\n    ")
}

fn show(ctx: &Ctx, id: &str) -> Result<Value> {
    let scope = ctx.scope()?;
    let not_found = || Error(format!("{id} not found in target {}", scope.key));
    let target = scope.existing.as_ref().ok_or_else(not_found)?;
    let mut locator = Locator {
        repo: &ctx.repo,
        head: scope.head_known.then(|| scope.head.clone()),
        diffs: HashMap::new(),
    };
    if let Some(thread) = target.threads.iter().find(|thread| thread.id == id) {
        return Ok(
            json!({ "target": scope.key, "kind": "thread", "thread": locator.view(thread)? }),
        );
    }
    for run in &target.runs {
        if run.id == id {
            return Ok(json!({ "target": scope.key, "kind": "run", "run": run }));
        }
        for pass in &run.passes {
            if format!("{}.{}", run.id, pass.id) == id {
                return Ok(
                    json!({ "target": scope.key, "kind": "pass", "run": run.id, "pass": pass }),
                );
            }
            if let Some(finding) = pass.killed.iter().find(|finding| finding.id == id) {
                return Ok(json!({ "target": scope.key, "kind": "killed", "finding": finding }));
            }
        }
    }
    Err(not_found())
}

#[derive(Default)]
struct Reviewed {
    patch_ids: HashSet<String>,
    trees: HashSet<String>,
    shas: HashSet<String>,
}

/// A pass covering a head, with its target and run.
#[derive(Clone)]
struct Covered<'a> {
    target: &'a str,
    run: &'a str,
    pass: &'a Pass,
    matched: Vec<&'static str>,
}

/// Ended passes of `targets` whose reviewed state matches `tree`, the whole-range patch-id or
/// the head commit's patch-id.
fn coverage<'a>(
    targets: &[&'a Target],
    tree: &str,
    range_patch_id: Option<&str>,
    head_patch_id: Option<&str>,
) -> Vec<Covered<'a>> {
    let mut covered = Vec::new();
    for target in targets {
        for run in &target.runs {
            // A partial pass reviewed only its delta, so it covers commits, never the whole
            // head.
            for pass in run
                .passes
                .iter()
                .filter(|pass| pass.ended_at.is_some() && !pass.partial)
            {
                let mut matched = Vec::new();
                if pass.tree == tree {
                    matched.push("tree");
                }
                if range_patch_id.is_some() && pass.range_patch_id.as_deref() == range_patch_id {
                    matched.push("range-patch-id");
                }
                // Only a one-commit head (the caller passes no id otherwise) may match by its
                // own patch-id, and only the pass's whole range: a matching last commit says
                // nothing about rewritten earlier ones.
                if let Some(id) = head_patch_id
                    && pass.range_patch_id.as_deref() == Some(id)
                {
                    matched.push("head-patch-id");
                }
                if !matched.is_empty() {
                    covered.push(Covered {
                        target: &target.key,
                        run: &run.id,
                        pass,
                        matched,
                    });
                }
            }
        }
    }
    covered
}

/// Passes covering `head` itself; a changed merge the caller found unreviewed voids them all.
fn head_coverage<'a>(
    repo: &Repo,
    targets: &[&'a Target],
    base: Option<&str>,
    head: &str,
    head_tree: &str,
    commits: &[CommitInfo],
) -> Result<Vec<Covered<'a>>> {
    let head_patch_id = match commits {
        [only] if only.sha == head => only.patch_id.clone(),
        _ => None,
    };
    let range_patch_id = repo.range_patch_id(base, head)?;
    Ok(coverage(
        targets,
        head_tree,
        range_patch_id.as_deref(),
        head_patch_id.as_deref(),
    ))
}

/// Changed merges after the last reviewed head: they bring in code no pass reviewed.
fn unreviewed_merges<'h>(
    repo: &Repo,
    merges: &[&'h LogEntry],
    last_reviewed: Option<&str>,
) -> Result<HashSet<&'h str>> {
    let mut unreviewed = HashSet::new();
    for merge in merges {
        let after = match last_reviewed {
            Some(last) => !repo.is_ancestor(&merge.sha, last)?,
            None => true,
        };
        if after {
            unreviewed.insert(merge.sha.as_str());
        }
    }
    Ok(unreviewed)
}

/// The reviews of run `run` covering `head`, counted the way `status` counts
/// `reviews_at_head`.
fn run_reviews_at(ctx: &Ctx, target: &Target, run: &str, head: &str) -> Result<usize> {
    if !ctx.repo.object_exists(&format!("{head}^{{commit}}"))? {
        return Ok(0);
    }
    let base = match (target.kind, &target.branch) {
        (TargetKind::Branch, Some(branch)) => Some(ctx.branch_base(branch, head)?.0),
        _ => target.base.clone(),
    };
    let history = ctx.repo.log(base.as_deref(), head)?;
    let merges = changed_merges(&ctx.repo, &history)?;
    let last_reviewed = last_reviewed_head(ctx, head)?;
    if !unreviewed_merges(&ctx.repo, &merges, last_reviewed.as_deref())?.is_empty() {
        return Ok(0);
    }
    let commits = ctx.repo.commits(base.as_deref(), head)?;
    let tree = ctx.repo.tree_of(head)?;
    let covered: Vec<_> =
        head_coverage(&ctx.repo, &[target], base.as_deref(), head, &tree, &commits)?
            .into_iter()
            .filter(|covered| covered.run == run)
            .collect();
    Ok(count(&covered))
}

fn coverage_json(covered: &[Covered]) -> Value {
    covered
        .iter()
        .map(|covered| {
            json!({
                "target": covered.target,
                "run": covered.run,
                "pass": covered.pass.id,
                "kind": covered.pass.kind,
                "major_found": covered.pass.major_found,
                "match": covered.matched,
            })
        })
        .collect()
}

/// Full reviews plus clean fix checks; parallel reviewers in one pass count once.
fn count(covered: &[Covered]) -> usize {
    covered
        .iter()
        .filter(|covered| {
            let pass = covered.pass;
            pass.kind == PassKind::Full || (pass.kind == PassKind::FixCheck && !pass.major_found)
        })
        .count()
}

fn status(ctx: &Ctx, head_arg: Option<&str>) -> Result<Value> {
    let mut scope = ctx.scope()?;
    if let Some(rev) = head_arg {
        scope.head = ctx.repo.commit(rev)?;
        scope.head_known = true;
    }
    if !scope.head_known {
        return Ok(status_without_head(ctx, &scope));
    }
    let target = scope.existing.as_ref();
    let head = scope.head.clone();
    let head_tree = ctx.repo.tree_of(&head)?;
    let commits = ctx.repo.commits(scope.base.as_deref(), &head)?;
    let history = ctx.repo.log(scope.base.as_deref(), &head)?;
    let merges = changed_merges(&ctx.repo, &history)?;
    let last_reviewed = last_reviewed_head(ctx, &head)?;
    let unreviewed_merges = unreviewed_merges(&ctx.repo, &merges, last_reviewed.as_deref())?;
    let merge_after = !unreviewed_merges.is_empty();
    let others: Vec<Target> = ctx
        .store
        .targets(&ctx.dir)?
        .into_iter()
        .filter(|other| Some(&other.key) != target.map(|target| &target.key))
        .collect();
    let others_ref: Vec<&Target> = others.iter().collect();
    // Another target's pass (a `--range` review of the branch's end point) covers the head
    // when it reviewed the same diff; a tree match alone may sit on another base.
    // A run stored in two files (a rename to the PR/MR key cut short) counts once.
    let covered_at = |tree: &str, range: Option<&str>, head_id: Option<&str>| {
        let mut covered = coverage(target.as_slice(), tree, range, head_id);
        for mut other in coverage(&others_ref, tree, range, head_id) {
            other.matched.retain(|matched| *matched != "tree");
            let seen = covered
                .iter()
                .any(|known| known.run == other.run && known.pass.id == other.pass.id);
            if !other.matched.is_empty() && !seen {
                covered.push(other);
            }
        }
        covered
    };
    let covered = if merge_after {
        Vec::new()
    } else {
        let head_patch_id = match commits.as_slice() {
            [only] if only.sha == head => only.patch_id.clone(),
            _ => None,
        };
        let range = ctx.repo.range_patch_id(scope.base.as_deref(), &head)?;
        covered_at(&head_tree, range.as_deref(), head_patch_id.as_deref())
    };

    let mut reviewed = Reviewed::default();
    for pass in target
        .iter()
        .flat_map(|target| passes(target))
        .filter(|pass| pass.ended_at.is_some())
    {
        reviewed.patch_ids.extend(pass_patch_ids(pass).cloned());
        reviewed.trees.insert(pass.tree.clone());
        if !pass.dirty {
            reviewed.shas.insert(pass.head.clone());
        }
        reviewed
            .shas
            .extend(pass.commits.iter().map(|commit| commit.sha.clone()));
    }
    // Commits a review of another target of this repository recorded (a `--range` review of
    // the branch's end point, an earlier branch target) are reviewed here too.
    for other in &others {
        for pass in passes(other).filter(|pass| pass.ended_at.is_some()) {
            reviewed.patch_ids.extend(pass_patch_ids(pass).cloned());
            if !pass.dirty {
                reviewed.shas.insert(pass.head.clone());
            }
            reviewed
                .shas
                .extend(pass.commits.iter().map(|commit| commit.sha.clone()));
        }
    }
    // Repaired commits (fixes after a run's last pass) count as reviewed but never add to
    // reviews_at_head, also when another target's run repaired them (a range review's fix).
    let mut repaired = Reviewed::default();
    for commit in target
        .iter()
        .flat_map(|target| &target.runs)
        .chain(others.iter().flat_map(|other| &other.runs))
        .flat_map(|run| &run.repair)
    {
        repaired.shas.insert(commit.sha.clone());
        repaired.patch_ids.extend(commit.patch_id.clone());
    }
    let mut commit_views: Vec<Value> = commits
        .iter()
        .map(|commit| {
            let in_set = |set: &Reviewed| {
                set.shas.contains(&commit.sha)
                    || set.trees.contains(&commit.tree)
                    || commit
                        .patch_id
                        .as_ref()
                        .is_some_and(|id| set.patch_ids.contains(id))
            };
            let matched = if in_set(&reviewed) {
                Some("pass")
            } else if in_set(&repaired) {
                Some("repair")
            } else {
                None
            };
            json!({
                "sha": commit.sha,
                "subject": commit.subject,
                "patch_id": commit.patch_id,
                "reviewed": matched.is_some(),
                "match": matched,
            })
        })
        .collect();
    // A changed merge is reviewed once a whole-branch pass reviewed a head that contains it.
    commit_views.extend(merges.iter().map(|merge| {
        let reviewed = !unreviewed_merges.contains(merge.sha.as_str());
        json!({
            "sha": merge.sha,
            "subject": merge.subject,
            "patch_id": null,
            "reviewed": reviewed,
            "match": reviewed.then_some("pass"),
            "merge": true,
        })
    }));
    let order: HashMap<&str, usize> = history
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.sha.as_str(), index))
        .collect();
    commit_views.sort_by_key(|view| view["sha"].as_str().and_then(|sha| order.get(sha)).copied());
    let unreviewed: Vec<&Value> = commit_views
        .iter()
        .filter(|view| view["reviewed"] == false)
        .map(|view| &view["sha"])
        .collect();
    // Where the next review starts: before the oldest unreviewed commit, else after the last
    // reviewed head.
    let review_from = match unreviewed.first().and_then(|sha| sha.as_str()) {
        Some(oldest) => ctx.repo.parent(oldest)?,
        None => last_reviewed.clone(),
    };

    // The work tree belongs to the target only when its branch is checked out.
    let checked_out = scope.kind == TargetKind::Branch
        && scope.branch.is_some()
        && ctx.repo.current_branch()? == scope.branch;
    let mut worktree_covered = Vec::new();
    let worktree = if head_arg.is_none() && checked_out {
        let tree = ctx.repo.worktree_tree()?;
        let dirty = tree != head_tree;
        worktree_covered = if dirty && !merge_after {
            let range = ctx.repo.range_patch_id(scope.base.as_deref(), &tree)?;
            covered_at(&tree, range.as_deref(), None)
        } else {
            covered.clone()
        };
        json!({
            "dirty": dirty,
            "tree": tree,
            "reviewed": !worktree_covered.is_empty(),
            "reviews": count(&worktree_covered),
            "covered_by": coverage_json(&worktree_covered),
        })
    } else {
        Value::Null
    };

    let mut locator = Locator {
        repo: &ctx.repo,
        head: Some(head.clone()),
        diffs: HashMap::new(),
    };
    let mut threads = thread_views(target, &mut locator);
    for view in &mut threads.open {
        view["target"] = json!(target.map(|_| &scope.key));
    }
    // A covering pass of another target counts here, so its open threads show here too.
    let mut shown = HashSet::new();
    for other in others.iter().filter(|other| {
        covered
            .iter()
            .chain(&worktree_covered)
            .any(|covered| covered.target == other.key)
            && shown.insert(&other.key)
    }) {
        let views = thread_views(Some(other), &mut locator);
        threads.open.extend(views.open.into_iter().map(|mut view| {
            view["target"] = json!(other.key);
            view
        }));
        threads.held.extend(views.held);
    }
    let records: Vec<&ImportedRecord> = target
        .iter()
        .flat_map(|target| &target.records)
        .filter(|record| {
            record.head == head || (record.head.len() >= 7 && head.starts_with(&record.head))
        })
        // Records of our own runs, or of runs covering the head, already count through their
        // passes.
        .filter(|record| {
            !target.is_some_and(|target| target.runs.iter().any(|run| run.id == record.run))
                && !covered.iter().any(|covered| covered.run == record.run)
        })
        .collect();
    let imported: u32 = records.iter().map(|record| record.reviews_at_head).sum();
    let local = count(&covered);
    // Without a pass covering the whole head, each commit since the merge-base must be reviewed.
    let every_commit = !commit_views.is_empty() && unreviewed.is_empty();
    Ok(json!({
        "repo": ctx.repo_key,
        "target": target.map(|_| &scope.key),
        "review": target.and_then(|target| target.review.as_ref()),
        "kind": scope.kind,
        "branch": scope.branch,
        "base": scope.base,
        "head": head,
        "head_tree": head_tree,
        "reviewed": !covered.is_empty() || imported > 0 || every_commit,
        "reviews_at_head": local + usize::try_from(imported).unwrap_or(usize::MAX),
        "last_reviewed_head": last_reviewed,
        "review_from": review_from,
        "covered_by": coverage_json(&covered),
        "imported_records": records,
        "worktree": worktree,
        "commits": commit_views,
        "unreviewed_commits": unreviewed,
        "open_threads": threads.open,
        "outdated_threads": threads.outdated,
        "held_questions": threads.held,
        "runs": run_views(target),
    }))
}

/// `status` without the per-commit list, runs, imported records and tree ids; `--full`
/// prints them.
fn brief_status(mut status: Value) -> Value {
    if let Some(fields) = status.as_object_mut() {
        for field in ["commits", "runs", "head_tree"] {
            fields.remove(field);
        }
        if let Some(worktree) = fields.get_mut("worktree").and_then(Value::as_object_mut) {
            worktree.remove("tree");
        }
    }
    status
}

/// `status` for a branch target with no branch, PR/MR head or pass: nothing is reviewed and
/// local anchors have no current place.
fn status_without_head(ctx: &Ctx, scope: &Scope) -> Value {
    let target = scope.existing.as_ref();
    let threads = thread_views(
        target,
        &mut Locator {
            repo: &ctx.repo,
            head: None,
            diffs: HashMap::new(),
        },
    );
    json!({
        "repo": ctx.repo_key,
        "target": target.map(|_| &scope.key),
        "review": target.and_then(|target| target.review.as_ref()),
        "kind": scope.kind,
        "branch": scope.branch,
        "base": scope.base,
        "head": null,
        "head_tree": null,
        "reviewed": false,
        "reviews_at_head": 0,
        "last_reviewed_head": null,
        "review_from": null,
        "covered_by": [],
        "imported_records": [],
        "worktree": null,
        "commits": [],
        "unreviewed_commits": [],
        "open_threads": threads.open,
        "outdated_threads": threads.outdated,
        "held_questions": threads.held,
        "runs": run_views(target),
    })
}

/// The reviewed head nearest to `head` among its ancestors: whole-branch pass heads and
/// repaired heads of this repository's branch and range targets. It scopes the next review,
/// such as on the default branch after a merge.
fn last_reviewed_head(ctx: &Ctx, head: &str) -> Result<Option<String>> {
    let mut best: Option<(u64, String)> = None;
    let targets = ctx.store.targets(&ctx.dir)?;
    let candidates = targets
        .iter()
        .filter(|target| matches!(target.kind, TargetKind::Branch | TargetKind::Range))
        .flat_map(|target| &target.runs)
        .flat_map(|run| {
            run.passes
                .iter()
                .filter(|pass| pass.ended_at.is_some() && !pass.dirty && !pass.partial)
                .map(|pass| &pass.head)
                // A run of only partial passes repaired a delta, not the branch.
                .chain(
                    run.repaired_head
                        .iter()
                        .filter(|_| run.passes.iter().any(|pass| !pass.partial)),
                )
        });
    for candidate in candidates {
        if best.as_ref().is_some_and(|(_, sha)| sha == candidate)
            || !ctx.repo.object_exists(&format!("{candidate}^{{commit}}"))?
            || !ctx.repo.is_ancestor(candidate, head)?
        {
            continue;
        }
        let distance = ctx.repo.distance(candidate, head)?;
        if best.as_ref().is_none_or(|(nearest, _)| distance < *nearest) {
            best = Some((distance, candidate.clone()));
        }
    }
    Ok(best.map(|(_, sha)| sha))
}

/// Merge commits whose tree is not git's automatic merge of their parents: a conflict
/// resolution or another hand edit nobody reviewed. Octopus merges always count.
fn changed_merges<'a>(repo: &Repo, history: &'a [LogEntry]) -> Result<Vec<&'a LogEntry>> {
    let mut changed = Vec::new();
    for entry in history.iter().filter(|entry| entry.parents.len() > 1) {
        let automatic = match entry.parents.as_slice() {
            [ours, theirs] => repo.merge_tree(ours, theirs)?,
            _ => None,
        };
        if automatic.as_deref() != Some(entry.tree.as_str()) {
            changed.push(entry);
        }
    }
    Ok(changed)
}

struct ThreadViews {
    open: Vec<Value>,
    outdated: Vec<String>,
    held: Vec<String>,
}

fn thread_views(target: Option<&Target>, locator: &mut Locator) -> ThreadViews {
    let mut views = ThreadViews {
        open: Vec::new(),
        outdated: Vec::new(),
        held: Vec::new(),
    };
    for thread in target.iter().flat_map(|target| &target.threads) {
        let current = locator.current(thread);
        if current["outdated"] == true {
            views.outdated.push(thread.id.clone());
        }
        if !is_open(thread) {
            continue;
        }
        if thread.held {
            views.held.push(thread.id.clone());
        }
        views.open.push(json!({
            "id": thread.id,
            "type": thread.kind,
            "severity": thread.severity,
            "triage": thread.triage,
            "held": thread.held,
            "current": current,
            "post": thread.comments.first().map(|first| first.post),
            "remote_id": thread.remote.as_ref().map(|remote| &remote.id),
            "url": thread.remote.as_ref().and_then(|remote| remote.url.as_ref()),
        }));
    }
    views
}

fn run_views(target: Option<&Target>) -> Vec<Value> {
    target
        .iter()
        .flat_map(|target| &target.runs)
        .map(|run| {
            json!({
                "id": run.id,
                "started_at": run.started_at,
                "ended_at": run.ended_at,
                "passes": run.passes.iter().map(|pass| &pass.id).collect::<Vec<_>>(),
                "repaired_head": run.repaired_head,
                "record": run.record,
            })
        })
        .collect()
}

/// Open review items: unresolved findings and threads still on the host. A general comment or
/// review body counts only when it is ours: written here, or posted by `publish` (it carries an
/// `agent-review` marker). Other people's, and my own unmarked ones, have no resolved state.
fn is_open(thread: &Thread) -> bool {
    let ours = thread
        .comments
        .first()
        .is_some_and(|first| match &first.remote {
            None => true,
            Some(remote) => remote.is_me && sync::agent_id(&first.body).is_some(),
        });
    // An approval is a statement, never something left to resolve.
    thread.approval.is_none()
        && !thread.resolved
        && !thread.deleted_remote
        && (thread.finding.is_some()
            || thread
                .remote
                .as_ref()
                .is_none_or(|remote| remote.resolvable)
            || ours)
}

/// Age reference for pruning: when the PR/MR closed, else the last change.
fn last_change(target: &Target) -> Result<i64> {
    let closed = target
        .review
        .as_ref()
        .filter(|review| review.state != ReviewState::Open)
        .and_then(|review| review.closed_at.as_deref());
    // A target touched after its PR/MR closed (a new run on a reused branch) ages from then.
    let updated = clock::parse(&target.updated_at)?;
    Ok(match closed {
        Some(closed) => updated.max(clock::parse(closed)?),
        None => updated,
    })
}

fn prune_dir(store: &Store, dir: &Path, now: i64, dry_run: bool) -> Result<Vec<Value>> {
    let mut pruned = Vec::new();
    for target in store.targets(dir)? {
        if now - last_change(&target)? < PRUNE_AFTER_SECS {
            continue;
        }
        let removed = dry_run
            || store.remove_if(dir, &target.key, |current| {
                Ok(now - last_change(current)? >= PRUNE_AFTER_SECS)
            })?;
        if removed {
            pruned.push(json!({
                "repo": store.relative(dir),
                "target": target.key,
                "updated_at": target.updated_at,
                "closed_at": target.review.as_ref().and_then(|review| review.closed_at.as_ref()),
            }));
        }
    }
    Ok(pruned)
}

fn prune_all(store: &Store, dry_run: bool) -> Result<Value> {
    let now = clock::now_secs()?;
    let mut pruned = Vec::new();
    for dir in store.repo_dirs()? {
        pruned.extend(prune_dir(store, &dir, now, dry_run)?);
    }
    Ok(json!({ "dry_run": dry_run, "pruned": pruned }))
}
