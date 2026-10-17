//! `agt issue`: GitHub and GitLab issues kept locally. Reads are local; `sync` fetches; every
//! write is queued in the local copy and sent by `push`.

mod cli;
mod local;
mod merge;
mod push;
mod refs;
mod sync;
mod trackers;
mod view;

pub use cli::Cli;

use cli::Command;

use crate::core::config::Config;
use crate::core::error::Result;
use crate::core::output::Output;
use crate::store::issues::Category;
use crate::store::{Namespace, Store};

pub fn run(cli: Cli) -> Result<Output> {
    let store = Store::open(Namespace::Issues)?;
    let filter = cli.tracker.as_deref();
    let value = match &cli.command {
        Command::Sync => sync::run(&store, &Config::load()?, filter)?,
        Command::List(args) => view::list(&store, filter, args)?,
        Command::Show { key } => view::show(&store, filter, key)?,
        Command::Search { text } => view::search(&store, filter, text)?,
        Command::Status => view::status(&store, filter)?,
        Command::Find(args) => view::find(&store, filter, args)?,
        Command::Link(args) => local::link(&store, filter, args)?,
        Command::Note { key, text } => local::note(&store, filter, key, text)?,
        Command::Comment { key, text } => local::comment(&store, filter, key, text)?,
        Command::Close { key, reason } => {
            local::set_state(&store, filter, key, Category::Closed, *reason)?
        }
        Command::Reopen { key } => local::set_state(&store, filter, key, Category::Open, None)?,
        Command::Assign(args) => local::assign(&store, filter, &args.key, &args.logins, true)?,
        Command::Unassign(args) => local::assign(&store, filter, &args.key, &args.logins, false)?,
        Command::Label { action } => local::labels(&store, filter, action)?,
        Command::LinkPr { key, pr } => local::link_pr(&store, filter, key, pr)?,
        Command::Create(args) => local::create(&store, &Config::load()?, filter, args)?,
        Command::Edit(args) => local::edit(&store, filter, args)?,
        Command::Push { dry_run, force } => push::run(
            &store,
            &Config::load()?,
            filter,
            &push::Options {
                dry_run: *dry_run,
                force: *force,
            },
        )?,
    };
    Ok(Output::Json(value))
}
