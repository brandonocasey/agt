//! Whether any process has its current directory inside a worktree.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The current directories of all processes, read once on first use.
#[derive(Default)]
pub struct Cwds(Option<Result<Vec<(i32, PathBuf)>, String>>);

impl Cwds {
    /// Why `root` is not idle: a process in it, or the reason this cannot be checked.
    pub fn busy(&mut self, root: &Path) -> Option<String> {
        match self.0.get_or_insert_with(read) {
            Err(why) => Some(format!("cannot check running processes: {why}")),
            Ok(all) => all
                .iter()
                .find(|(_, cwd)| cwd.starts_with(root))
                .map(|(pid, cwd)| format!("process {pid} runs in {}", cwd.display())),
        }
    }
}

fn read() -> Result<Vec<(i32, PathBuf)>, String> {
    let me = std::process::id().cast_signed();
    let mut all = if cfg!(target_os = "linux") {
        proc_cwds()?
    } else {
        lsof_cwds()?
    };
    all.retain(|(pid, _)| *pid != me);
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
