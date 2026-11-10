//! The gates of a local auto-merge that other domains judge. `pr` may not import them, so the
//! command line hands in the way to ask.

use crate::core::error::Result;

/// What the review and check domains tell the gates.
pub trait Gates {
    /// Whether an `agt check run` pass is recorded for the tree of `head`, with its log still
    /// present.
    fn checked(&self, head: &str) -> Result<bool>;

    /// The review of local branch `branch` at `head` against the local `target`, wherever the
    /// command runs.
    fn review(&self, branch: &str, head: &str, target: &str) -> Result<Review>;
}

/// The state of a review at one head.
pub struct Review {
    pub reviewed: bool,
    /// How many commits since the base no review covers.
    pub unreviewed: usize,
    /// Ids of the open findings that block an approval.
    pub blocking: Vec<String>,
    /// An approval is recorded at exactly the head.
    pub approved: bool,
}

/// The gates that fail for `branch` at `head`, each as `name: why`; empty when they pass.
pub fn failing(gates: &dyn Gates, branch: &str, head: &str, target: &str) -> Result<Vec<String>> {
    let mut failing = Vec::new();
    if !gates.checked(head)? {
        failing.push("checks: no `agt check run` pass is recorded for this tree".to_owned());
    }
    let review = gates.review(branch, head, target)?;
    if !review.reviewed || review.unreviewed > 0 || !review.blocking.is_empty() {
        let mut why = Vec::new();
        if !review.reviewed {
            why.push("no review covers this head".to_owned());
        }
        if review.unreviewed > 0 {
            why.push(format!("{} commits are unreviewed", review.unreviewed));
        }
        if !review.blocking.is_empty() {
            why.push(format!("open findings {}", review.blocking.join(", ")));
        }
        failing.push(format!("review: {}", why.join("; ")));
    }
    if !review.approved {
        failing.push("approval: none is recorded at this head".to_owned());
    }
    Ok(failing)
}
