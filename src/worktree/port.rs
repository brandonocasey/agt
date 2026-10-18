//! `agt worktree port`: a dev-server port that stays with its worktree. Ports live in
//! `$XDG_STATE_HOME/agents/worktrees/ports.json`, keyed by the worktree's absolute path.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Args;
use serde_json::{Value, json};

use crate::core::error::{Error, Result, ctx, fail};
use crate::core::output::Output;
use crate::store::{Namespace, Store};

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

type Ports = BTreeMap<String, u16>;

pub fn run(cli: Cli) -> Result<Output> {
    let dir = match cli.path {
        Some(path) => path,
        None => std::env::current_dir().map_err(ctx("read the current directory"))?,
    };
    let (path, port) = assign(&super::create::toplevel(&dir)?)?;
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
    let folder = Store::open(Namespace::Tasks)?
        .agents_dir()
        .join("worktrees");
    fs::create_dir_all(&folder).map_err(ctx(format!("create {}", folder.display())))?;
    let _lock = lock(&folder.join("ports.lock"))?;
    let file = folder.join("ports.json");
    let mut ports = load(&file)?;
    let before = ports.len();
    ports.retain(|worktree, _| Path::new(worktree).exists());
    let mut changed = ports.len() != before;
    let port = match ports.get(&key) {
        Some(port) => *port,
        None => {
            let port = pick(&ports.values().copied().collect())?;
            ports.insert(key, port);
            changed = true;
            port
        }
    };
    if changed {
        save(&file, &ports)?;
    }
    Ok((path, port))
}

fn lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(ctx(format!("open {}", path.display())))?;
    file.lock()
        .map_err(ctx(format!("lock {}", path.display())))?;
    Ok(file)
}

fn load(file: &Path) -> Result<Ports> {
    match fs::read(file) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(ctx(format!("parse {}", file.display())))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Ports::new()),
        Err(error) => fail(format!("read {}: {error}", file.display())),
    }
}

fn save(file: &Path, ports: &Ports) -> Result<()> {
    let temp = file.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(ports).map_err(ctx("serialize ports"))?;
    fs::write(&temp, text + "\n").map_err(ctx(format!("write {}", temp.display())))?;
    fs::rename(&temp, file).map_err(ctx(format!("replace {}", file.display())))
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
