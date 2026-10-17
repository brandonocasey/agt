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

/// The crash-safe posting state of something `publish` sends: marked `Posting` with an attempt
/// id and the text before the call, so a rerun can match a write whose response was lost.
#[derive(Serialize, Deserialize, Clone)]
pub struct Delivery {
    pub post: PostState,
    pub attempt: Option<String>,
    /// The body last sent; a service that shows no edits is trusted only for this text.
    #[serde(default)]
    pub sent: Option<String>,
}

impl Delivery {
    pub fn new(post: PostState) -> Self {
        Self {
            post,
            attempt: None,
            sent: None,
        }
    }

    /// Marks the item as being sent in `attempt`, before the call is made.
    pub fn begin(&mut self, attempt: &str, sent: Option<String>) {
        self.post = PostState::Posting;
        self.attempt = Some(attempt.to_owned());
        self.sent = sent;
    }

    pub fn is_posting(&self) -> bool {
        self.post == PostState::Posting
    }

    /// Whether a marker naming `attempt` is the write of this item's attempt.
    pub fn is_attempt(&self, attempt: Option<&str>) -> bool {
        self.attempt.is_some() && self.attempt.as_deref() == attempt
    }
}

/// Publish state of a run's final record.
#[derive(Serialize, Deserialize, Clone)]
pub struct RecordPost {
    #[serde(flatten)]
    pub delivery: Delivery,
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
    pub resolution: Option<Resolution>,
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

impl Thread {
    /// A thread with no host state, resolution or approval yet.
    pub fn new(
        id: String,
        kind: ThreadType,
        comments: Vec<Comment>,
        created_at: String,
        updated_at: String,
    ) -> Self {
        Self {
            id,
            kind,
            severity: None,
            anchor: None,
            resolved: false,
            resolution: None,
            triage: None,
            held: false,
            finding: None,
            comments,
            remote: None,
            resolve_pending: None,
            deleted_remote: false,
            deleted_seen_at: None,
            created_at,
            updated_at,
            approval: None,
        }
    }

    /// Applies the host's resolved state, keeping `resolution` consistent: a thread the host
    /// reopened no longer carries the outcome of its resolving.
    pub fn set_remote_resolved(&mut self, resolved: bool) {
        self.resolved = resolved;
        if !resolved && self.resolution.as_ref().is_some_and(Resolution::resolves) {
            self.resolution = None;
        }
    }
}

/// How a thread was resolved or answered. Stored as the free text older versions wrote, so
/// files stay compatible: `fixed <sha>`, `not fixed: <reason>`, `rejected: <reason>`, `stale`
/// and `answered`. Any other text is kept as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    Fixed { sha: String },
    NotFixed { reason: String },
    Rejected { reason: String },
    Stale,
    Answered,
    Other(String),
}

impl Resolution {
    pub fn parse(text: &str) -> Self {
        if text == "stale" {
            return Self::Stale;
        }
        if text == "answered" {
            return Self::Answered;
        }
        if let Some(reason) = text.strip_prefix("not fixed: ") {
            return Self::NotFixed {
                reason: reason.to_owned(),
            };
        }
        if let Some(reason) = text.strip_prefix("rejected: ") {
            return Self::Rejected {
                reason: reason.to_owned(),
            };
        }
        if let Some(sha) = text.strip_prefix("fixed ") {
            return Self::Fixed {
                sha: sha.to_owned(),
            };
        }
        Self::Other(text.to_owned())
    }

    /// The outcome of a resolve, as opposed to a note left on a reopened thread.
    pub fn resolves(&self) -> bool {
        !matches!(self, Self::NotFixed { .. })
    }

    /// A finding that will never be posted: judged stale or rejected.
    pub fn withdrawn(&self) -> bool {
        matches!(self, Self::Stale | Self::Rejected { .. })
    }
}

impl std::fmt::Display for Resolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fixed { sha } => write!(f, "fixed {sha}"),
            Self::NotFixed { reason } => write!(f, "not fixed: {reason}"),
            Self::Rejected { reason } => write!(f, "rejected: {reason}"),
            Self::Stale => f.write_str("stale"),
            Self::Answered => f.write_str("answered"),
            Self::Other(text) => f.write_str(text),
        }
    }
}

impl Serialize for Resolution {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Resolution {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::parse(&String::deserialize(deserializer)?))
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Approval {
    pub head: String,
    pub formal: bool,
    /// GitLab accepted the formal approval; a rerun does not send it again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub approved: bool,
    /// Why the host refused the formal approval; `approve --formal` clears it to retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
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

impl RemoteThread {
    /// Host state with no position, as before a thread is anchored.
    pub fn new(kind: RemoteKind, id: String, resolvable: bool) -> Self {
        Self {
            kind,
            id,
            url: None,
            resolvable,
            outdated: None,
            path: None,
            old_path: None,
            line: None,
            old_line: None,
            original_line: None,
            start_line: None,
            original_start_line: None,
            side: None,
            commit: None,
            review_state: None,
        }
    }
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

impl ThreadType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bug => "bug",
            Self::Question => "question",
            Self::Requirement => "requirement",
            Self::Nit => "nit",
            Self::Comment => "comment",
        }
    }

    /// The type named in a finding heading; anything unknown is a plain comment.
    pub fn parse(text: &str) -> Self {
        [Self::Bug, Self::Question, Self::Requirement, Self::Nit]
            .into_iter()
            .find(|kind| kind.as_str() == text)
            .unwrap_or(Self::Comment)
    }

    /// Whether an open thread of this type stops an approval.
    pub fn blocks_approval(self) -> bool {
        matches!(self, Self::Bug | Self::Requirement | Self::Question)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    High,
    Medium,
    Low,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [Self::High, Self::Medium, Self::Low]
            .into_iter()
            .find(|severity| severity.as_str() == text)
    }
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
    #[serde(flatten)]
    pub delivery: Delivery,
    pub remote: Option<RemoteComment>,
    pub draft_id: Option<String>,
    /// Kept in the store only (`reply --local`), such as the user's answer to a held
    /// question; `publish` never posts it.
    #[serde(default)]
    pub local: bool,
    pub deleted_remote: bool,
    pub deleted_seen_at: Option<String>,
}

impl Comment {
    const IMPORTED_PREFIX: &'static str = "remote-";

    /// The id of a comment imported from the host comment `remote_id`.
    pub fn imported_id(remote_id: &str) -> String {
        format!("{}{remote_id}", Self::IMPORTED_PREFIX)
    }

    /// Whether the comment was imported from the host rather than written here.
    pub fn is_imported(&self) -> bool {
        self.id.starts_with(Self::IMPORTED_PREFIX)
    }

    /// A comment with no draft, attempt or deletion state yet.
    pub fn new(
        id: String,
        author: Author,
        body: String,
        created_at: String,
        post: PostState,
        remote: Option<RemoteComment>,
    ) -> Self {
        Self {
            id,
            author,
            body,
            created_at,
            delivery: Delivery::new(post),
            remote,
            draft_id: None,
            local: false,
            deleted_remote: false,
            deleted_seen_at: None,
        }
    }
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
    fn delivery_state_stays_flat_in_stored_records() {
        let record: RecordPost = serde_json::from_str(
            r#"{"post":"posting","attempt":"at-1","head":"abc","remote_id":null,"url":null}"#,
        )
        .unwrap();
        assert!(record.delivery.is_posting());
        assert_eq!(record.delivery.sent, None);
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["post"], "posting");
        assert_eq!(value["attempt"], "at-1");
        assert_eq!(value["sent"], serde_json::Value::Null);
        assert!(value.get("delivery").is_none());
    }

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

    #[test]
    fn type_and_severity_names_match_their_serialized_form() {
        for kind in [
            ThreadType::Bug,
            ThreadType::Question,
            ThreadType::Requirement,
            ThreadType::Nit,
            ThreadType::Comment,
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), kind.as_str());
            assert!(ThreadType::parse(kind.as_str()) == kind);
        }
        for severity in [Severity::High, Severity::Medium, Severity::Low] {
            assert_eq!(serde_json::to_value(severity).unwrap(), severity.as_str());
            assert!(Severity::parse(severity.as_str()) == Some(severity));
        }
        assert!(ThreadType::parse("other") == ThreadType::Comment);
    }

    #[test]
    fn resolution_round_trips_the_stored_text() {
        for text in [
            "fixed 0123abc",
            "not fixed: still broken",
            "rejected: by design",
            "rejected: ",
            "stale",
            "answered",
            "something older",
        ] {
            let resolution: Resolution = serde_json::from_value(serde_json::json!(text)).unwrap();
            assert_eq!(serde_json::to_value(&resolution).unwrap(), text);
            assert_eq!(resolution.to_string(), text);
        }
        assert_eq!(
            Resolution::parse("fixed abc"),
            Resolution::Fixed { sha: "abc".into() }
        );
        assert_eq!(
            Resolution::parse("not fixed: x"),
            Resolution::NotFixed { reason: "x".into() }
        );
        assert_eq!(
            Resolution::parse("rejected: y"),
            Resolution::Rejected { reason: "y".into() }
        );
    }
}
