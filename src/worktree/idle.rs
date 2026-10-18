//! Whether any process other than agt, its ancestors and the calling session's own processes
//! has its current directory inside a worktree.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
            .map(|(pid, cwd)| describe(*pid, cwd))
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
            .map(|(pid, cwd)| format!("{} (of the calling session)", describe(*pid, cwd)))
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

fn describe(pid: i32, cwd: &Path) -> String {
    let command = agents::read(pid).map_or_else(|| "?".to_owned(), |p| p.comm);
    format!("process {pid} ({command}) runs in {}", cwd.display())
}

fn read() -> Result<Vec<(i32, PathBuf)>, String> {
    let own = agents::lineage();
    let mut all = if cfg!(target_os = "linux") {
        proc_cwds()?
    } else {
        lsof_cwds()?
    };
    all.retain(|(pid, _)| !own.contains(pid));
    Ok(all)
}

fn proc_cwds() -> Result<Vec<(i32, PathBuf)>, String> {
    let dir = fs::read_dir("/proc").map_err(|error| format!("read /proc: {error}"))?;
    Ok(dir
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            // Other users' processes are not readable and cannot be inside the user's worktree.
            Some((pid, fs::read_link(entry.path().join("cwd")).ok()?))
        })
        .collect())
}

fn lsof_cwds() -> Result<Vec<(i32, PathBuf)>, String> {
    let output = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-Fpn"])
        .output()
        .map_err(|error| format!("run lsof: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() && text.is_empty() {
        return Err(format!(
            "lsof failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut all = Vec::new();
    let mut pid = None;
    for line in text.lines() {
        if let Some(number) = line.strip_prefix('p') {
            pid = number.parse().ok();
        } else if let (Some(path), Some(pid)) = (line.strip_prefix('n'), pid) {
            all.push((pid, PathBuf::from(path)));
        }
    }
    Ok(all)
}
