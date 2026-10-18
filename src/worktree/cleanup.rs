//! `agt worktree cleanup`: remove worktrees and delete local branches whose work already
//! landed in the default branch. Never touches a remote, a PR, uncommitted work (a snapshot
//! backup keeps it) or a branch whose merge cannot be proven.

use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::{Value, json};

use super::idle::Cwds;
use super::landed::{Finding, Scene, Verdict, recent};
use super::remove::{refuse, removal};
use super::{info, nested, roots, submodules};
use crate::core::clock;
use crate::core::error::Result;
use crate::core::output::Output;
use crate::forge::landed::Origin;
use crate::git::Repo;

#[derive(Args)]
pub struct Cli {
    /// Sweep every repository under the configured roots (`[worktree] roots` in the agt
    /// config), not only the current one.
    #[arg(long)]
    all: bool,
    /// Remove and delete. Without it nothing changes and the plan is printed.
    #[arg(long)]
    yes: bool,
    /// Judge by local ancestry only; do not ask GitHub or GitLab.
    #[arg(long)]
    offline: bool,
    /// Also remove worktrees with uncommitted work; the snapshot backup keeps it.
    #[arg(long)]
    dirty: bool,
    /// Also remove worktrees where something changed in the last 3 days.
    #[arg(long)]
    recent: bool,
}

/// What a sweep of one repository found.
#[derive(Default)]
struct Report {
    remove: Vec<Value>,
    delete: Vec<Value>,
    kept: Vec<Value>,
    warnings: Vec<String>,
}

pub fn run(cli: Cli) -> Result<Output> {
    let repos: Vec<Repo> = if cli.all {
        roots::repositories()?.into_iter().map(Repo::at).collect()
    } else {
        vec![Repo::discover()?]
    };
    let mut cwds = Cwds::default();
    let mut rows = Vec::new();
    for repo in repos {
        let swept = info::main_checkout(&repo)
            .and_then(|main| sweep(&repo, &cli, &mut cwds).map(|report| (main.path, report)));
        let (main, report) = match swept {
            Ok(done) => done,
            Err(error) if cli.all => (
                repo.root().to_owned(),
                Report {
                    warnings: vec![error.to_string()],
                    ..Report::default()
                },
            ),
            Err(error) => return Err(error),
        };
        let (removed, deleted) = if cli.yes {
            ("removed", "deleted")
        } else {
            ("remove", "delete")
        };
        rows.push(json!({
            "repository": main,
            removed: report.remove,
            deleted: report.delete,
            "kept": report.kept,
            "warnings": report.warnings,
        }));
    }
    let mut out = json!({ "plan": !cli.yes, "repositories": rows });
    if !cli.yes {
        out["next"] = json!("pass --yes to remove and delete");
    }
    Ok(Output::Json(out))
}

fn sweep(repo: &Repo, cli: &Cli, cwds: &mut Cwds) -> Result<Report> {
    let mut report = Report::default();
    let Some(scene) = Scene::read(repo)? else {
        report.warnings.push("no default branch found".into());
        return Ok(report);
    };
    let origin = if cli.offline { None } else { Origin::of(repo)? };
    let main = info::real(&scene.main().path)?;
    let shared = Repo::at(&main);
    for entry in scene.entries.iter().skip(1).filter(|e| e.branch.is_none()) {
        if scene.holds_cwd(&entry.path) {
            continue;
        }
        let verdict = if scene.contains(repo, &entry.head)? {
            Finding {
                verdict: Verdict::Merged,
                evidence: format!("detached HEAD is an ancestor of {}", scene.name),
            }
        } else {
            Finding {
                verdict: Verdict::Unmerged,
                evidence: format!("detached HEAD is not an ancestor of {}", scene.name),
            }
        };
        let subject = Subject {
            branch: None,
            tip: &entry.head,
            finding: &verdict,
        };
        if verdict.verdict.landed() {
            remove_worktree(&scene, entry, &main, &subject, cli, cwds, &mut report)?;
        } else {
            report
                .kept
                .push(kept(&subject, Some(&entry.path), &verdict.evidence));
        }
    }
    for (branch, tip) in scene.branches(repo)? {
        let finding = scene.judge(repo, origin.as_ref(), &branch, &tip)?;
        let subject = Subject {
            branch: Some(&branch),
            tip: &tip,
            finding: &finding,
        };
        if !finding.verdict.landed() {
            report.kept.push(kept(
                &subject,
                scene.worktree_of(&branch).map(|e| &e.path),
                &finding.evidence,
            ));
            continue;
        }
        if let Some(entry) = scene.worktree_of(&branch)
            && !remove_worktree(&scene, entry, &main, &subject, cli, cwds, &mut report)?
        {
            continue;
        }
        delete_branch(&shared, &subject, cli.yes, &mut report)?;
    }
    Ok(report)
}

/// The branch (or detached HEAD) under judgment.
struct Subject<'a> {
    branch: Option<&'a str>,
    tip: &'a str,
    finding: &'a Finding,
}

fn kept(subject: &Subject, path: Option<&PathBuf>, reason: &str) -> Value {
    kept_as(subject.finding.verdict.as_str(), subject, path, reason)
}

fn kept_as(verdict: &str, subject: &Subject, path: Option<&PathBuf>, reason: &str) -> Value {
    json!({
        "branch": subject.branch,
        "path": path,
        "tip": subject.tip,
        "verdict": verdict,
        "reason": reason,
    })
}

/// Removes (or plans to remove) the worktree of a landed subject. `false` when it is kept.
fn remove_worktree(
    scene: &Scene,
    entry: &info::Entry,
    main: &Path,
    subject: &Subject,
    cli: &Cli,
    cwds: &mut Cwds,
    report: &mut Report,
) -> Result<bool> {
    let path = &entry.path;
    if let Some((verdict, reason)) = blocker(scene, entry, main, cli, cwds)? {
        let verdict = verdict.unwrap_or(subject.finding.verdict.as_str());
        report
            .kept
            .push(kept_as(verdict, subject, Some(path), &reason));
        return Ok(false);
    }
    let real = info::real(path)?;
    let action = format!(
        "snapshot backup, then git worktree remove --force {}",
        real.display()
    );
    if !cli.yes {
        report.remove.push(json!({
            "path": real,
            "branch": subject.branch,
            "verdict": subject.finding.verdict.as_str(),
            "evidence": subject.finding.evidence,
            "action": action,
        }));
        return Ok(true);
    }
    match removal(main, &real, entry) {
        Ok(done) => {
            report.remove.push(json!({
                "path": real,
                "branch": subject.branch,
                "id": done["id"],
                "ref": done["ref"],
                "verdict": subject.finding.verdict.as_str(),
                "evidence": subject.finding.evidence,
            }));
            Ok(true)
        }
        Err(error) => {
            report.kept.push(kept(
                subject,
                Some(path),
                &format!("removal failed: {error}"),
            ));
            Ok(false)
        }
    }
}

/// Why the worktree must stay, if it must.
fn blocker(
    scene: &Scene,
    entry: &info::Entry,
    main: &Path,
    cli: &Cli,
    cwds: &mut Cwds,
) -> Result<Option<(Option<&'static str>, String)>> {
    if !entry.path.exists() {
        return Ok(Some((
            None,
            "the directory is missing; run git worktree prune".into(),
        )));
    }
    let real = info::real(&entry.path)?;
    if let Err(error) = refuse(&scene.entries, &real, main) {
        return Ok(Some((None, error.to_string())));
    }
    if let Err(error) = submodules::check(&real, main) {
        return Ok(Some((None, error.to_string())));
    }
    let inside = nested::find(&real)?;
    if !inside.is_empty() {
        return Ok(Some((None, nested::reason(&inside))));
    }
    if let Some(reason) = cwds.busy(&real) {
        return Ok(Some((None, reason)));
    }
    if !cli.recent && recent(&real, clock::now_secs()?)? {
        return Ok(Some((
            Some("recent"),
            "something in it changed in the last 3 days; pass --recent to remove it anyway".into(),
        )));
    }
    let unsaved = info::unsaved(&real)?;
    if unsaved > 0 && !cli.dirty {
        return Ok(Some((
            None,
            format!(
                "dirty: {unsaved} modified or untracked files; pass --dirty to remove it after a snapshot backup"
            ),
        )));
    }
    Ok(None)
}

fn delete_branch(repo: &Repo, subject: &Subject, yes: bool, report: &mut Report) -> Result<()> {
    let Some(branch) = subject.branch else {
        return Ok(());
    };
    let flag = if subject.finding.verdict == Verdict::Merged {
        "-d"
    } else {
        "-D"
    };
    let mut row = json!({
        "branch": branch,
        "tip": subject.tip,
        "verdict": subject.finding.verdict.as_str(),
        "evidence": subject.finding.evidence,
    });
    if !yes {
        row["action"] = json!(format!("git branch {flag} {branch}"));
        report.delete.push(row);
        return Ok(());
    }
    let now = repo.git_opt(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/heads/{branch}"),
    ])?;
    if now.as_deref() != Some(subject.tip) {
        report
            .kept
            .push(kept(subject, None, "the branch moved since it was judged"));
        return Ok(());
    }
    match repo.git(&["branch", flag, "--", branch]) {
        Ok(_) => {
            row["restore"] = json!(format!("git branch {branch} {}", subject.tip));
            report.delete.push(row);
        }
        Err(error) => report.kept.push(kept(subject, None, &error.to_string())),
    }
    Ok(())
}
