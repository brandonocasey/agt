use std::path::PathBuf;

use crate::core::error::{Error, Result};
use crate::core::output::Output;
use crate::git::Repo;
use crate::review::cli::{Cli, Command, FindingAction, PassAction, RunAction};
use crate::store::Store;
use crate::store::model::{
    Pass, Reveal, RoleUsage, Run, Severity, Target, TargetKind, Thread, ThreadType,
};

mod approve;
mod auto;
mod findings;
mod input;
pub mod prune;
mod publish;
mod run;
mod scope;
mod status;
mod sync;
mod usage;
mod view;
use self::approve::approve;
use self::findings::{comment, finding_add, finding_kill, reply, resolve};
use self::run::{pass_end, pass_start, run_end, run_start};
use self::status::{brief_status, status};
use self::usage::UsageInput;
use self::view::{list, show};
use crate::core::local::read_source;

const MAX_TEXT: usize = 65_536;

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
    // Publish writes no git objects and needs `merge-tree` for its records' coverage.
    let scratch_git = read_only || matches!(cli.command, Command::Publish { .. });
    if crate::forge::auto::is_offline(cli.offline)
        && matches!(cli.command, Command::Sync { .. } | Command::Publish { .. })
    {
        return crate::core::error::fail("offline mode forbids review sync and publish");
    }
    let mut ctx = Ctx::new(
        selection,
        read_only,
        scratch_git,
        matches!(cli.command, Command::Publish { dry_run: true, .. }),
    )?;
    if let Some(run) = run_hint(&cli.command) {
        ctx.select_run_target(&run)?;
    }
    // Pure reads pull the PR/MR first when the local copy is stale.
    if matches!(
        cli.command,
        Command::Status { .. } | Command::List(_) | Command::Show { .. }
    ) {
        auto::sync_stale(&ctx, cli.offline)?;
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
                        serde_json::from_str(&read_source(&source, "usage")?).map_err(
                            crate::core::error::ctx("parse usage (expected a JSON array)"),
                        )?;
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
            auto::publish_ended(&mut ctx, &run, cli.offline);
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
            &mut ctx,
            pr.as_deref(),
            &publish::Options {
                dry_run,
                not_own,
                findings,
                only,
                run: None,
            },
        )?,
    };
    Ok(Output::Json(value))
}

/// The run a command names, through `--run` or an id that starts with one (`rv-xxxx...`).
fn run_hint(command: &Command) -> Option<String> {
    let from_id = |id: &str| {
        let run = id.split('.').next()?;
        is_run_id(run).then(|| run.to_owned())
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

const RUN_ID_PREFIX: &str = "rv-";
const RUN_ID_CHARS: usize = 4;

fn is_run_id(text: &str) -> bool {
    text.len() == RUN_ID_PREFIX.len() + RUN_ID_CHARS && text.starts_with(RUN_ID_PREFIX)
}

fn random_id() -> String {
    crate::core::random::token(RUN_ID_PREFIX, RUN_ID_CHARS)
}
