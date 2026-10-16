use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::anchors::{self, FileDiff, Location};
use crate::cli::{
    AuthorArgs, Cli, Command, CommentArgs, FindingAction, Format, ListArgs, PassAction, ReplyArgs,
    ResolveArgs, RunAction,
};
use crate::clock;
use crate::error::{Error, Result, ctx, fail};
use crate::git::{CommitInfo, Repo};
use crate::model::{
    Anchor, Author, Comment, CommitPatch, FindingInfo, ImportedRecord, KilledFinding, Pass,
    PassKind, PostState, ReviewState, Reviewer, Run, Severity, Side, Target, TargetKind, Thread,
    ThreadType, Triage, VERSION,
};
use crate::store::Store;

mod sync;

const PRUNE_AFTER_SECS: i64 = 90 * 24 * 60 * 60;
const MAX_TEXT: usize = 65_536;

pub enum Output {
    Json(Value),
    Text(String),
}

struct Selection {
    target: Option<String>,
    range: Option<String>,
    default_branch: Option<String>,
}

struct Ctx {
    repo: Repo,
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
        Command::Status { .. } | Command::List(_) | Command::Show { .. }
    );
    if let Command::Prune { all: true, dry_run } = cli.command {
        return prune_all(&store, dry_run).map(Output::Json);
    }
    let ctx = Ctx::new(store, selection, read_only)?;
    let value = match cli.command {
        Command::Run {
            action: RunAction::Start,
        } => run_start(&ctx)?,
        Command::Run {
            action: RunAction::End { run },
        } => run_end(&ctx, &run)?,
        Command::Pass {
            action:
                PassAction::Start {
                    run,
                    kind,
                    base,
                    reviewers,
                },
        } => pass_start(&ctx, &run, kind, base.as_deref(), reviewers.as_deref())?,
        Command::Pass {
            action: PassAction::End { run, pass, lock },
        } => pass_end(&ctx, &run, &pass, lock)?,
        Command::Finding {
            action:
                FindingAction::Add {
                    run,
                    pass,
                    reviewer,
                },
        } => finding_add(&ctx, &run, &pass, &reviewer)?,
        Command::Comment(args) => comment(&ctx, &args)?,
        Command::Reply(args) => reply(&ctx, &args)?,
        Command::Resolve(args) => resolve(&ctx, &args)?,
        Command::List(args) => return list(&ctx, &args),
        Command::Show { id } => show(&ctx, &id)?,
        Command::Status { head } => status(&ctx, head.as_deref())?,
        Command::Sync { pr } => sync::sync(&ctx, pr.as_deref())?,
        Command::Prune { dry_run, .. } => {
            let pruned = prune_dir(&ctx.store, &ctx.dir, clock::now_secs()?, dry_run)?;
            json!({ "dry_run": dry_run, "pruned": pruned })
        }
    };
    Ok(Output::Json(value))
}

impl Ctx {
    fn new(store: Store, selection: Selection, read_only: bool) -> Result<Self> {
        let mut repo = Repo::discover()?;
        if read_only {
            repo = repo.read_only()?;
        }
        let repo_key = repo.repo_key()?;
        let dir = store.repo_dir(&repo_key)?;
        Ok(Self {
            repo,
            store,
            repo_key,
            dir,
            selection,
        })
    }

    /// Author default: the forge login cached by `sync`, else git `user.name`.
    fn default_login(&self) -> Result<Option<String>> {
        match self.store.load_account(&self.dir)? {
            Some(account) => Ok(Some(account.login)),
            None => self.repo.user_name(),
        }
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
        let (base, head) = match target.kind {
            TargetKind::Branch => {
                let branch = target.branch.clone().unwrap_or_default();
                let head = match self.repo.commit(&format!("refs/heads/{branch}")) {
                    Ok(head) => head,
                    Err(_) => self.repo.head()?,
                };
                (Some(self.branch_base(&branch, &head)?.0), head)
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
        let mut key = format!("branch-{}-{key_base}", encode_key(&branch));
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
            existing,
        })
    }

    /// Returns the review base and the commit that names the target. On the default branch
    /// itself the merge-base is the head, so the root commit names the target instead.
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

    /// The reviewed state: the work tree for a branch (possibly uncommitted), else the head.
    fn reviewed_state(&self, scope: &Scope) -> Result<(Option<String>, String)> {
        let head_tree = self.repo.tree_of(&scope.head)?;
        if scope.kind != TargetKind::Branch {
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

fn find_run_mut<'a>(target: &'a mut Target, id: &str) -> Result<&'a mut Run> {
    target
        .runs
        .iter_mut()
        .find(|run| run.id == id)
        .ok_or_else(|| Error(format!("run {id} not found in target {}", target.key)))
}

fn find_pass<'a>(target: &'a Target, run: &str, pass: &str) -> Result<&'a Pass> {
    target
        .runs
        .iter()
        .find(|candidate| candidate.id == run)
        .ok_or_else(|| Error(format!("run {run} not found in target {}", target.key)))?
        .passes
        .iter()
        .find(|candidate| candidate.id == pass)
        .ok_or_else(|| Error(format!("pass {run}.{pass} not found")))
}

fn find_pass_mut<'a>(target: &'a mut Target, run: &str, pass: &str) -> Result<&'a mut Pass> {
    find_run_mut(target, run)?
        .passes
        .iter_mut()
        .find(|candidate| candidate.id == pass)
        .ok_or_else(|| Error(format!("pass {run}.{pass} not found")))
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

fn local_comment(author: Author, body: String, now: &str) -> Comment {
    Comment {
        author,
        body,
        created_at: now.to_owned(),
        post: PostState::Pending,
        remote: None,
        draft_id: None,
    }
}

fn author(args: &AuthorArgs, target: &Target, login: Option<&str>) -> Result<Author> {
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
        });
    }
    if !agent {
        return login
            .map(|login| Author::Human {
                login: login.to_owned(),
            })
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
    let run = ctx.update(&scope, |target, now| {
        let id = loop {
            let id = random_id();
            if !target.runs.iter().any(|run| run.id == id) {
                break id;
            }
        };
        target.runs.push(Run {
            id: id.clone(),
            started_at: now.to_owned(),
            ended_at: None,
            passes: Vec::new(),
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
                let from = if self.repo.is_ancestor(&pass.head, &scope.head)? {
                    Some(pass.head.clone())
                } else {
                    scope.base.clone()
                };
                let commits = self.repo.commits(from.as_deref(), &scope.head)?;
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

fn run_end(ctx: &Ctx, run_id: &str) -> Result<Value> {
    let scope = ctx.scope()?;
    if scope.existing.is_none() {
        return fail(format!("run {run_id} not found in target {}", scope.key));
    }
    let adoption = ctx.adoption(&scope)?;
    let (ended_at, recorded) = ctx.update(&scope, |target, now| {
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
        Ok((now.to_owned(), adoption.apply(target)))
    })?;
    let pruned = prune_dir(&ctx.store, &ctx.dir, clock::now_secs()?, false)?;
    Ok(json!({
        "target": scope.key,
        "run": run_id,
        "ended_at": ended_at,
        "recorded": recorded,
        "pruned": pruned,
    }))
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
                if reviewer_number(&id).is_none() {
                    return fail(format!("reviewer id must look like r<n>: {id}"));
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
) -> Result<Value> {
    let scope = ctx.scope()?;
    let inputs = read_reviewers(reviewers)?;
    let (commit, tree) = ctx.reviewed_state(&scope)?;
    let previous_head = scope.existing.as_ref().and_then(|target| {
        target
            .runs
            .iter()
            .find(|run| run.id == run_id)
            .and_then(|run| run.passes.last())
            .map(|pass| pass.head.clone())
    });
    let base = match (base, kind) {
        (Some(rev), _) => Some(ctx.repo.commit(rev)?),
        (None, PassKind::Full) => scope.base.clone(),
        (None, PassKind::FixCheck | PassKind::Reveal) => {
            previous_head.or_else(|| scope.base.clone())
        }
    };
    let commits = ctx.repo.commits(base.as_deref(), &scope.head)?;
    let range_patch_id = ctx.repo.range_patch_id(scope.base.as_deref(), &tree)?;
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
    Ok(json!({ "target": scope.key, "run": run_id, "pass": pass }))
}

fn pass_end(ctx: &Ctx, run_id: &str, pass_id: &str, lock: bool) -> Result<Value> {
    let scope = ctx.scope()?;
    let pass = ctx.update(&scope, |target, now| {
        let major = major_found(target, run_id, pass_id);
        let pass = find_pass_mut(target, run_id, pass_id)?;
        if pass.ended_at.is_some() {
            return fail(format!("pass {run_id}.{pass_id} already ended"));
        }
        pass.ended_at = Some(now.to_owned());
        pass.locked = lock;
        pass.major_found = major;
        Ok(pass.clone())
    })?;
    Ok(json!({ "target": scope.key, "run": run_id, "pass": pass }))
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
}

fn read_findings() -> Result<Vec<FindingInput>> {
    let value: Value =
        serde_json::from_str(&read_stdin("findings")?).map_err(ctx("parse findings JSON"))?;
    let items = match value {
        Value::Array(items) => items,
        item @ Value::Object(_) => vec![item],
        _ => return fail("findings must be a JSON object or array"),
    };
    if items.is_empty() {
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
    let findings = read_findings()?;
    let (added, killed) = ctx.update(&scope, |target, now| {
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
            .ok_or_else(|| Error(format!("reviewer {reviewer_id} is not in pass {pass_id}")))?;
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
            threads.push(Thread {
                id,
                kind: finding.kind,
                severity: finding.severity,
                anchor,
                resolved: false,
                resolution: None,
                triage: None,
                held: false,
                finding: Some(FindingInfo {
                    run: run_id.to_owned(),
                    pass: pass_id.to_owned(),
                    reviewer: reviewer_id.to_owned(),
                    evidence: finding.evidence,
                    tests_run: finding.tests_run,
                    post_reveal: finding.post_reveal,
                }),
                remote: None,
                comments: vec![local_comment(agent.clone(), finding.scenario, now)],
                created_at: now.to_owned(),
                updated_at: now.to_owned(),
            });
        }
        let added: Vec<String> = threads.iter().map(|thread| thread.id.clone()).collect();
        let killed_ids: Vec<String> = killed.iter().map(|finding| finding.id.clone()).collect();
        target.threads.extend(threads);
        let major = major_found(target, run_id, pass_id);
        let pass = find_pass_mut(target, run_id, pass_id)?;
        pass.killed.extend(killed);
        pass.major_found = major;
        Ok((added, killed_ids))
    })?;
    Ok(json!({ "target": scope.key, "added": added, "killed": killed }))
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
    let login = ctx.default_login()?;
    let thread = ctx.update(&scope, |target, now| {
        let author = author(&args.author, target, login.as_deref())?;
        let number = next_number(target.threads.iter().map(|thread| thread.id.clone()), "h") + 1;
        let thread = Thread {
            id: format!("h{number}"),
            kind: args.kind,
            severity: args.severity,
            anchor,
            resolved: false,
            resolution: None,
            triage: None,
            held: false,
            finding: None,
            remote: None,
            comments: vec![local_comment(author, body, now)],
            created_at: now.to_owned(),
            updated_at: now.to_owned(),
        };
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
    let login = ctx.default_login()?;
    let thread = ctx.update(&scope, |target, now| {
        let author = author(&args.author, target, login.as_deref())?;
        let thread = find_thread_mut(target, &args.id)?;
        thread.comments.push(local_comment(author, body, now));
        thread.updated_at = now.to_owned();
        Ok(thread.clone())
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
        return fail(format!("thread {} not found", args.id));
    }
    let resolution = args
        .resolution
        .as_deref()
        .map(|text| normalize_resolution(ctx, text))
        .transpose()?;
    let records_only = matches!(args.triage, Some(Triage::Valid | Triage::Question));
    if !args.reopen && records_only && resolution.is_some() {
        return fail("--resolution needs a resolving triage (fixed or stale)");
    }
    let resolution = match (args.triage, resolution) {
        (Some(Triage::Stale), None) if !args.reopen => Some("stale".to_owned()),
        (Some(Triage::Fixed), None) if !args.reopen => {
            return fail("--triage fixed needs --resolution 'fixed <sha>'");
        }
        (_, resolution) => resolution,
    };
    let thread = ctx.update(&scope, |target, now| {
        let thread = find_thread_mut(target, &args.id)?;
        if args.triage.is_some() {
            thread.triage = args.triage;
        }
        if args.reopen {
            thread.resolved = false;
            thread.resolution = resolution;
            thread.held = args.triage == Some(Triage::Question);
        } else if records_only {
            thread.held = args.triage == Some(Triage::Question);
        } else {
            thread.resolved = true;
            thread.held = false;
            if resolution.is_some() {
                thread.resolution = resolution;
            }
        }
        thread.updated_at = now.to_owned();
        Ok(thread.clone())
    })?;
    Ok(json!({ "target": scope.key, "thread": thread }))
}

/// Maps anchors to the current head, caching one diff per anchor commit or tree.
struct Locator<'a> {
    repo: &'a Repo,
    head: String,
    diffs: HashMap<String, Option<Vec<FileDiff>>>,
}

impl Locator<'_> {
    /// An anchor whose commit or tree is gone (collected) counts as outdated.
    fn locate(&mut self, anchor: &Anchor) -> Location {
        let from = anchor.commit.clone().unwrap_or_else(|| anchor.tree.clone());
        if from == self.head {
            return anchors::unchanged(anchor);
        }
        let repo = self.repo;
        let head = &self.head;
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
            let location = self.locate(anchor);
            return json!({ "path": location.path, "line": location.line, "outdated": location.outdated });
        }
        match &thread.remote {
            Some(remote) if remote.path.is_some() => json!({
                "path": remote.path,
                "line": remote.line.or(remote.old_line).or(remote.original_line),
                "outdated": remote.outdated.unwrap_or(false),
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
        args.run
            .as_ref()
            .is_none_or(|run| id.starts_with(&format!("{run}.")))
    };
    let selected: Vec<&Thread> = threads
        .iter()
        .filter(|thread| !args.open || !thread.resolved)
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
        head: scope.head.clone(),
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
        Author::Human { login } => login.clone(),
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
        head: scope.head.clone(),
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

/// Ended passes whose reviewed state matches `tree`, the whole-range patch-id or the head
/// commit's patch-id.
fn coverage<'a>(
    target: Option<&'a Target>,
    tree: &str,
    range_patch_id: Option<&str>,
    head_patch_id: Option<&str>,
) -> Vec<(String, &'a Pass, Vec<&'static str>)> {
    let mut covered = Vec::new();
    for run in target.iter().flat_map(|target| &target.runs) {
        for pass in run.passes.iter().filter(|pass| pass.ended_at.is_some()) {
            let mut matched = Vec::new();
            if pass.tree == tree {
                matched.push("tree");
            }
            if range_patch_id.is_some() && pass.range_patch_id.as_deref() == range_patch_id {
                matched.push("range-patch-id");
            }
            if let Some(id) = head_patch_id
                && (pass.head_patch_id.as_deref() == Some(id)
                    || pass.range_patch_id.as_deref() == Some(id))
            {
                matched.push("head-patch-id");
            }
            if !matched.is_empty() {
                covered.push((run.id.clone(), pass, matched));
            }
        }
    }
    covered
}

fn coverage_json(covered: &[(String, &Pass, Vec<&str>)]) -> Value {
    covered
        .iter()
        .map(|(run, pass, matched)| {
            json!({
                "run": run,
                "pass": pass.id,
                "kind": pass.kind,
                "major_found": pass.major_found,
                "match": matched,
            })
        })
        .collect()
}

/// Full reviews plus clean fix checks; parallel reviewers in one pass count once.
fn count(covered: &[(String, &Pass, Vec<&str>)]) -> usize {
    covered
        .iter()
        .filter(|(_, pass, _)| {
            pass.kind == PassKind::Full || (pass.kind == PassKind::FixCheck && !pass.major_found)
        })
        .count()
}

fn status(ctx: &Ctx, head_arg: Option<&str>) -> Result<Value> {
    let mut scope = ctx.scope()?;
    if let Some(rev) = head_arg {
        scope.head = ctx.repo.commit(rev)?;
    }
    let target = scope.existing.as_ref();
    let head = scope.head.clone();
    let head_tree = ctx.repo.tree_of(&head)?;
    let commits = ctx.repo.commits(scope.base.as_deref(), &head)?;
    let head_patch_id = match commits.iter().find(|commit| commit.sha == head) {
        Some(commit) => commit.patch_id.clone(),
        None => None,
    };
    let range_patch_id = ctx.repo.range_patch_id(scope.base.as_deref(), &head)?;
    let covered = coverage(
        target,
        &head_tree,
        range_patch_id.as_deref(),
        head_patch_id.as_deref(),
    );

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
    let commit_views: Vec<Value> = commits
        .iter()
        .map(|commit| {
            let ok = reviewed.shas.contains(&commit.sha)
                || reviewed.trees.contains(&commit.tree)
                || commit
                    .patch_id
                    .as_ref()
                    .is_some_and(|id| reviewed.patch_ids.contains(id));
            json!({
                "sha": commit.sha,
                "subject": commit.subject,
                "patch_id": commit.patch_id,
                "reviewed": ok,
            })
        })
        .collect();
    let unreviewed: Vec<&Value> = commit_views
        .iter()
        .filter(|view| view["reviewed"] == false)
        .map(|view| &view["sha"])
        .collect();

    let worktree = if head_arg.is_none() && scope.kind == TargetKind::Branch {
        let tree = ctx.repo.worktree_tree()?;
        let dirty = tree != head_tree;
        let covered = if dirty {
            let range = ctx.repo.range_patch_id(scope.base.as_deref(), &tree)?;
            coverage(target, &tree, range.as_deref(), None)
        } else {
            covered.clone()
        };
        json!({
            "dirty": dirty,
            "tree": tree,
            "reviewed": !covered.is_empty(),
            "reviews": count(&covered),
            "covered_by": coverage_json(&covered),
        })
    } else {
        Value::Null
    };

    let mut locator = Locator {
        repo: &ctx.repo,
        head: head.clone(),
        diffs: HashMap::new(),
    };
    let mut open = Vec::new();
    let mut outdated = Vec::new();
    let mut held = Vec::new();
    for thread in target.iter().flat_map(|target| &target.threads) {
        let current = locator.current(thread);
        if current["outdated"] == true {
            outdated.push(thread.id.clone());
        }
        if !is_open(thread) {
            continue;
        }
        if thread.held {
            held.push(thread.id.clone());
        }
        open.push(json!({
            "id": thread.id,
            "type": thread.kind,
            "severity": thread.severity,
            "triage": thread.triage,
            "current": current,
        }));
    }
    let runs: Vec<Value> = target
        .iter()
        .flat_map(|target| &target.runs)
        .map(|run| {
            json!({
                "id": run.id,
                "started_at": run.started_at,
                "ended_at": run.ended_at,
                "passes": run.passes.iter().map(|pass| &pass.id).collect::<Vec<_>>(),
            })
        })
        .collect();
    let records: Vec<&ImportedRecord> = target
        .iter()
        .flat_map(|target| &target.records)
        .filter(|record| {
            record.head == head || (record.head.len() >= 7 && head.starts_with(&record.head))
        })
        .collect();
    let imported: u32 = records.iter().map(|record| record.reviews_at_head).sum();
    let local = count(&covered);
    Ok(json!({
        "repo": ctx.repo_key,
        "target": target.map(|_| &scope.key),
        "review": target.and_then(|target| target.review.as_ref()),
        "kind": scope.kind,
        "branch": scope.branch,
        "base": scope.base,
        "head": head,
        "head_tree": head_tree,
        "reviewed": !covered.is_empty() || imported > 0,
        "reviews_at_head": local + usize::try_from(imported).unwrap_or(usize::MAX),
        "covered_by": coverage_json(&covered),
        "imported_records": records,
        "worktree": worktree,
        "commits": commit_views,
        "unreviewed_commits": unreviewed,
        "open_threads": open,
        "outdated_threads": outdated,
        "held_questions": held,
        "runs": runs,
    }))
}

/// Open review items: unresolved threads, except other people's general comments, which have
/// no resolved state on the host.
fn is_open(thread: &Thread) -> bool {
    !thread.resolved
        && (thread.finding.is_some()
            || thread
                .remote
                .as_ref()
                .is_none_or(|remote| remote.resolvable))
}

/// Age reference for pruning: when the PR/MR closed, else the last change.
fn last_change(target: &Target) -> Result<i64> {
    let closed = target
        .review
        .as_ref()
        .filter(|review| review.state != ReviewState::Open)
        .and_then(|review| review.closed_at.as_deref());
    clock::parse(closed.unwrap_or(&target.updated_at))
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
