//! The directories `agt worktree list --all` scans for repositories.

use std::fs;
use std::path::PathBuf;

use crate::core::config::Config;
use crate::core::error::{Error, Result};

const DEFAULT_ROOTS: [&str; 2] = ["~/Projects/*/*", "~/projects/*/*"];

/// Main checkouts (a directory holding a `.git` directory) under the configured roots,
/// without duplicates.
pub fn repositories() -> Result<Vec<PathBuf>> {
    let patterns = match Config::load()?.worktree.roots {
        Some(roots) => roots,
        None => DEFAULT_ROOTS.map(str::to_owned).to_vec(),
    };
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Error("HOME is not set".into()))?;
    let mut found = Vec::new();
    for pattern in patterns {
        let pattern = match pattern.strip_prefix("~/") {
            Some(rest) => format!("{}/{rest}", home.display()),
            None if pattern == "~" => home.display().to_string(),
            None => pattern,
        };
        for path in expand(&pattern) {
            if path.join(".git").is_dir()
                && let Ok(real) = fs::canonicalize(&path)
            {
                found.push(real);
            }
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

/// Directories matching `pattern`, where `*` and `?` match within one path component.
fn expand(pattern: &str) -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from(if pattern.starts_with('/') {
        "/"
    } else {
        ""
    })];
    for part in pattern.split('/').filter(|part| !part.is_empty()) {
        let mut next = Vec::new();
        for base in &paths {
            if !part.contains(['*', '?']) {
                next.push(base.join(part));
                continue;
            }
            let Ok(entries) = fs::read_dir(base) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.path().is_dir() && matches(part, &entry.file_name().to_string_lossy()) {
                    next.push(entry.path());
                }
            }
        }
        paths = next;
    }
    paths
}

fn matches(pattern: &str, name: &str) -> bool {
    fn go(pattern: &[char], name: &[char]) -> bool {
        match pattern.split_first() {
            None => name.is_empty(),
            Some(('*', rest)) => (0..=name.len()).any(|skip| go(rest, &name[skip..])),
            Some((c, rest)) => {
                name.first().is_some_and(|n| *c == '?' || c == n) && go(rest, &name[1..])
            }
        }
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    go(&pattern, &name)
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn stars_and_marks_match_within_a_component() {
        assert!(matches("*", "anything"));
        assert!(matches("a*c", "abbbc"));
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "ac"));
        assert!(!matches("a*c", "abd"));
    }
}
