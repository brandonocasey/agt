//! `agt issue`: GitHub and GitLab issues kept locally. Reads are local, after an auto-sync of
//! the trackers whose last sync is older than `sync.max_age_secs` (see [`auto`]); `sync` fetches;
//! every write is queued in the local copy and sent by `push`.

mod auto;
mod cli;
mod fixes;
mod local;
mod merge;
pub mod prune;
mod push;
mod refs;
mod sync;
mod trackers;
mod view;

pub use cli::Cli;
pub use fixes::close_fixed;

use cli::Command;

use crate::core::config::Config;
use crate::core::error::Result;
use crate::core::output::Output;
use crate::git::Repo;
use crate::store::{Namespace, Store};

pub fn run(cli: Cli) -> Result<Output> {
    let repo = Repo::discover()?;
    let preview = matches!(cli.command, Command::Push { dry_run: true, .. });
    let store = Store::open_repo(Namespace::Issues, &repo, preview)?;
    let filter = cli.tracker.as_deref();
    if matches!(
        cli.command,
        Command::List(_) | Command::Show { .. } | Command::Status
    ) {
        auto::sync_stale(
            &store,
            &repo,
            filter,
            crate::forge::auto::Offline::current(),
        )?;
    }
    if matches!(cli.command, Command::Sync | Command::Push { .. }) {
        crate::forge::auto::Offline::current().forbid("issue sync and push")?;
    }
    let mut value = match &cli.command {
        Command::Sync => {
            let mut config = Config::load_for_repo(&repo)?;
            trackers::destinations(&mut config, &repo, filter, false)?;
            sync::run(&store, &config, filter)?
        }
        Command::List(args) => view::list(&store, filter, args)?,
        Command::Show { key } => view::show(&store, filter, key)?,
        Command::Status => view::status(&store, filter)?,
        Command::Link(args) => local::link(&store, filter, args)?,
        Command::Comment { key, text, private } => {
            local::comment(&store, filter, key, text, *private)?
        }
        Command::Create(args) => {
            let config = if filter.is_some() && filter != Some("local") {
                Config::load_for_repo(&repo)?
            } else {
                Config::default()
            };
            local::create(&store, &config, filter, args)?
        }
        Command::Edit(args) => local::edit(&store, filter, args)?,
        Command::Push { dry_run, force } => {
            let mut config = Config::load_for_repo(&repo)?;
            trackers::destinations(&mut config, &repo, filter, false)?;
            if !dry_run {
                auto::attach_pending(&store, &config, filter)?;
            }
            push::run(
                &store,
                &config,
                filter,
                &push::Options {
                    dry_run: *dry_run,
                    force: *force,
                },
            )?
        }
    };
    if matches!(
        &cli.command,
        Command::Create(_) | Command::Edit(_) | Command::Comment { private: false, .. }
    ) || matches!(&cli.command, Command::Link(args) if args.comment)
    {
        auto::push_changed(
            &store,
            &repo,
            filter,
            crate::forge::auto::Offline::current(),
            &mut value,
        );
    }
    Ok(Output::Json(value))
}
