//! Whether a branch's work landed in the default branch, and the evidence for it.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::info::{self, Entry};
use crate::core::error::Result;
use crate::forge::landed::{Origin, State};
use crate::git::Repo;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The tip is an ancestor of the default branch.
    Merged,
    /// A merged PR/MR holds the tip.
    SquashMerged,
    Open,
    Closed,
    Diverged,
    Unknown,
    Unmerged,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::SquashMerged => "squash-merged",
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Diverged => "diverged",
            Self::Unknown => "unknown",
            Self::Unmerged => "unmerged",
        }
    }

    /// Whether the work is safe to delete.
    pub fn landed(self) -> bool {
        matches!(self, Self::Merged | Self::SquashMerged)
    }
}

pub struct Finding {
    pub verdict: Verdict,
    pub evidence: String,
    /// Fields that join the row of a kept branch: the closed PR/MR's `head`, `title` and
    /// `closed_at`, else null.
    pub details: Value,
}

fn finding(verdict: Verdict, evidence: impl Into<String>) -> Finding {
    Finding {
        verdict,
        evidence: evidence.into(),
        details: Value::Null,
    }
}

/// A repository's worktrees and default branch, and what must never be touched.
pub struct Scene {
    pub entries: Vec<Entry>,
    /// The default branch's name and ref.
    pub name: String,
    pub reference: String,
    /// The worktree holding the current directory, if one of this repository does.
    held: Option<PathBuf>,
}

impl Scene {
    /// `None` when the repository has no default branch.
    pub fn read(repo: &Repo) -> Result<Option<Self>> {
        let Some((name, reference)) = info::default_branch(repo)? else {
            return Ok(None);
        };
        let entries = info::entries(repo)?;
        let cwd = std::env::current_dir().and_then(|cwd| cwd.canonicalize());
        let held = cwd.ok().and_then(|cwd| {
            entries
                .iter()
                .filter_map(|entry| info::real(&entry.path).ok())
                .filter(|path| cwd.starts_with(path))
                .max_by_key(|path| path.components().count())
        });
        Ok(Some(Self {
            entries,
            name,
            reference,
            held,
        }))
    }

    pub fn main(&self) -> &Entry {
        &self.entries[0]
    }

    /// Whether the current directory is in the worktree at `path`.
    pub fn holds_cwd(&self, path: &Path) -> bool {
        self.held
            .as_deref()
            .is_some_and(|held| info::real(path).is_ok_and(|real| real == held))
    }

    /// The default branch, `main`, `master` and `develop` by name, the main checkout's branch
    /// and the branch of the worktree that holds the current directory.
    pub fn protected(&self, branch: &str) -> bool {
        branch == self.name
            || ["main", "master", "develop"].contains(&branch)
            || self.main().branch.as_deref() == Some(branch)
            || self
                .entries
                .iter()
                .any(|entry| entry.branch.as_deref() == Some(branch) && self.holds_cwd(&entry.path))
    }

    /// Local branches and their tips, without the protected ones.
    pub fn branches(&self, repo: &Repo) -> Result<Vec<(String, String)>> {
        let listing = repo.git(&[
            "for-each-ref",
            "--format=%(refname)%09%(objectname)",
            "refs/heads",
        ])?;
        Ok(listing
            .lines()
            .filter_map(|line| {
                let (name, tip) = line.split_once('\t')?;
                Some((name.strip_prefix("refs/heads/")?.to_owned(), tip.to_owned()))
            })
            .filter(|(name, _)| !self.protected(name))
            .collect())
    }

    /// The linked worktree that has `branch` checked out.
    pub fn worktree_of(&self, branch: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .skip(1)
            .find(|entry| entry.branch.as_deref() == Some(branch))
    }

    /// Whether a commit is in the default branch.
    pub fn contains(&self, repo: &Repo, commit: &str) -> Result<bool> {
        repo.is_ancestor(commit, &self.reference)
    }

    /// The verdict for `branch` at `tip`: local ancestry first, then, with an `origin`, its
    /// PRs/MRs for the branch.
    pub fn judge(
        &self,
        repo: &Repo,
        origin: Option<&Origin>,
        branch: &str,
        tip: &str,
    ) -> Result<Finding> {
        if self.contains(repo, tip)? {
            return Ok(finding(
                Verdict::Merged,
                format!("tip is an ancestor of {}", self.name),
            ));
        }
        let Some(origin) = origin else {
            return Ok(finding(
                Verdict::Unmerged,
                format!(
                    "not an ancestor of {}; no forge evidence checked",
                    self.name
                ),
            ));
        };
        let pulls = match origin.pulls(branch) {
            Ok(pulls) => pulls,
            Err(error) => return Ok(finding(Verdict::Unknown, format!("forge lookup: {error}"))),
        };
        if let Some(open) = pulls.iter().find(|pull| pull.state == State::Open) {
            return Ok(finding(
                Verdict::Open,
                format!("open PR #{} {}", open.number, open.url),
            ));
        }
        let mut merged: Vec<_> = pulls
            .iter()
            .filter(|pull| pull.state == State::Merged)
            .collect();
        merged.sort_by_key(|pull| std::cmp::Reverse(pull.number));
        let (mut unknown, mut diverged, mut foreign) = (None, None, None);
        for pull in merged {
            if pull.base != self.name || pull.head_branch != branch || pull.head_sha.is_empty() {
                continue;
            }
            if !pull.same_repo {
                foreign.get_or_insert(pull.number);
                continue;
            }
            if let Err(why) = self.fetch_head(repo, origin, pull.number, &pull.head_sha) {
                unknown.get_or_insert(format!("PR #{}: {why}", pull.number));
                continue;
            }
            if tip == pull.head_sha || repo.is_ancestor(tip, &pull.head_sha)? {
                return Ok(finding(
                    Verdict::SquashMerged,
                    format!(
                        "merged PR #{} into {}, head {}: {}",
                        pull.number, self.name, pull.head_sha, pull.url
                    ),
                ));
            }
            diverged.get_or_insert(pull.number);
        }
        if let Some(why) = unknown {
            return Ok(finding(Verdict::Unknown, why));
        }
        if let Some(number) = diverged {
            return Ok(finding(
                Verdict::Diverged,
                format!("diverged from merged PR #{number}"),
            ));
        }
        if let Some(unknown) = pulls.iter().find(|pull| pull.state == State::Unknown) {
            return Ok(finding(
                Verdict::Unknown,
                format!("PR #{} is in a state agt does not know", unknown.number),
            ));
        }
        if let Some(closed) = pulls.iter().find(|pull| pull.state == State::Closed) {
            let mut found = finding(
                Verdict::Closed,
                format!(
                    "PR #{} was closed without merging: {}",
                    closed.number, closed.url
                ),
            );
            found.details = json!({
                "head": closed.head_sha,
                "title": closed.title,
                "closed_at": closed.closed_at,
            });
            return Ok(found);
        }
        let note = match foreign {
            Some(number) => format!("only the cross-repository PR #{number} merged"),
            None => "no merged PR".to_owned(),
        };
        Ok(finding(
            Verdict::Unmerged,
            format!("not an ancestor of {}; {note}", self.name),
        ))
    }

    /// Makes sure the head commit of a merged PR/MR is in the repository, fetching its ref from
    /// origin when it is not.
    fn fetch_head(
        &self,
        repo: &Repo,
        origin: &Origin,
        number: u64,
        sha: &str,
    ) -> std::result::Result<(), String> {
        let spec = format!("{sha}^{{commit}}");
        if repo.object_exists(&spec).map_err(|e| e.to_string())? {
            return Ok(());
        }
        let reference = origin.head_ref(number);
        repo.git(&[
            "fetch",
            "--no-tags",
            "--quiet",
            "--refmap=",
            "origin",
            &reference,
        ])
        .map_err(|error| error.to_string())?;
        let fetched = repo
            .git(&["rev-parse", "FETCH_HEAD"])
            .map_err(|e| e.to_string())?;
        if fetched == sha {
            Ok(())
        } else {
            Err(format!("{reference} is at {fetched}, not the head {sha}"))
        }
    }
}

/// How long a worktree must sit untouched before cleanup removes it.
const QUIET_SECS: i64 = 3 * 86_400;

/// Whether anything in the worktree changed in the last 3 days before `now`: its HEAD commit,
/// its git dir's HEAD, index or HEAD log, or a tracked or untracked non-ignored file.
pub fn recent(path: &Path, now: i64) -> Result<bool> {
    let repo = Repo::at(path);
    let since = |secs: i64| now - secs < QUIET_SECS;
    let committed = repo.git(&["log", "-1", "--format=%ct", "HEAD"])?;
    if committed.parse().is_ok_and(since) {
        return Ok(true);
    }
    let touched = |file: &Path| {
        std::fs::symlink_metadata(file)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|age| since(i64::try_from(age.as_secs()).unwrap_or(i64::MAX)))
    };
    let git_dir = PathBuf::from(repo.git(&["rev-parse", "--absolute-git-dir"])?);
    if ["HEAD", "index", "logs/HEAD"]
        .iter()
        .any(|name| touched(&git_dir.join(name)))
    {
        return Ok(true);
    }
    let listing = repo.checked(
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        None,
        &[],
    )?;
    Ok(listing
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .any(|name| touched(&path.join(String::from_utf8_lossy(name).as_ref()))))
}
