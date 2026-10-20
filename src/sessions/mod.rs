//! `agt sessions`: cost, context, tool, cache, error and friction reports from Claude Code and
//! Codex transcripts.

mod events;
mod fmt;
mod index;
pub mod live;
mod prices;
mod source;
mod spend;
mod tools;

use std::io::Write;
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};

use crate::core::clock;
use crate::core::error::{Result, fail};
use crate::core::harness::Harness;
use crate::core::local;
use crate::core::output::Output;

/// Cost, context, tools, cache gaps, errors and friction from Claude Code and Codex
/// transcripts in `~/.claude*/projects` and `~/.codex`. Prices come from
/// `$XDG_CONFIG_HOME/agt/model-prices.json`, else `~/.config/agents/model-prices.json`
/// (override: `AGT_PRICES`). Scratch and probe projects
/// are skipped unless `--scratch` is given. `index`, `resolve` and `search` keep a private
/// index of the transcripts to find sessions and what was typed in them.
#[derive(Args)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// USD by agent and model.
    Cost(CostArgs),
    /// Per agent type: runs, start and peak context, calls, USD.
    Agents(Common),
    /// Tool calls and the share of cost their results carry.
    Tools(Common),
    /// Idle gaps that expired the prompt cache, and what ran before them.
    CacheGaps(GapArgs),
    /// Tool errors grouped by tool and message.
    Errors(ErrorArgs),
    /// Permission denials grouped by source and reason.
    Denials(Common),
    /// Interrupts, corrections, short replies and repeated prompts.
    Friction(FrictionArgs),
    /// Combine selected reports with one transcript scan.
    Report(ReportArgs),
    /// Update the private session index and show coverage.
    Index(index::IndexArgs),
    /// Resolve an exact session id to its source.
    Resolve(index::ResolveArgs),
    /// Search indexed human turns with bounded context.
    Search(index::SearchArgs),
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Which {
    All,
    Claude,
    Codex,
}

impl Which {
    fn includes(self, harness: Harness) -> bool {
        matches!(
            (self, harness),
            (Self::All, _) | (Self::Claude, Harness::Claude) | (Self::Codex, Harness::Codex)
        )
    }
}

#[derive(Args)]
struct Common {
    /// Look back this many days.
    #[arg(long, default_value_t = 30)]
    days: i64,
    /// Start date, YYYY-MM-DD; overrides --days.
    #[arg(long)]
    since: Option<String>,
    /// Only projects whose directory name contains this text.
    #[arg(long)]
    project: Option<String>,
    /// Include scratch and probe projects.
    #[arg(long)]
    scratch: bool,
    /// Transcript root; repeat for several. Default: every Claude Code profile's `projects`
    /// and Codex's session directories.
    #[arg(long, value_name = "DIR")]
    root: Vec<String>,
    /// Rows per table.
    #[arg(long, default_value_t = 25)]
    top: usize,
    /// Which harness's transcripts to read.
    #[arg(long, value_enum, default_value_t = Which::All)]
    harness: Which,
}

#[derive(Args)]
struct CostArgs {
    #[command(flatten)]
    common: Common,
    /// Comma list of agent, model, project, session, file, harness.
    #[arg(long, default_value = "agent,model")]
    by: String,
    /// Print rows as JSON.
    #[arg(long)]
    json: bool,
    /// Print what the session with this full id has cost so far, subagents included, instead
    /// of the grouped report.
    #[arg(long, value_name = "ID", conflicts_with_all = ["by", "files"])]
    session: Option<String>,
    /// Price these transcript or stream-json files instead of the roots.
    files: Vec<String>,
}

#[derive(Args)]
struct GapArgs {
    #[command(flatten)]
    common: Common,
    /// Idle minutes that count as a gap [default: 5].
    #[arg(long)]
    gap: Option<f64>,
}

#[derive(Args)]
struct FrictionArgs {
    #[command(flatten)]
    common: Common,
    /// Print every user message: date, project, session, text.
    #[arg(long)]
    tsv: bool,
    /// TSV text width.
    #[arg(long, default_value_t = 300)]
    width: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ErrorKind {
    Error,
    Denial,
    All,
}

#[derive(Args)]
struct ErrorArgs {
    #[command(flatten)]
    common: Common,
    /// Include tool errors, permission denials, or both.
    #[arg(long, value_enum, default_value_t = ErrorKind::Error)]
    kind: ErrorKind,
}

#[derive(Clone, Copy, ValueEnum)]
enum Section {
    Cost,
    Agents,
    Tools,
    CacheGaps,
    Errors,
    Denials,
    Friction,
}

#[derive(Args)]
struct ReportArgs {
    #[command(flatten)]
    common: Common,
    /// Comma-separated reports to print, in this order.
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        default_value = "cost,agents,tools,cache-gaps,errors,denials,friction"
    )]
    sections: Vec<Section>,
    /// Cost grouping: agent, model, project, session, file, harness.
    #[arg(long, default_value = "agent,model")]
    by: String,
    /// Idle minutes that count as a cache gap.
    #[arg(long)]
    gap: Option<f64>,
}

enum Report {
    Cost(spend::Cost, bool),
    Agents(spend::Agents),
    Tools(tools::Tools),
    CacheGaps(tools::CacheGaps),
    Failures(events::Failures),
    Friction(events::Friction, bool, usize),
}

impl Report {
    fn needs(&self, needs: &mut source::Needs) {
        match self {
            Self::Cost(..) | Self::Agents(_) => needs.calls = true,
            Self::Tools(_) => needs.tools = true,
            Self::CacheGaps(_) => needs.gaps = true,
            Self::Failures(_) => needs.failures = true,
            Self::Friction(..) => needs.friction = true,
        }
    }

    fn feed(&mut self, transcript: &source::Loaded) {
        match self {
            Self::Cost(report, _) => report.feed(transcript),
            Self::Agents(report) => report.feed(transcript),
            Self::Tools(report) => report.feed(transcript),
            Self::CacheGaps(report) => report.feed(transcript),
            Self::Failures(report) => report.feed(transcript),
            Self::Friction(report, _, _) => report.feed(transcript),
        }
    }

    fn render(self, scope: &Scope) -> Result<String> {
        match self {
            Self::Cost(report, json) => report.render(scope, json),
            Self::Agents(report) => Ok(report.render()),
            Self::Tools(report) => Ok(report.render(scope)),
            Self::CacheGaps(report) => Ok(report.render(scope)),
            Self::Failures(report) => Ok(report.render(scope)),
            Self::Friction(report, tsv, width) => Ok(report.render(scope, tsv, width)),
        }
    }
}

fn single(scope: Scope, mut report: Report) -> Result<String> {
    let mut needs = source::Needs::default();
    report.needs(&mut needs);
    for transcript in source::load(&scope, needs) {
        report.feed(&transcript);
    }
    report.render(&scope)
}

/// What a report reads.
pub struct Scope {
    /// The start date as given or computed, `YYYY-MM-DD`.
    pub since: String,
    /// Records stamped before this are skipped; empty for explicit files.
    pub since_iso: String,
    /// Files last changed before this local midnight are skipped.
    pub cutoff: f64,
    pub project: Option<String>,
    pub scratch: bool,
    pub roots: Vec<String>,
    pub files: Vec<String>,
    pub top: usize,
    pub harness: Which,
    pub home: PathBuf,
}

fn scope(common: Common, files: Vec<String>) -> Result<Scope> {
    let home = local::home()?;
    let since = match common.since {
        Some(since) => since,
        None => clock::days_back(clock::now_secs()?, common.days),
    };
    let Some(cutoff) = clock::local_midnight(&since) else {
        return fail("--since needs YYYY-MM-DD");
    };
    // A file that cannot be read must not price as nothing.
    for file in &files {
        match std::fs::metadata(file) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return fail(format!("read {file}: not a file; --root takes a directory")),
            Err(error) => return fail(format!("read {file}: {error}")),
        }
    }
    Ok(Scope {
        // Stream-json files from `claude -p` have no timestamps, so explicit files skip the
        // date filter.
        since_iso: if files.is_empty() {
            since.clone()
        } else {
            String::new()
        },
        since,
        cutoff,
        project: common.project.filter(|project| !project.is_empty()),
        scratch: common.scratch,
        roots: common.root,
        files,
        top: common.top,
        harness: common.harness,
        home,
    })
}

pub fn run(cli: Cli) -> Result<Output> {
    let text = match cli.command {
        Command::Cost(CostArgs {
            session: Some(id),
            json,
            ..
        }) => spend::session(&id, json)?,
        Command::Cost(args) => single(
            scope(args.common, args.files)?,
            Report::Cost(spend::Cost::new(&args.by)?, args.json),
        )?,
        Command::Agents(common) => single(
            scope(common, Vec::new())?,
            Report::Agents(spend::Agents::new()?),
        )?,
        Command::Tools(common) => single(
            scope(common, Vec::new())?,
            Report::Tools(tools::Tools::new()),
        )?,
        Command::CacheGaps(args) => single(
            scope(args.common, Vec::new())?,
            Report::CacheGaps(tools::CacheGaps::new(args.gap)?),
        )?,
        Command::Errors(args) => single(
            scope(args.common, Vec::new())?,
            Report::Failures(events::Failures::new(args.kind)),
        )?,
        Command::Denials(common) => single(
            scope(common, Vec::new())?,
            Report::Failures(events::Failures::new(ErrorKind::Denial)),
        )?,
        Command::Friction(args) => single(
            scope(args.common, Vec::new())?,
            Report::Friction(events::Friction::new(), args.tsv, args.width),
        )?,
        Command::Report(args) => {
            let scope = scope(args.common, Vec::new())?;
            let mut reports = args
                .sections
                .into_iter()
                .map(|section| {
                    Ok(match section {
                        Section::Cost => ("Cost", Report::Cost(spend::Cost::new(&args.by)?, false)),
                        Section::Agents => ("Agents", Report::Agents(spend::Agents::new()?)),
                        Section::Tools => ("Tools", Report::Tools(tools::Tools::new())),
                        Section::CacheGaps => (
                            "Cache gaps",
                            Report::CacheGaps(tools::CacheGaps::new(args.gap)?),
                        ),
                        Section::Errors => (
                            "Errors",
                            Report::Failures(events::Failures::new(ErrorKind::Error)),
                        ),
                        Section::Denials => (
                            "Denials",
                            Report::Failures(events::Failures::new(ErrorKind::Denial)),
                        ),
                        Section::Friction => (
                            "Friction",
                            Report::Friction(events::Friction::new(), false, 300),
                        ),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let mut needs = source::Needs::default();
            for (_, report) in &reports {
                report.needs(&mut needs);
            }
            for transcript in source::load(&scope, needs) {
                for (_, report) in &mut reports {
                    report.feed(&transcript);
                }
            }
            reports
                .into_iter()
                .map(|(heading, report)| Ok(format!("{heading}:\n{}", report.render(&scope)?)))
                .collect::<Result<Vec<_>>>()?
                .join("\n")
        }
        Command::Index(args) => return index::index_command(&args),
        Command::Resolve(args) => return index::resolve(&args),
        Command::Search(args) => return index::search(&args),
    };
    // A reader that stops early, such as `head`, is not an error.
    match std::io::stdout().lock().write_all(text.as_bytes()) {
        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
            fail(format!("write stdout: {error}"))
        }
        _ => Ok(Output::Exit(0)),
    }
}
