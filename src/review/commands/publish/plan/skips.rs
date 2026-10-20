//! Why something is not posted, and the checks that decide it.

use serde_json::{Value, json};

use super::{Planner, allowed, comment_in_run};
use crate::review::commands::view::author_label;
use crate::store::model::{Comment, Pass, PassKind, Target, Thread};

/// Why something is not posted.
#[derive(Clone)]
pub(super) enum Skip {
    NotNamed,
    /// `--only` names threads, so the summary is held.
    OnlyThreads,
    /// The PR/MR is someone else's (`--not-own`).
    Foreign {
        whose: String,
        held: Held,
    },
    PassOpen {
        run: String,
        pass: String,
    },
    PassLocked {
        run: String,
        pass: String,
        cycle: u32,
    },
    NotAllowed {
        author: String,
        login: String,
    },
    BeforeLink {
        run: String,
        url: String,
    },
    OutsideDiff,
    HeadMoved {
        approved: String,
        head: String,
    },
    OpenFindings(Vec<String>),
    /// The service lacks a capability the item needs.
    Unsupported {
        service: &'static str,
        what: Unsupported,
    },
}

#[derive(Clone)]
pub(super) enum Held {
    Threads,
    Records,
}

#[derive(Clone)]
pub(super) enum Unsupported {
    SelfApproval { posted_as_comment: bool },
    FormalApproval,
    Resolve,
    Unresolve,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotNamed => write!(f, "not named by --only"),
            Self::OnlyThreads => write!(f, "--only posts named threads, not the summary"),
            Self::Foreign {
                whose,
                held: Held::Threads,
            } => write!(
                f,
                "{whose}: --not-own posts only replies and resolves on others' threads (name \
                 ours with --only)"
            ),
            Self::Foreign {
                whose,
                held: Held::Records,
            } => write!(f, "{whose}: the summary is never posted there"),
            Self::PassOpen { run, pass } => write!(f, "pass {run}.{pass} has not ended"),
            Self::PassLocked { run, pass, cycle } => write!(
                f,
                "pass {run}.{pass} is locked until a reveal pass in cycle {cycle} or the run ends"
            ),
            Self::NotAllowed { author, login } => write!(f, "written by {author}, not {login}"),
            Self::BeforeLink { run, url } => {
                write!(f, "run {run} ended before the target was linked to {url}")
            }
            Self::OutsideDiff => write!(
                f,
                "its lines are not in the PR/MR diff of the reviewed range"
            ),
            Self::HeadMoved { approved, head } => {
                write!(
                    f,
                    "it approves {approved}, but the PR/MR head is now {head}"
                )
            }
            Self::OpenFindings(ids) => {
                write!(f, "open findings block approval: {}", ids.join(", "))
            }
            Self::Unsupported { service, what } => match what {
                Unsupported::SelfApproval {
                    posted_as_comment: false,
                } => write!(f, "{service} does not let the author approve their own PR"),
                Unsupported::SelfApproval {
                    posted_as_comment: true,
                } => write!(
                    f,
                    "{service} does not let the author approve their own PR; posted as a comment"
                ),
                Unsupported::FormalApproval => {
                    write!(f, "{service} has no formal approval; posted as a comment")
                }
                Unsupported::Resolve => {
                    write!(f, "{service} cannot resolve threads; cleared locally")
                }
                Unsupported::Unresolve => {
                    write!(f, "{service} cannot reopen threads; cleared locally")
                }
            },
        }
    }
}

pub(in super::super) struct Skipped {
    pub(super) item: String,
    pub(super) reason: Skip,
}

impl Skipped {
    pub(super) fn new(item: &str, reason: Skip) -> Self {
        Self {
            item: item.to_owned(),
            reason,
        }
    }

    pub(in super::super) fn view(&self) -> Value {
        json!({ "item": self.item, "reason": self.reason.to_string() })
    }
}

impl Planner<'_> {
    /// Why a new thread is not posted: a finding of a pass still in progress, or another
    /// person's text.
    pub(super) fn blocked(
        &self,
        target: &Target,
        thread: &Thread,
        first: &Comment,
    ) -> Option<Skip> {
        if let Some(reason) = self.held(thread) {
            return Some(reason);
        }
        if let Some(reason) = self.pre_pr(target, thread) {
            return Some(reason);
        }
        if let Some(finding) = &thread.finding {
            let passes: Vec<&Pass> = target
                .runs
                .iter()
                .filter(|run| run.id == finding.run)
                .flat_map(|run| &run.passes)
                .collect();
            let pass = passes.iter().find(|pass| pass.id == finding.pass);
            let Some(pass) = pass.filter(|pass| pass.ended_at.is_some()) else {
                return Some(Skip::PassOpen {
                    run: finding.run.clone(),
                    pass: finding.pass.clone(),
                });
            };
            // A blind pass's findings stay hidden from the host until its reveal ends, or its
            // run ends when the harness cannot continue reviewers for a reveal.
            let run_ended = target
                .runs
                .iter()
                .any(|run| run.id == finding.run && run.ended_at.is_some());
            let revealed = run_ended
                || passes.iter().any(|later| {
                    later.kind == PassKind::Reveal
                        && later.cycle == pass.cycle
                        && later.number > pass.number
                        && later.ended_at.is_some()
                });
            if pass.locked && !revealed {
                return Some(Skip::PassLocked {
                    run: finding.run.clone(),
                    pass: finding.pass.clone(),
                    cycle: pass.cycle,
                });
            }
        }
        if !allowed(&first.author, &self.connection.login) {
            return Some(Skip::NotAllowed {
                author: author_label(&first.author),
                login: self.connection.login.clone(),
            });
        }
        None
    }

    /// Why a finding is not the PR/MR's business: its run ended before the target was linked
    /// to it, so it reviewed history from before the PR/MR.
    pub(super) fn pre_pr(&self, target: &Target, thread: &Thread) -> Option<Skip> {
        let run = &thread.finding.as_ref()?.run;
        self.run_before_pr(target, run)
    }

    pub(super) fn run_before_pr(&self, target: &Target, run_id: &str) -> Option<Skip> {
        let (publication, ended) = match target.runs.iter().find(|run| run.id == run_id) {
            Some(run) => (&run.publication_target, &run.ended_at),
            None => {
                let run = target.qa_runs.iter().find(|run| run.id == run_id)?;
                (&run.publication_target, &run.ended_at)
            }
        };
        // Retry eligibility MUST name this PR/MR so a later attachment cannot expose earlier runs.
        if publication.as_deref() == Some(self.review.url.as_str()) {
            return None;
        }
        let attached = self.review.attached_at.as_deref()?;
        let ended = ended.as_deref()?;
        // Timestamps share one fixed-width UTC format, so they order as text.
        (ended < attached).then(|| Skip::BeforeLink {
            run: run_id.to_owned(),
            url: self.review.url.clone(),
        })
    }

    /// Why a thread of ours is held: not named by `--only`, or on someone else's PR/MR
    /// with no `--only` naming threads to post there.
    pub(super) fn held(&self, thread: &Thread) -> Option<Skip> {
        self.unnamed(thread)
            .or_else(|| self.foreign_threads().filter(|_| !self.is_approval(thread)))
    }

    fn is_approval(&self, thread: &Thread) -> bool {
        self.approval == Some(thread.id.as_str())
    }

    pub(super) fn selected_thread(&self, thread: &Thread) -> bool {
        self.run.is_none_or(|run| {
            thread
                .finding
                .as_ref()
                .is_some_and(|finding| finding.run == run)
                || thread
                    .comments
                    .first()
                    .is_some_and(|comment| comment_in_run(comment, run))
                // A QA run's findings belong to it whoever wrote them.
                || thread.check.as_deref().is_some_and(|check| {
                    check
                        .strip_prefix(run)
                        .is_some_and(|rest| rest.starts_with('.'))
                })
        })
    }

    pub(super) fn unnamed(&self, thread: &Thread) -> Option<Skip> {
        (!self.only.is_empty() && !self.only.contains(&thread.id) && !self.is_approval(thread))
            .then_some(Skip::NotNamed)
    }

    /// Why our findings and local threads are held on someone else's PR/MR, unless `--only`
    /// names threads, which posts exactly those.
    pub(super) fn foreign_threads(&self) -> Option<Skip> {
        self.foreign()
            .filter(|_| self.only.is_empty())
            .map(|whose| Skip::Foreign {
                whose,
                held: Held::Threads,
            })
    }

    /// Whose PR/MR this is when it is not ours (`--not-own`): a final record there would
    /// speak for its author, and our findings and threads go out only when `--only` names them.
    pub(super) fn foreign(&self) -> Option<String> {
        let author = self.review.author.as_deref();
        (author != Some(self.connection.login.as_str())).then(|| {
            format!(
                "{} is authored by {}",
                self.review.url,
                author.unwrap_or("an unknown user")
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_reasons_read_as_they_are_posted() {
        let foreign = |held| Skip::Foreign {
            whose: "https://h/pull/1 is authored by bob".into(),
            held,
        };
        assert_eq!(
            foreign(Held::Records).to_string(),
            "https://h/pull/1 is authored by bob: the summary is never posted there"
        );
        assert_eq!(
            foreign(Held::Threads).to_string(),
            "https://h/pull/1 is authored by bob: --not-own posts only replies and resolves on \
             others' threads (name ours with --only)"
        );
        let head = Skip::HeadMoved {
            approved: "aaaaaaa".into(),
            head: "bbbbbbb".into(),
        };
        assert_eq!(
            head.to_string(),
            "it approves aaaaaaa, but the PR/MR head is now bbbbbbb"
        );
        let open = Skip::OpenFindings(vec!["f1".into(), "f2".into()]);
        assert_eq!(open.to_string(), "open findings block approval: f1, f2");
    }

    #[test]
    fn unsupported_capabilities_name_the_service() {
        let unsupported = |what| Skip::Unsupported {
            service: "Forgejo",
            what,
        };
        assert_eq!(
            unsupported(Unsupported::Resolve).to_string(),
            "Forgejo cannot resolve threads; cleared locally"
        );
        assert_eq!(
            unsupported(Unsupported::SelfApproval {
                posted_as_comment: true
            })
            .to_string(),
            "Forgejo does not let the author approve their own PR; posted as a comment"
        );
    }
}
