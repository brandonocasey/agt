use serde::Deserialize;
use serde_json::{Value, json};

use crate::clock;
use crate::error::{Result, fail};
use crate::model::{Run, Usage};

/// Time and tokens of a run: wall time, time in passes (reviewing), time between passes and
/// after the last (fixing), and the tokens reviewers and the main session reported.
pub fn run_spend(run: &Run) -> Value {
    let secs = |text: &str| clock::parse(text).ok();
    let span = |from: &str, to: Option<&str>| match (secs(from), to.and_then(secs)) {
        (Some(from), Some(to)) => Some((to - from).max(0)),
        _ => None,
    };
    let reviewing: i64 = run
        .passes
        .iter()
        .filter_map(|pass| span(&pass.started_at, pass.ended_at.as_deref()))
        .sum();
    let total = span(&run.started_at, run.ended_at.as_deref());
    let reviewer_tokens: u64 = run
        .passes
        .iter()
        .flat_map(|pass| &pass.reviewers)
        .filter_map(|reviewer| reviewer.usage.as_ref()?.tokens)
        .sum();
    let main_tokens: u64 = run.usage.iter().filter_map(|role| role.usage.tokens).sum();
    json!({
        "total_secs": total,
        "reviewing_secs": reviewing,
        "fixing_secs": total.map(|total| (total - reviewing).max(0)),
        "reviewer_tokens": (reviewer_tokens > 0).then_some(reviewer_tokens),
        "main_tokens": (main_tokens > 0).then_some(main_tokens),
        "roles": run.usage,
    })
}

/// One agent's spend as `pass end` and `run end` read it: `reviewer` (pass) or `role` (run).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UsageInput {
    pub(super) reviewer: Option<String>,
    pub(super) role: Option<String>,
    pub(super) tokens: Option<u64>,
    pub(super) duration_ms: Option<u64>,
    pub(super) tool_uses: Option<u64>,
}

impl UsageInput {
    /// The `reviewer` (pass end) or `role` (run end), whichever `field` names.
    pub(super) fn who(&self, field: &str) -> Result<String> {
        let (wanted, other) = match field {
            "reviewer" => (&self.reviewer, &self.role),
            _ => (&self.role, &self.reviewer),
        };
        match (wanted, other) {
            (Some(name), None) => Ok(name.clone()),
            (_, Some(_)) if field == "reviewer" => fail("usage: pass end takes reviewer, not role"),
            (_, Some(_)) => fail("usage: run end takes role, not reviewer"),
            (None, None) => fail(format!("usage: every entry needs {field}")),
        }
    }

    pub(super) fn usage(&self) -> Usage {
        Usage {
            tokens: self.tokens,
            duration_ms: self.duration_ms,
            tool_uses: self.tool_uses,
        }
    }
}
