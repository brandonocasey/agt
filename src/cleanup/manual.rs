//! Leftovers that cleanup lists but never deletes, because they may hold work or another tool
//! owns them: merged worktrees and branches, backups, old Codex releases and superseded
//! Playwright browsers. `--yes` saves the list to a report, so a session can mention it
//! without scanning every repository again.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::files::measure;
use crate::core::error::{Error, Result, ctx};
use crate::store::{Namespace, Store};
use crate::worktree::prune;

const COUNT_PREFIX: &str = "<!-- count: ";

pub const SECTIONS: [(&str, &str); 5] = [
    (
        "worktrees",
        "Merged, clean worktrees. Remove with the command shown. Never rm.",
    ),
    (
        "branches",
        "Merged local branches. Delete with the command shown.",
    ),
    ("backups", "Backups. Never deleted automatically."),
    (
        "codex_releases",
        "Old Codex releases. Keep current and the previous one.",
    ),
    ("playwright", "Superseded ms-playwright builds."),
];

pub struct Candidate {
    pub section: &'static str,
    /// The command to run, or the path to look at.
    pub item: String,
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Error("HOME is not set".into()))
}

/// A word the shell reads back unchanged: as is when it holds only safe characters, else in
/// single quotes. Branch names come from other people's PRs and may hold `;` or `$(`.
fn quote(word: &str) -> String {
    let safe = |ch: char| ch.is_ascii_alphanumeric() || "_-./@:+,=%".contains(ch);
    if !word.is_empty() && word.chars().all(safe) {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// Merged worktrees and branches under the configured roots, each with the command that
/// removes it.
fn merged(found: &mut Vec<Candidate>) -> Result<()> {
    let mut branches = Vec::new();
    for work in prune::candidates()? {
        let repo = quote(&work.repo.display().to_string());
        match (work.worktree, work.branch) {
            (Some(path), _) => found.push(Candidate {
                section: "worktrees",
                item: format!(
                    "agt worktree remove {} --yes",
                    quote(&path.display().to_string())
                ),
            }),
            (None, Some(branch)) => branches.push(Candidate {
                section: "branches",
                item: format!(
                    "cd {repo} && agt worktree cleanup --yes  # branch {}",
                    quote(&branch)
                ),
            }),
            (None, None) => {}
        }
    }
    found.append(&mut branches);
    Ok(())
}

/// Everything in the backups directory except agt's own backups, which the `backups` kind
/// handles.
fn backups(found: &mut Vec<Candidate>) -> Result<()> {
    let store = Store::open(Namespace::Reviews)?;
    let own: Vec<PathBuf> = store
        .agt_backups()
        .unwrap_or_default()
        .into_iter()
        .map(|backup| backup.path)
        .collect();
    let Ok(entries) = fs::read_dir(store.backups_dir()) else {
        return Ok(());
    };
    let mut sized: Vec<(u64, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| !own.contains(path))
        .map(|path| (measure(&path).map_or(0, |(_, bytes)| bytes), path))
        .collect();
    sized.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    for (bytes, path) in sized {
        found.push(Candidate {
            section: "backups",
            item: format!("{:.1} MiB {}", bytes as f64 / 1_048_576.0, path.display()),
        });
    }
    Ok(())
}

/// Splits a name into text and number runs, so `1.10` sorts after `1.9` as `sort -V` does.
fn version_key(name: &str) -> Vec<(String, u64)> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut digits = String::new();
    for ch in name.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            if !digits.is_empty() {
                parts.push((
                    std::mem::take(&mut text),
                    digits.parse().unwrap_or(u64::MAX),
                ));
                digits.clear();
            }
            text.push(ch);
        }
    }
    parts.push((text, digits.parse().unwrap_or(0)));
    parts
}

fn codex_releases(home: &Path, found: &mut Vec<Candidate>) {
    let standalone = home.join(".codex").join("packages").join("standalone");
    let releases = standalone.join("releases");
    let Ok(entries) = fs::read_dir(&releases) else {
        return;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort_by_key(|name| version_key(name));
    let current = fs::read_link(standalone.join("current")).ok();
    let current_index = names
        .iter()
        .position(|name| current.as_deref() == Some(releases.join(name).as_path()));
    for (index, name) in names.iter().enumerate() {
        let keep = current_index.is_some_and(|at| index == at || index + 1 == at);
        if !keep {
            found.push(Candidate {
                section: "codex_releases",
                item: releases.join(name).display().to_string(),
            });
        }
    }
}

/// Playwright's browser cache: `$PLAYWRIGHT_BROWSERS_PATH`, else its per-OS default
/// (`~/Library/Caches` on macOS, `$XDG_CACHE_HOME` or `~/.cache` elsewhere).
fn playwright_roots(home: &Path) -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH").filter(|p| !p.is_empty()) {
        return vec![PathBuf::from(path)];
    }
    let mut roots = vec![home.join("Library").join("Caches").join("ms-playwright")];
    if let Ok(cache) = super::files::cache_root() {
        roots.push(cache.join("ms-playwright"));
    }
    roots.push(home.join(".cache").join("ms-playwright"));
    roots.dedup();
    roots
}

fn playwright(home: &Path, found: &mut Vec<Candidate>) {
    for root in playwright_roots(home) {
        playwright_in(&root, found);
    }
}

fn playwright_in(root: &Path, found: &mut Vec<Candidate>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut builds: Vec<(String, u64)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let (browser, build) = name.rsplit_once('-')?;
            let tail = browser.chars().last()?;
            (tail.is_ascii_lowercase() || tail == '_')
                .then(|| Some((browser.to_owned(), build.parse().ok()?)))
                .flatten()
        })
        .collect();
    builds.sort();
    for pair in builds.windows(2) {
        if pair[0].0 == pair[1].0 {
            found.push(Candidate {
                section: "playwright",
                item: root
                    .join(format!("{}-{}", pair[0].0, pair[0].1))
                    .display()
                    .to_string(),
            });
        }
    }
}

pub fn candidates() -> Result<Vec<Candidate>> {
    let mut found = Vec::new();
    merged(&mut found)?;
    backups(&mut found)?;
    let home = home()?;
    codex_releases(&home, &mut found);
    playwright(&home, &mut found);
    Ok(found)
}

pub fn json(candidates: &[Candidate]) -> Value {
    let mut map = serde_json::Map::new();
    for (section, _) in SECTIONS {
        let items: Vec<&str> = candidates
            .iter()
            .filter(|candidate| candidate.section == section)
            .map(|candidate| candidate.item.as_str())
            .collect();
        map.insert(section.to_owned(), json!(items));
    }
    Value::Object(map)
}

/// `$XDG_STATE_HOME/agents/reports/cleanup-candidates.md`.
pub fn report_path() -> Result<PathBuf> {
    Ok(Store::open(Namespace::Reviews)?
        .agents_dir()
        .join("reports")
        .join("cleanup-candidates.md"))
}

/// Writes the report through a temporary file, so a reader never sees half of it.
pub fn save(candidates: &[Candidate], now: i64) -> Result<PathBuf> {
    let path = report_path()?;
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(ctx("create the reports directory"))?;
    let mut text = format!(
        "# Cleanup candidates\n\n{COUNT_PREFIX}{} -->\nGenerated {}. List only; nothing here is deleted automatically.\n",
        candidates.len(),
        crate::core::clock::format(now)
    );
    for (section, heading) in SECTIONS {
        text.push_str(&format!("\n## {heading}\n"));
        for candidate in candidates.iter().filter(|c| c.section == section) {
            text.push_str(&format!("- {}\n", candidate.item));
        }
    }
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(ctx("create the report"))?;
    std::io::Write::write_all(&mut file, text.as_bytes()).map_err(ctx("write the report"))?;
    file.persist(&path)
        .map_err(|error| Error(format!("save {}: {}", path.display(), error.error)))?;
    Ok(path)
}

/// The count the last saved report holds, `None` when there is no report.
pub fn saved_count() -> Result<Option<(usize, PathBuf)>> {
    let path = report_path()?;
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(None);
    };
    Ok(text
        .lines()
        .find_map(|line| {
            line.strip_prefix(COUNT_PREFIX)?
                .strip_suffix(" -->")?
                .parse()
                .ok()
        })
        .map(|count| (count, path)))
}

#[cfg(test)]
mod tests {
    use super::version_key;

    #[test]
    fn commands_quote_what_the_shell_would_read() {
        assert_eq!(super::quote("feat/a-b"), "feat/a-b");
        assert_eq!(super::quote("a;b"), "'a;b'");
        assert_eq!(super::quote("c$(d)"), "'c$(d)'");
        assert_eq!(super::quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn versions_sort_by_number() {
        let mut names = vec!["0.10.0", "0.9.1", "0.9.0"];
        names.sort_by_key(|name| version_key(name));
        assert_eq!(names, ["0.9.0", "0.9.1", "0.10.0"]);
    }
}
