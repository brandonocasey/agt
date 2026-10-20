//! `agt worktree port`: a dev-server port that stays with its worktree. Ports live in
//! `$XDG_STATE_HOME/agents/worktrees/ports.json`, keyed by the worktree's absolute path.

use std::collections::{BTreeMap, HashSet};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Args;
use serde_json::{Value, json};

use crate::core::error::{Error, Result, ctx};
use crate::core::local::{self, read_json, update_json};
use crate::core::output::Output;

/// Below the ephemeral ranges of Linux (32768 and up) and macOS (49152 and up), so an
/// outgoing connection never takes a recorded port.
const FIRST: u16 = 10000;
const LAST: u16 = 32767;

#[derive(Args)]
pub struct Cli {
    /// The worktree; default: the current one.
    path: Option<PathBuf>,
    /// Print `PORT=<n>` instead of JSON.
    #[arg(long)]
    env: bool,
}

const PORTS_FILE: &str = "ports.json";

type Ports = BTreeMap<String, u16>;

pub fn run(cli: Cli) -> Result<Output> {
    let dir = match cli.path {
        Some(path) => path,
        None => std::env::current_dir().map_err(ctx("read the current directory"))?,
    };
    let (path, port) = assign(&super::info::toplevel(&dir)?)?;
    if cli.env {
        return Ok(Output::Text(format!("PORT={port}\n")));
    }
    Ok(Output::Json(report(&path, port)))
}

pub(super) fn report(path: &Path, port: u16) -> Value {
    json!({"path": path, "port": port})
}

/// The port recorded for the worktree at `dir`, picking and recording a free one the first time.
pub(super) fn assign(dir: &Path) -> Result<(PathBuf, u16)> {
    let path = dir
        .canonicalize()
        .map_err(ctx(format!("resolve {}", dir.display())))?;
    let key = path.to_string_lossy().into_owned();
    let file = folder()?.join(PORTS_FILE);
    let mut port = 0;
    update_json::<Ports>(&file, |ports| {
        let before = ports.len();
        ports.retain(|worktree, _| Path::new(worktree).exists());
        let mut changed = ports.len() != before;
        port = match ports.get(&key) {
            Some(port) => *port,
            None => {
                let port = pick(&ports.values().copied().collect())?;
                ports.insert(key, port);
                changed = true;
                port
            }
        };
        Ok(changed)
    })?;
    Ok((path, port))
}

/// `$XDG_STATE_HOME/agents/worktrees`, without creating it.
fn state_dir() -> Result<PathBuf> {
    Ok(local::agents_state()?.join("worktrees"))
}

/// `$XDG_STATE_HOME/agents/worktrees`, created privately when missing.
pub(super) fn folder() -> Result<PathBuf> {
    let folder = state_dir()?;
    local::private_dir(&folder)?;
    Ok(folder)
}

/// The port already recorded for the worktree at `dir`; nothing is recorded or created.
pub(super) fn recorded(dir: &Path) -> Result<Option<u16>> {
    let path = dir
        .canonicalize()
        .map_err(ctx(format!("resolve {}", dir.display())))?;
    let file = state_dir()?.join(PORTS_FILE);
    Ok(read_json::<Ports>(&file)?
        .unwrap_or_default()
        .get(path.to_string_lossy().as_ref())
        .copied())
}

/// A free port that no recorded worktree owns, found from a random start in the range.
fn pick(taken: &HashSet<u16>) -> Result<u16> {
    let span = u32::from(LAST - FIRST) + 1;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let start = nanos ^ std::process::id();
    (0..span)
        .map(|step| {
            let offset = (start % span + step) % span;
            FIRST + u16::try_from(offset).unwrap_or(0)
        })
        .find(|port| !taken.contains(port) && is_free(*port))
        .ok_or_else(|| Error(format!("no free port in {FIRST}-{LAST}")))
}

/// Nothing listens on the port and it can be bound. Binding alone can succeed beside a
/// listener bound to another address.
fn is_free(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).is_err()
        && TcpListener::bind(("127.0.0.1", port)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_taken_port_is_never_picked() {
        let open = [12345, 23456];
        let taken: HashSet<u16> = (FIRST..=LAST).filter(|port| !open.contains(port)).collect();
        assert!(open.contains(&pick(&taken).unwrap()));
    }

    #[test]
    fn a_full_range_has_no_port() {
        let taken: HashSet<u16> = (FIRST..=LAST).collect();
        assert!(pick(&taken).is_err());
    }
}
