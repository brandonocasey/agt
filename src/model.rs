use clap::ValueEnum;
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone)]
pub struct Target {
    pub version: u32,
    pub revision: u64,
    pub key: String,
    pub repo: String,
    pub kind: TargetKind,
    pub branch: Option<String>,
    pub branch_birth: Option<String>,
    pub base: Option<String>,
    pub head: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub review: Option<Review>,
    pub runs: Vec<Run>,
    pub records: Vec<ImportedRecord>,
    pub threads: Vec<Thread>,
}

/// The PR or MR a target is linked to, as of the last `sync`.
#[derive(Serialize, Deserialize, Clone)]
pub struct Review {
    pub forge: Forge,
    pub host: String,
    pub repo: String,
    pub number: u64,
    pub url: String,
    pub state: ReviewState,
    pub closed_at: Option<String>,
    pub head_branch: String,
    pub head_sha: String,
    pub base_sha: Option<String>,
    pub start_sha: Option<String>,
    pub author: Option<String>,
    pub login: String,
    pub synced_at: String,
    /// When a sync first linked the target to this PR/MR; `publish` never posts runs that
    /// ended earlier. Absent in stores written before it existed.
    #[serde(default)]
    pub attached_at: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Forge {
    Github,
    Gitlab,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewState {
    Open,
    Closed,
    Merged,
}

/// A trusted `review-record v2` comment from the PR/MR.
#[derive(Serialize, Deserialize, Clone)]
pub struct ImportedRecord {
    pub run: String,
    pub head: String,
    pub reviews_at_head: u32,
    pub open: Option<u32>,
    pub comment_id: String,
    pub url: Option<String>,
    pub created_at: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    Branch,
    Range,
    Commit,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Run {
    pub id: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub passes: Vec<Pass>,
    /// `HEAD` at `run end`; commits after the last pass up to it count as reviewed (`repair`).
    pub repaired_head: Option<String>,
    pub repair: Vec<CommitPatch>,
    pub reveal: Option<Reveal>,
    pub record: Option<RecordPost>,
    /// Usage of the main session's roles (`manual`, `fix`, `main`), from `run end --usage`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usage: Vec<RoleUsage>,
}

/// Tokens, time and tool calls an agent spent, as its harness reports them.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_uses: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RoleUsage {
    pub role: String,
    #[serde(flatten)]
    pub usage: Usage,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct Reveal {
    pub agree: u32,
    pub disagree: u32,
}

/// Publish state of a run's final record.
#[derive(Serialize, Deserialize, Clone)]
pub struct RecordPost {
    pub post: PostState,
    pub attempt: Option<String>,
    /// The body last sent; GitLab shows no edits, so sync trusts only this text.
    #[serde(default)]
    pub sent: Option<String>,
    pub head: String,
    pub remote_id: Option<String>,
    pub url: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Pass {
    pub id: String,
    pub cycle: u32,
    pub number: u32,
    pub kind: PassKind,
    pub base: Option<String>,
    pub head: String,
    pub tree: String,
    pub dirty: bool,
    pub commits: Vec<CommitPatch>,
    pub range_patch_id: Option<String>,
    pub head_patch_id: Option<String>,
    /// The pass started after the target merge-base and did not continue a whole-branch pass:
    /// it covers the commits it reviewed, never the whole head. Missing in earlier builds.
    #[serde(default)]
    pub partial: bool,
    pub reviewers: Vec<Reviewer>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub locked: bool,
    pub major_found: bool,
    pub killed: Vec<KilledFinding>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum PassKind {
    Full,
    FixCheck,
    Reveal,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CommitPatch {
    pub sha: String,
    pub patch_id: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Reviewer {
    pub id: String,
    pub model: String,
    pub effort: String,
    pub harness: String,
    pub harness_version: Option<String>,
    pub starting_context: String,
    /// What the reviewer spent on this pass, from `pass end --findings`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Thread {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ThreadType,
    pub severity: Option<Severity>,
    pub anchor: Option<Anchor>,
    pub resolved: bool,
    pub resolution: Option<String>,
    pub triage: Option<Triage>,
    pub held: bool,
    pub finding: Option<FindingInfo>,
    pub comments: Vec<Comment>,
    pub remote: Option<RemoteThread>,
    /// A local resolve or reopen that `publish` has not sent yet; `sync` keeps it.
    pub resolve_pending: Option<bool>,
    pub deleted_remote: bool,
    pub deleted_seen_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// An approval of `head`: `publish` posts it as a comment, or with `formal` as the
    /// host's approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<Approval>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Approval {
    pub head: String,
    pub formal: bool,
    /// GitLab accepted the formal approval; a rerun does not send it again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub approved: bool,
}

/// Host-side state of a thread. `resolvable` is false for general comments and review bodies.
#[derive(Serialize, Deserialize, Clone)]
pub struct RemoteThread {
    pub kind: RemoteKind,
    pub id: String,
    pub url: Option<String>,
    pub resolvable: bool,
    pub outdated: Option<bool>,
    pub path: Option<String>,
    pub old_path: Option<String>,
    pub line: Option<u32>,
    pub old_line: Option<u32>,
    pub original_line: Option<u32>,
    pub start_line: Option<u32>,
    pub original_start_line: Option<u32>,
    pub side: Option<Side>,
    pub commit: Option<String>,
    pub review_state: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteKind {
    ReviewThread,
    Review,
    General,
    Discussion,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ThreadType {
    Bug,
    Question,
    Requirement,
    Nit,
    Comment,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    High,
    Medium,
    Low,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Triage {
    Fixed,
    Stale,
    Valid,
    Question,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Side {
    New,
    Old,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Anchor {
    pub path: String,
    pub line: Option<u32>,
    pub old_line: Option<u32>,
    pub side: Side,
    pub start_line: Option<u32>,
    pub commit: Option<String>,
    pub tree: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct FindingInfo {
    pub run: String,
    pub pass: String,
    pub reviewer: String,
    pub evidence: Option<String>,
    pub tests_run: Vec<String>,
    pub post_reveal: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct KilledFinding {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ThreadType,
    pub severity: Option<Severity>,
    pub anchor: Option<Anchor>,
    pub scenario: String,
    pub evidence: Option<String>,
    pub tests_run: Vec<String>,
    pub post_reveal: bool,
    pub reason: String,
    pub created_at: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Comment {
    /// `<thread id>/c<n>` for local comments, `remote-<id>` for imported ones; the
    /// `agent-review id=` marker of a posted body.
    pub id: String,
    pub author: Author,
    pub body: String,
    pub created_at: String,
    pub post: PostState,
    pub remote: Option<RemoteComment>,
    pub draft_id: Option<String>,
    pub attempt: Option<String>,
    /// The body last sent; GitLab shows no edits, so sync trusts only this text.
    #[serde(default)]
    pub sent: Option<String>,
    /// Kept in the store only (`reply --local`), such as the user's answer to a held
    /// question; `publish` never posts it.
    #[serde(default)]
    pub local: bool,
    pub deleted_remote: bool,
    pub deleted_seen_at: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PostState {
    Pending,
    Posting,
    Posted,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RemoteComment {
    pub id: String,
    pub url: Option<String>,
    pub login: Option<String>,
    pub is_me: bool,
    pub editor: Option<String>,
    pub review_id: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Author {
    Human {
        login: String,
        /// The login is git `user.name`, written before any sync; sync replaces it with the
        /// forge login. Missing in target files from builds before the field existed.
        #[serde(default)]
        git_name: bool,
    },
    Agent {
        model: String,
        effort: String,
        harness: String,
        harness_version: Option<String>,
        run: Option<String>,
        pass: Option<String>,
        reviewer: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_authors_from_earlier_builds_parse() {
        let author: Author = serde_json::from_str(r#"{"kind":"human","login":"me"}"#).unwrap();
        assert!(matches!(
            author,
            Author::Human {
                git_name: false,
                ..
            }
        ));
    }
}
