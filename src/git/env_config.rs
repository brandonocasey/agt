//! Config that git takes from `GIT_CONFIG_COUNT` and `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_VALUE_<n>`,
//! which libgit2 does not read. Only the reads that go through [`Repo::config_value`] and the
//! `core.bare` check see it: adding it to the repository's own config would redirect libgit2's
//! writes to it. `GIT_CONFIG_PARAMETERS` (what `git -c` exports) is not read.
//!
//! [`Repo::config_value`]: super::Repo::config_value

/// The value the environment sets for `key`; the last of several wins.
pub(super) fn value(key: &str) -> Option<String> {
    let count = std::env::var("GIT_CONFIG_COUNT")
        .ok()?
        .parse::<usize>()
        .ok()?;
    (0..count).rev().find_map(|n| {
        let name = std::env::var(format!("GIT_CONFIG_KEY_{n}")).ok()?;
        if !name.eq_ignore_ascii_case(key) {
            return None;
        }
        std::env::var(format!("GIT_CONFIG_VALUE_{n}")).ok()
    })
}

/// Whether `repo` is bare: `core.bare` as the environment has it, which libgit2 did not see when
/// it opened the repository, else as libgit2 found it.
pub(super) fn is_bare(repo: &git2::Repository) -> bool {
    match value("core.bare").as_deref() {
        Some("true" | "yes" | "on" | "1") => true,
        Some("false" | "no" | "off" | "0") => false,
        _ => repo.is_bare(),
    }
}
