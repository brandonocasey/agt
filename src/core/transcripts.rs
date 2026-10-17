//! Where Claude Code and Codex keep their transcripts, shared by `agt task` and `agt sessions`.

use std::fs;
use std::path::{Path, PathBuf};

/// Claude Code profile directories in `home`: `.claude` and `.claude-<name>`, sorted by name.
pub fn claude_profiles(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(home) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    entries
        .into_iter()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name == ".claude" || name.starts_with(".claude-")
        })
        .map(|entry| entry.path())
        .collect()
}

/// Codex session roots: live sessions, then archived ones.
pub fn codex_roots(home: &Path) -> [PathBuf; 2] {
    let codex = home.join(".codex");
    [codex.join("sessions"), codex.join("archived_sessions")]
}
