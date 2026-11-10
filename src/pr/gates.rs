//! The gates of a local auto-merge that other domains judge. `pr` MUST NOT import them, so the
//! command line hands in the way to ask.

use crate::core::error::Result;

/// What the review and check domains tell the gates.
pub trait Gates {
    /// Whether `head`'s tree has a recorded `agt check run` pass with its log.
    fn checked(&self, head: &str) -> Result<bool>;

    /// The review of local `branch` at `head` against `target`, wherever the command runs.
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
            let (noun, verb) = if review.unreviewed == 1 {
                ("commit", "is")
            } else {
                ("commits", "are")
            };
            why.push(format!("{} {noun} {verb} unreviewed", review.unreviewed));
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(usize);

    impl Gates for Fixed {
        fn checked(&self, _head: &str) -> Result<bool> {
            Ok(true)
        }

        fn review(&self, _branch: &str, _head: &str, _target: &str) -> Result<Review> {
            Ok(Review {
                reviewed: true,
                unreviewed: self.0,
                blocking: Vec::new(),
                approved: true,
            })
        }
    }

    #[test]
    fn the_review_gate_counts_one_commit_and_several() {
        for (unreviewed, text) in [
            (1, "review: 1 commit is unreviewed"),
            (2, "review: 2 commits are unreviewed"),
        ] {
            let failing = failing(&Fixed(unreviewed), "feat", "head", "main").unwrap();
            assert_eq!(failing, [text]);
        }
    }
}
