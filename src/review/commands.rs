use std::collections::HashSet;
use std::path::PathBuf;

use crate::core::error::{Error, Result, fail};
use crate::core::output::Output;
use crate::git::Repo;
use crate::review::cli::{
    AuthorArgs, Cli, Command, FindingAction, PassAction, RunAction, StatusCli,
};
use crate::store::Store;
use crate::store::model::{
    KilledFinding, Pass, PassKind, Reveal, Run, Severity, Target, TargetKind, Thread, ThreadType,
};

mod approve;
mod auto;
mod findings;
mod input;
pub mod prune;
mod publish;
mod qa;
mod report;
mod run;
mod scope;
mod status;
mod sync;
mod usage;
mod view;
use self::approve::approve;
use self::findings::{comment, finding_kill, reply, resolve};
pub use self::qa::run_qa;
use self::report::Section;
use self::run::{pass_end, pass_start, run_end, run_start};
use self::status::{brief_status, status};
use self::view::{list, show};

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
    /// The default branch the base was taken against: `--default-branch`, else the target's.
    default_branch: Option<String>,
    existing: Option<Target>,
}

pub fn run(cli: Cli) -> Result<Output> {
    run_with(cli, Section::Review)
}

/// `agt status`: the review command's status over review and QA together.
pub fn run_status(cli: StatusCli) -> Result<Output> {
    run_with(
        Cli {
            target: cli.target,
            range: cli.range,
            default_branch: cli.default_branch,
            command: Command::Status {
                head: cli.head,
                full: cli.full,
                sync: cli.sync,
                report: cli.report,
            },
        },
        Section::All,
    )
}

fn run_with(cli: Cli, section: Section) -> Result<Output> {
    let selection = Selection {
        target: cli.target,
        range: cli.range,
        default_branch: cli.default_branch,
    };
    let approving = matches!(
        &cli.command,
        Command::Publish { approval, .. } if approval.approve
    );
    let read_only = matches!(
        cli.command,
        Command::Status { .. } | Command::List(_) | Command::Show { .. }
    ) || matches!(cli.command, Command::Publish { dry_run: true, .. })
        && !approving;
    // Publish writes no git objects and needs `merge-tree` for its records' coverage.
    let scratch_git = read_only || matches!(cli.command, Command::Publish { .. });
    if matches!(cli.command, Command::Sync { .. } | Command::Publish { .. }) {
        crate::forge::auto::Offline::current().forbid("review sync and publish")?;
    }
    let sync_first = match &cli.command {
        Command::Status { sync, .. } => Some(sync),
        Command::List(args) => Some(&args.sync),
        _ => None,
    }
    .filter(|first| first.sync);
    if sync_first.is_some() {
        crate::forge::auto::Offline::current().forbid("review status and list --sync")?;
    }
    let preview = matches!(cli.command, Command::Publish { dry_run: true, .. }) && !approving;
    // `--sync` writes through its own repository so the read itself stays read-only.
    if let Some(first) = sync_first {
        let writer = Ctx::new(selection.clone(), false, false, false)?;
        sync::sync(&writer, first.pr.as_deref())?;
    }
    let mut ctx = open_ctx(
        selection,
        (read_only, scratch_git, preview),
        run_hint(&cli.command),
    )?;
    let listing = matches!(&cli.command, Command::Status { report, .. } if report.all);
    if let Command::Status { report, .. } = &cli.command {
        report::select(&mut ctx, report)?;
    }
    // Pure reads pull the PR/MR first when the local copy is stale.
    if !listing
        && matches!(
            cli.command,
            Command::Status { .. } | Command::List(_) | Command::Show { .. }
        )
    {
        auto::sync_stale(&ctx, crate::forge::auto::Offline::current())?;
    }
    let status_selection = ctx.selection.clone();
    let value = match cli.command {
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
            let usage = usage::read_role_usage_opt(usage.as_deref())?;
            let mut ended = run_end(&ctx, &run, reveal, head.as_deref(), usage)?;
            auto::publish_ended(&mut ctx, &run, crate::forge::auto::Offline::current());
            // The status the caller would check next, from a read-only repository so
            // merge checks write no objects.
            let mut reader_selection = status_selection;
            if reader_selection.target.is_some()
                && let Some(target) = ctx
                    .store
                    .targets(&ctx.dir)?
                    .into_iter()
                    .find(|target| target.runs.iter().any(|known| known.id == run))
            {
                reader_selection.target = Some(target.key);
            }
            let reader = Ctx::new(reader_selection, true, true, false)?;
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
        } => match run {
            Some(run) => pass_start(
                &ctx,
                &run,
                kind,
                base.as_deref(),
                reviewers.as_deref(),
                head.as_deref(),
            )?,
            None if kind == PassKind::Full => {
                let started = run_start(&ctx)?;
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
            None => return fail("--run is required unless --kind is full, which starts the run"),
        },
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
            action: FindingAction::Kill { id, reason },
        } => finding_kill(&ctx, &id, &reason)?,
        Command::Comment(args) => comment(&ctx, &args, false, None)?,
        Command::Reply(args) => reply(&ctx, &args, false)?,
        Command::Resolve(args) => resolve(&ctx, &args)?,
        Command::List(args) => return list(&ctx, &args),
        Command::Show { id } => show(&ctx, &id)?,
        Command::Status {
            head, full, report, ..
        } => return report::run(&mut ctx, &report, section, head.as_deref(), full),
        Command::Sync { pr } => sync::sync(&ctx, pr.as_deref())?,
        Command::Publish {
            dry_run,
            not_own,
            only,
            pr,
            approval,
        } => {
            let mut approval_id = None;
            if !approval.approve && (approval.author.is_set()) {
                return fail("author flags on publish need --approve");
            }
            if approval.approve {
                let approved = approve(&ctx, &approval)?;
                approval_id = approved["thread"]["id"].as_str().map(str::to_owned);
            }
            publish::publish(
                &mut ctx,
                pr.as_deref(),
                &publish::Options {
                    dry_run,
                    not_own,
                    only,
                    approval: approval_id,
                    run: None,
                },
            )?
        }
    };
    Ok(Output::Json(value))
}

/// A [`Ctx`] on `selection` as `(read_only, scratch_git, preview)` say, on the target of `run`
/// when one is given.
fn open_ctx(
    selection: Selection,
    (read_only, scratch_git, preview): (bool, bool, bool),
    run: Option<String>,
) -> Result<Ctx> {
    let mut ctx = Ctx::new(selection, read_only, scratch_git, preview)?;
    if let Some(run) = run {
        ctx.select_run_target(&run)?;
    }
    Ok(ctx)
}

/// The run an id `<run>` or `<run>.<rest>` names, when `is_run` accepts the run.
fn run_of_id(id: &str, is_run: fn(&str) -> bool) -> Option<String> {
    let run = id.split('.').next()?;
    is_run(run).then(|| run.to_owned())
}

/// The run a command names, through `--run` or an id that starts with one (`rv-xxxx...`).
fn run_hint(command: &Command) -> Option<String> {
    let from_id = |id: &str| run_of_id(id, is_run_id);
    match command {
        Command::Run {
            action: RunAction::End { run, .. },
        }
        | Command::Pass {
            action: PassAction::End { run, .. },
        } => Some(run.clone()),
        Command::Pass {
            action: PassAction::Start { run, .. },
        } => run.clone(),
        Command::Finding {
            action: FindingAction::Kill { id, .. },
        }
        | Command::Show { id } => from_id(id),
        Command::Reply(args) => author_run(&args.author).or_else(|| from_id(&args.id)),
        Command::Resolve(args) => {
            author_run(&args.author).or_else(|| args.ids.first().and_then(|id| from_id(id)))
        }
        Command::Publish { approval, .. } => author_run(&approval.author),
        Command::List(args) => args.run.clone(),
        _ => None,
    }
}

/// The run of an agent author's `--reviewer <run>.<pass>.<reviewer>`.
fn author_run(author: &AuthorArgs) -> Option<String> {
    let run = author.reviewer.as_deref()?.split('.').next()?;
    is_run_id(run).then(|| run.to_owned())
}

fn passes(target: &Target) -> impl Iterator<Item = &Pass> {
    target.runs.iter().flat_map(|run| run.passes.iter())
}

fn killed(target: &Target) -> impl Iterator<Item = &KilledFinding> {
    passes(target).flat_map(|pass| pass.killed.iter())
}

fn pass_patch_ids(pass: &Pass) -> impl Iterator<Item = &String> {
    pass.commits
        .iter()
        .filter_map(|commit| commit.patch_id.as_ref())
        .chain(pass.range_patch_id.as_ref())
        .chain(pass.head_patch_id.as_ref())
}

/// A joined list of `names`, or `none` when there are none.
fn list_or_none<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<&str> = names.collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

/// A fresh id from `make` that is neither in `taken` nor already in the target (`in_target`).
fn fresh_id(
    taken: &HashSet<String>,
    in_target: impl Fn(&str) -> bool,
    make: impl Fn() -> String,
) -> String {
    loop {
        let id = make();
        if !taken.contains(&id) && !in_target(&id) {
            return id;
        }
    }
}

/// Names the open runs of kind `kind` (`run`, `QA run`), so a wrong id costs no second lookup.
fn run_not_found<'a>(
    kind: &str,
    id: &str,
    target: &Target,
    open: impl Iterator<Item = &'a str>,
) -> Error {
    Error(format!(
        "{kind} {id} not found in target {}; open {kind}s: {}",
        target.key,
        list_or_none(open)
    ))
}

/// Names the open runs, so a wrong id costs no second lookup.
fn run_missing(target: &Target, id: &str) -> Error {
    let open = target
        .runs
        .iter()
        .filter(|run| run.ended_at.is_none())
        .map(|run| run.id.as_str());
    run_not_found("run", id, target, open)
}

/// Names the run's passes, so a wrong id costs no second lookup.
fn pass_missing(run: &Run, pass: &str) -> Error {
    Error(format!(
        "pass {}.{pass} not found; its passes: {}",
        run.id,
        list_or_none(run.passes.iter().map(|pass| pass.id.as_str()))
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
        .thread_mut(id)
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

const RUN_ID_PREFIX: &str = "rv-";
const RUN_ID_CHARS: usize = 4;

fn is_run_id(text: &str) -> bool {
    text.len() == RUN_ID_PREFIX.len() + RUN_ID_CHARS && text.starts_with(RUN_ID_PREFIX)
}

fn random_id() -> String {
    crate::core::random::token(RUN_ID_PREFIX, RUN_ID_CHARS)
}
