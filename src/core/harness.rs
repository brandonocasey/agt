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

/// Path fragment of the skill files an agent reads.
pub const SKILLS_PATH: &str = "/skills/";

/// The user-message starts only a harness writes: Codex's `# AGENTS.md instructions`,
/// `<environment_context>` and `<skills_instructions>`, and the `<system...>` and
/// `<developer...>` tags.
pub const HARNESS_PREFIXES: [&str; 5] = [
    "# AGENTS.md instructions",
    "<environment_context>",
    "<skills_instructions>",
    "<system",
    "<developer",
];

/// Further starts of a user message that a typed prompt can share: any tag (Claude Code's
/// `<command-...>` and `<system-reminder>`) or Claude Code's `Caveat:`. Reports of what a
/// person typed drop these too; the session index keeps them.
pub const TYPED_PREFIXES: [&str; 2] = ["<", "Caveat:"];

/// Path fragment of files under Codex's config directory.
const CODEX_PATH: &str = "/.codex/";

/// An agent harness agt reads transcripts of and finds running.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    Claude,
    Codex,
}

impl Harness {
    pub const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    /// The name reports and `agt ps` give the harness.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// The name of its process.
    pub const fn command(self) -> &'static str {
        self.id()
    }

    /// The harness a transcript file at `path` belongs to, going by its directory.
    pub fn of_path(path: &str) -> Self {
        if path.contains(CODEX_PATH) {
            Self::Codex
        } else {
            Self::Claude
        }
    }
}

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

/// A working directory or config path as Claude Code names its project directory: every
/// character but a letter or digit is `-`.
pub fn project_name(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// A regex for the transcript project directory names of scratch, probe and `/tmp` sessions:
/// the scratch directory, Claude Code's temporary files, and `/private/tmp` and `/tmp`.
pub fn scratch_project_pattern() -> String {
    format!(
        "{}|{}|^-private-tmp|^-tmp",
        project_name(SCRATCH_DIR),
        project_name(CLAUDE_TMP_DIR)
    )
}

/// Whether `path` is inside a Claude Code profile directory: `.claude` or `.claude-<name>`
/// directly under `home`, or `$CLAUDE_CONFIG_DIR`.
pub fn in_claude_profile(path: &Path, home: &Path) -> bool {
    let under_home = path.strip_prefix(home).is_ok_and(|rest| {
        let mut parts = rest.components();
        parts.next().is_some_and(|first| {
            first
                .as_os_str()
                .to_string_lossy()
                .starts_with(CLAUDE_DIR_PREFIX)
        }) && parts.next().is_some()
    });
    under_home
        || std::env::var_os(CLAUDE_CONFIG_ENV)
            .filter(|dir| !dir.is_empty())
            .is_some_and(|dir| path.starts_with(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scratch_pattern_names_the_scratch_and_tmp_directories() {
        assert_eq!(
            scratch_project_pattern(),
            r"-cache-agents-scratch|-cache-claude-tmp|^-private-tmp|^-tmp"
        );
    }

    #[test]
    fn only_claude_profile_directories_count_as_claude_paths() {
        let home = Path::new("/home/al");
        for inside in [
            "/home/al/.claude/rules/x.md",
            "/home/al/.claude-work/skills/y",
        ] {
            assert!(in_claude_profile(Path::new(inside), home), "{inside}");
        }
        for outside in [
            "/home/al/repo/.claude/settings.json",
            "/home/al/.claude.json",
            "/home/al/.claude",
            "/srv/.claude/x",
        ] {
            assert!(!in_claude_profile(Path::new(outside), home), "{outside}");
        }
    }
}
