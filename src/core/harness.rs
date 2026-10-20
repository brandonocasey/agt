//! Names agt takes from the user's agent harness setup (Claude Code, Codex and the shared
//! `agents` cache) rather than from agt itself, in one place.

use std::path::{Path, PathBuf};

/// Task scratch space, relative to `$HOME`; `agt doctor` wants it in the global git ignore.
pub const SCRATCH_DIR: &str = ".cache/agents/scratch";

/// The scratch directory as the anchored global git ignore pattern.
pub const SCRATCH_IGNORE: &str = "/.cache/agents/scratch/";

/// A path in the scratch directory that `git check-ignore` probes.
pub const SCRATCH_PROBE: &str = ".cache/agents/scratch/probe";

/// The scratch directory's name under the `agents` cache directory.
pub const SCRATCH_NAME: &str = "scratch";

/// Where Claude Code's settings put its temporary files, relative to `$HOME`.
pub const CLAUDE_TMP_DIR: &str = ".cache/claude-tmp";

/// Name prefix of Claude Code's per-profile config directories under `$HOME`: `.claude`,
/// `.claude-<name>`.
pub const CLAUDE_DIR_PREFIX: &str = ".claude";

/// The environment variable that relocates Claude Code's config directory.
pub const CLAUDE_CONFIG_ENV: &str = "CLAUDE_CONFIG_DIR";

/// Codex's config directory under `$HOME`.
pub const CODEX_DIR: &str = ".codex";

/// Where Codex keeps its standalone releases, relative to [`CODEX_DIR`].
pub const CODEX_STANDALONE_DIR: &str = "packages/standalone";

/// Transcript project directory names (a working directory with `/` and `.` as `-`) of scratch,
/// probe and `/tmp` sessions.
pub const SCRATCH_PROJECT_PATTERN: &str =
    r"-cache-agents-scratch|-cache-claude-tmp|^-private-tmp|^-tmp";

/// Path fragment of the skill files an agent reads.
pub const SKILLS_PATH: &str = "/skills/";

/// Opening tag of the skill listing Codex injects as a user message.
pub const SKILLS_INSTRUCTIONS_TAG: &str = "<skills_instructions>";

/// Path fragment of files under Codex's config directory.
pub const CODEX_PATH: &str = "/.codex/";

/// Claude Code profile directories: `.claude` and `.claude-<name>` in `home`, links followed,
/// and `$CLAUDE_CONFIG_DIR` when set, sorted by path.
pub fn claude_profiles(home: &Path) -> Vec<PathBuf> {
    let mut profiles: Vec<PathBuf> = std::fs::read_dir(home)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(CLAUDE_DIR_PREFIX)
        })
        .map(|entry| entry.path())
        .chain(
            std::env::var_os(CLAUDE_CONFIG_ENV)
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from),
        )
        .filter(|path| path.is_dir())
        .collect();
    profiles.sort();
    profiles.dedup();
    profiles
}
