//! Whether any process other than agt, its ancestors and the calling session's own processes
//! has its current directory inside a worktree.

use std::path::{Path, PathBuf};

use crate::core::agents;

/// The current directories of all processes, read once on first use.
#[derive(Default)]
pub struct Cwds {
    all: Option<Result<Vec<(i32, PathBuf)>, String>>,
    /// The calling session's other processes, read when one is found inside a worktree.
    session: Option<Vec<i32>>,
}

impl Cwds {
    /// Why `root` is not idle: a process in it that is not the calling session's, or the
    /// reason this cannot be checked.
    pub fn busy(&mut self, root: &Path) -> Option<String> {
        let inside = match self.inside(root) {
            Err(why) => return Some(format!("cannot check running processes: {why}")),
            Ok(inside) => inside,
        };
        let session = self.session_tree(&inside);
        inside
            .iter()
            .find(|(pid, _)| !session.contains(pid))
            .map(|(pid, cwd)| describe(*pid, cwd, true))
    }

    /// The calling session's own processes inside `root`, which do not block it: stop them
    /// before removing it.
    pub fn own_in(&mut self, root: &Path) -> Vec<String> {
        let Ok(inside) = self.inside(root) else {
            return Vec::new();
        };
        let session = self.session_tree(&inside);
        inside
            .iter()
            .filter(|(pid, _)| session.contains(pid))
            .map(|(pid, cwd)| format!("{} (of the calling session)", describe(*pid, cwd, false)))
            .collect()
    }

    fn inside(&mut self, root: &Path) -> Result<Vec<(i32, PathBuf)>, String> {
        match self.all.get_or_insert_with(read) {
            Err(why) => Err(why.clone()),
            Ok(all) => Ok(all
                .iter()
                .filter(|(_, cwd)| cwd.starts_with(root))
                .cloned()
                .collect()),
        }
    }

    fn session_tree(&mut self, inside: &[(i32, PathBuf)]) -> &[i32] {
        if inside.is_empty() {
            return &[];
        }
        self.session.get_or_insert_with(agents::session_tree)
    }
}

/// `pid` and its program name; `full` adds the command line, which names what blocks a removal.
fn describe(pid: i32, cwd: &Path, full: bool) -> String {
    let command = agents::read(pid).map_or_else(
        || "?".to_owned(),
        |p| match p.cmd.as_str() {
            cmd if full && !cmd.is_empty() => format!("{}: {cmd}", p.comm),
            _ => p.comm,
        },
    );
    format!("process {pid} ({command}) runs in {}", cwd.display())
}

fn read() -> Result<Vec<(i32, PathBuf)>, String> {
    let own = agents::lineage();
    let mut all = agents::cwds_checked().map_err(|error| error.to_string())?;
    all.retain(|(pid, _)| !own.contains(pid));
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_failure_refuses_an_idle_worktree() {
        let mut cwds = Cwds {
            all: Some(Err("process table unavailable".to_owned())),
            session: None,
        };
        assert_eq!(
            cwds.busy(Path::new("/unused/worktree")),
            Some("cannot check running processes: process table unavailable".to_owned())
        );
    }
}
