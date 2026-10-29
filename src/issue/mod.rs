//! `agt issue`: GitHub and GitLab issues kept locally. Reads are local, after an auto-sync of
//! the trackers whose last sync is older than `sync.max_age_secs` (see [`auto`]); `sync` fetches;
//! every write is queued in the local copy and sent by `push`.

mod auto;
mod cli;
mod fixes;
mod hierarchy;
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

use crate::core::config::{Config, Mode};
use crate::core::error::Result;
use crate::core::output::Output;
use crate::git::Repo;
use crate::store::{Namespace, Store};

/// The push half of `sync`: send the queued changes of every tracker whose push is manual or
/// auto, the way `push` does. When none sends, the changes stay local and the report says so.
fn push_after_pull(
    store: &Store,
    destinations: &[crate::core::config::Tracker],
    filter: Option<&str>,
) -> Result<serde_json::Value> {
    let sending: Vec<crate::core::config::Tracker> = trackers::configured(destinations, filter)?
        .into_iter()
        .filter(|tracker| tracker.push_mode() != Mode::Off)
        .cloned()
        .collect();
    if sending.is_empty() {
        let names = trackers::configured(destinations, filter)?
            .iter()
            .map(|tracker| tracker.name())
            .collect::<Vec<_>>()
            .join(", ");
        let mut report = serde_json::json!({ "dry_run": false, "trackers": [] });
        if let Some(reason) = crate::forge::auto::kept_local(
            "issue sync",
            crate::core::config::Kind::Issues,
            &names,
            None,
        ) {
            report["kept_local"] = serde_json::json!(reason);
        }
        return Ok(report);
    }
    if sending.len() == 1 {
        auto::attach_pending(store, &sending, None)?;
    }
    push::run(
        store,
        destinations,
        filter,
        &push::Options {
            dry_run: false,
            force: false,
        },
    )
}

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
    let _hierarchy_lock = if matches!(
        cli.command,
        Command::Create(_) | Command::Edit(_) | Command::Discard { .. }
    ) {
        Some(store.publish_lock(store.root(), "hierarchy")?)
    } else {
        None
    };
    let mut value = match &cli.command {
        Command::Sync => {
            let config = Config::load_for_repo(&repo)?;
            let destinations = trackers::destinations(&config, &repo, filter, false)?;
            let synced = sync::run(&store, &destinations, filter)?;
            let mut value = synced.report;
            // A tracker that failed to pull is not pushed to; its changes stay queued.
            let healthy: Vec<crate::core::config::Tracker> = destinations
                .iter()
                .filter(|tracker| {
                    !synced
                        .failed
                        .iter()
                        .any(|(name, _)| *name == tracker.name())
                })
                .cloned()
                .collect();
            // With `--tracker`, the trackers it names may all have failed: nothing is left to push.
            let all_named_failed =
                trackers::configured(&destinations, filter)?
                    .iter()
                    .all(|tracker| {
                        synced
                            .failed
                            .iter()
                            .any(|(name, _)| *name == tracker.name())
                    });
            value["push"] = if healthy.is_empty() || all_named_failed {
                serde_json::json!({ "dry_run": false, "trackers": [] })
            } else {
                push_after_pull(&store, &healthy, filter)?
            };
            if !synced.failed.is_empty() {
                let failures: Vec<String> = synced
                    .failed
                    .iter()
                    .map(|(name, error)| format!("{name}: {error}"))
                    .collect();
                eprintln!(
                    "agt: error: issue sync failed for {} tracker(s): {}",
                    failures.len(),
                    failures.join("; ")
                );
                return Ok(Output::Json(value).exiting(1));
            }
            value
        }
        Command::List(args) => view::list(&store, filter, args)?,
        Command::Show { key } => view::show(&store, filter, key)?,
        Command::Status => {
            // Without a readable configuration the push flag is the one stored at the last sync.
            let current = Config::load_for_repo(&repo)
                .and_then(|config| config.trackers_for(&repo, crate::core::config::Kind::Issues))
                .ok();
            view::status(&store, filter, current.as_deref())?
        }
        Command::Link(args) => local::link(&store, filter, args)?,
        Command::Comment { key, text, private } => {
            local::comment(&store, filter, key, text, *private)?
        }
        Command::Create(args) => {
            let destinations = if filter.is_some() && filter != Some("local") {
                let config = Config::load_for_repo(&repo)?;
                trackers::destinations(&config, &repo, filter, false)?
            } else {
                Vec::new()
            };
            local::create(&store, &destinations, filter, args)?
        }
        Command::Edit(args) => local::edit(&store, filter, args)?,
        Command::Discard { key } => local::discard(&store, filter, key)?,
        Command::Push { dry_run, force } => {
            let config = Config::load_for_repo(&repo)?;
            let destinations = trackers::destinations(&config, &repo, filter, false)?;
            let sends = trackers::configured(&destinations, filter)?
                .iter()
                .any(|tracker| tracker.push_mode() != Mode::Off);
            if !dry_run && sends {
                auto::attach_pending(&store, &destinations, filter)?;
            }
            push::run(
                &store,
                &destinations,
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
