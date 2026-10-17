//! Leftovers that cleanup lists but never deletes, because they may hold work or another tool
//! owns them: merged worktrees and branches, backups, old Codex releases and superseded
//! Playwright browsers. `--yes` saves the list to a report, so a session can mention it
//! without scanning every repository again.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use super::files::measure;
use crate::core::error::{Error, Result, ctx};
use crate::store::{Namespace, Store};

const WORKTREE_IDLE_SECS: i64 = 3 * 86_400;
const COUNT_PREFIX: &str = "<!-- count: ";

pub const SECTIONS: [(&str, &str); 5] = [
    (
        "worktrees",
        "Merged, clean worktrees older than 3 days. Remove with the command shown. Never rm.",
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

/// `git -C <dir> <args>`, its trimmed stdout when it succeeds.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    })
}

/// Repositories at `~/projects/<owner>/<name>` (or `~/Projects`, once if they are the same
/// directory) with a `.git` directory.
fn repos() -> Result<Vec<PathBuf>> {
    let home = home()?;
    let mut found = Vec::new();
    for root in ["projects", "Projects"] {
        let Ok(owners) = fs::read_dir(home.join(root)) else {
            continue;
        };
        for owner in owners.flatten() {
            let Ok(names) = fs::read_dir(owner.path()) else {
                continue;
            };
            for name in names.flatten() {
                let repo = name.path();
                if repo.join(".git").is_dir()
                    && let Ok(real) = fs::canonicalize(&repo)
                {
                    found.push(real);
                }
            }
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

fn base(repo: &Path) -> String {
    git(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .filter(|base| !base.is_empty())
    .unwrap_or_else(|| "origin/main".into())
}

fn worktrees(repo: &Path, base: &str, now: i64, found: &mut Vec<Candidate>) {
    let Some(list) = git(repo, &["worktree", "list", "--porcelain"]) else {
        return;
    };
    let inside = repo.join(".worktrees");
    for line in list.lines() {
        let Some(path) = line.strip_prefix("worktree ") else {
            continue;
        };
        let worktree = PathBuf::from(path);
        if !worktree.starts_with(&inside) || !worktree.join(".git").exists() {
            continue;
        }
        let merged = Command::new("git")
            .arg("-C")
            .arg(&worktree)
            .args(["merge-base", "--is-ancestor", "HEAD", base])
            .output()
            .is_ok_and(|output| output.status.success());
        let clean = git(&worktree, &["--no-optional-locks", "status", "--porcelain"])
            .is_some_and(|status| status.is_empty());
        if !merged || !clean {
            continue;
        }
        let mut newest = git(&worktree, &["log", "-1", "--format=%ct"])
            .and_then(|stamp| stamp.parse().ok())
            .unwrap_or(0);
        if let Some(git_dir) = git(&worktree, &["rev-parse", "--absolute-git-dir"]) {
            for name in ["HEAD", "index"] {
                // Only the file itself: a tree walk of the git dir would be slow and pointless.
                if let Ok(meta) = fs::symlink_metadata(Path::new(&git_dir).join(name))
                    && let Ok(time) = meta.modified()
                    && let Ok(elapsed) = time.duration_since(std::time::UNIX_EPOCH)
                {
                    newest = newest.max(i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX));
                }
            }
        }
        if now - newest >= WORKTREE_IDLE_SECS {
            found.push(Candidate {
                section: "worktrees",
                item: format!(
                    "git -C {} worktree remove {}",
                    repo.display(),
                    worktree.display()
                ),
            });
        }
    }
}

fn branches(repo: &Path, base: &str, found: &mut Vec<Candidate>) {
    let checked: Vec<String> = git(repo, &["worktree", "list", "--porcelain"])
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.strip_prefix("branch refs/heads/"))
        .map(str::to_owned)
        .collect();
    let Some(merged) = git(
        repo,
        &["branch", "--merged", base, "--format=%(refname:short)"],
    ) else {
        return;
    };
    for branch in merged.lines().filter(|branch| !branch.is_empty()) {
        // `(HEAD detached at …)` is not a branch.
        if branch.starts_with('(')
            || matches!(branch, "main" | "master" | "develop")
            || checked.iter().any(|b| b == branch)
        {
            continue;
        }
        found.push(Candidate {
            section: "branches",
            item: format!("git -C {} branch -d {branch}", repo.display()),
        });
    }
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

fn playwright(home: &Path, found: &mut Vec<Candidate>) {
    let root = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH")
        .filter(|path| !path.is_empty())
        .map_or_else(|| home.join(".cache").join("ms-playwright"), PathBuf::from);
    let Ok(entries) = fs::read_dir(&root) else {
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

pub fn candidates(now: i64) -> Result<Vec<Candidate>> {
    let repos = repos()?;
    // Each repository costs several git runs; look at them in parallel, in repository order.
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut per_repo: Vec<(usize, Vec<Candidate>, Vec<Candidate>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(repo) = repos.get(index) else { break };
                        let base = base(repo);
                        let (mut trees, mut merged) = (Vec::new(), Vec::new());
                        worktrees(repo, &base, now, &mut trees);
                        branches(repo, &base, &mut merged);
                        done.push((index, trees, merged));
                    }
                    done
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    });
    per_repo.sort_by_key(|(index, _, _)| *index);
    let mut found = Vec::new();
    let mut merged = Vec::new();
    for (_, trees, branches) in per_repo {
        found.extend(trees);
        merged.extend(branches);
    }
    found.extend(merged);
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
    fn versions_sort_by_number() {
        let mut names = vec!["0.10.0", "0.9.1", "0.9.0"];
        names.sort_by_key(|name| version_key(name));
        assert_eq!(names, ["0.9.0", "0.9.1", "0.10.0"]);
    }
}
