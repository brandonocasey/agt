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
    pub runs: Vec<Run>,
    pub threads: Vec<Thread>,
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
    pub created_at: String,
    pub updated_at: String,
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
    pub author: Author,
    pub body: String,
    pub created_at: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Author {
    Human {
        login: String,
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
