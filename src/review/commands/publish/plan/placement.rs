//! Where an anchored comment goes in the PR/MR diff.

use super::Planner;
use crate::forge::anchors;
use crate::forge::service::{Placement, Position};
use crate::review::commands::publish::render::location;
use crate::store::model::{Anchor, Side};

/// Where a comment goes on the host.
pub(super) enum Spot {
    InDiff(Position),
    /// No anchor: a general comment.
    General,
    /// A file the PR/MR does not change: a general comment naming the location.
    OtherFile(String),
    /// Lines of a changed file that are not placeable in the PR/MR diff: a file-level thread
    /// where the host has one, else never posted.
    Outside,
}

impl Planner<'_> {
    /// The place of an anchored comment in the PR/MR diff. A file the PR/MR does not change,
    /// or a finding on a whole file, gets a general comment; lines of a changed file that fit
    /// no hunk are outside.
    pub(super) fn spot(&self, anchor: &Anchor) -> Spot {
        let wanted = match anchor.side {
            Side::New => anchor.line,
            Side::Old => anchor.old_line,
        };
        let single;
        let anchor = if self.caps().ranges {
            anchor
        } else {
            single = Anchor {
                start_line: None,
                ..anchor.clone()
            };
            &single
        };
        let placed = match (&anchor.commit, wanted) {
            (Some(commit), Some(line)) => {
                self.service()
                    .place(self.repo, self.review, anchor, commit, line)
            }
            _ => None,
        };
        match placed {
            Some(Placement::InDiff(position)) => Spot::InDiff(position),
            Some(Placement::OtherFile) => Spot::OtherFile(location(anchor)),
            None if wanted.is_none() || !self.in_pr_diff(&anchor.path) => {
                Spot::OtherFile(location(anchor))
            }
            None => Spot::Outside,
        }
    }

    /// Whether the PR/MR diff changes this file; unknown counts as changed, so nothing
    /// unplaceable is posted as a general comment.
    pub(super) fn in_pr_diff(&self, path: &str) -> bool {
        let files = self.pr_files();
        files.is_empty() || files.iter().any(|file| file.touches(path))
    }

    /// Whether the PR/MR diff is known and changes this file.
    pub(super) fn known_in_pr_diff(&self, path: &str) -> bool {
        self.pr_files().iter().any(|file| file.touches(path))
    }

    fn pr_files(&self) -> &[anchors::FileDiff] {
        self.pr_files.get_or_init(|| {
            let head = &self.review.head_sha;
            let diff = self
                .review
                .base_sha
                .as_deref()
                .and_then(|base| self.repo.merge_base(base, head).ok().flatten())
                .and_then(|base| self.repo.diff_u0(&base, head).ok());
            diff.map(|diff| anchors::parse(&diff)).unwrap_or_default()
        })
    }
}
