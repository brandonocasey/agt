//! `agt issue`: GitHub and GitLab issues kept locally. Reads are local; `sync` fetches; every
//! write is queued in the local copy and sent by `push`.

mod cli;
mod local;
mod merge;
mod refs;
mod sync;
mod trackers;
mod view;

pub use cli::Cli;

use cli::Command;

use crate::core::config::Config;
use crate::core::error::Result;
use crate::core::output::Output;
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
    };
    Ok(Output::Json(value))
}
