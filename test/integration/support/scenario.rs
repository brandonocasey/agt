//! A reviewed branch and the helpers every publish and sync scenario shares: how a scenario
//! reads command output, names calls, and asserts what was posted. Nothing here knows a
//! service; that is [`super::forge`].

use serde_json::Value;

use super::{Env, numbered};

/// The reviewer and findings a [`Setup`] is reviewed with.
pub struct Review {
    pub reviewers: &'static str,
    pub findings: &'static str,
}

/// An in-diff finding (f1), an out-of-diff one (f2, line 9) and a killed one (f3).
pub const PUBLISH: Review = Review {
    reviewers: r#"[{"model":"m1","effort":"xhigh","harness":"claude-code","harness_version":"2.1.296","session":"a0b5ad828155531d2"}]"#,
    findings: r#"[
  {"type":"bug","severity":"high","path":"a.txt","line":3,"scenario":"In-diff text of f1."},
  {"type":"nit","scenario":"General text of f2."},
  {"type":"bug","severity":"low","path":"a.txt","line":3,"scenario":"Killed text of f3.","killed":true,"reason":"not reproducible"}
]"#,
};

/// Two in-diff findings on one line.
pub const SYNC: Review = Review {
    reviewers: r#"[{"model":"m1","effort":"xhigh","harness":"codex"}]"#,
    findings: r#"[
  {"type":"bug","severity":"high","path":"a.txt","line":3,"scenario":"Local text of f1."},
  {"type":"bug","severity":"low","path":"a.txt","line":3,"scenario":"Local text of f2."}
]"#,
};

/// Text that `publish` must never send: killed findings and other people's comments.
const NEVER_POSTED: [&str; 4] = [
    "Killed text of f3",
    "Alice thread text",
    "Alice general text",
    "Someone else's text",
];

pub struct Setup {
    pub env: Env,
    pub run: String,
    pub head: String,
    pub base: String,
}

impl Setup {
    pub fn finding(&self, number: u32) -> String {
        format!("{}.c1p0.r1.f{number}", self.run)
    }

    /// A `feat` branch changing line 3 of `a.txt`, reviewed by one full pass of `review`. The
    /// run is still open. `lock` ends the pass locked, as a blind review does; `old` first
    /// commits a `b.txt`, so the base is one commit behind.
    pub fn branch(origin: &str, review: &Review, lock: bool, old: bool) -> Self {
        let env = Env::with_branch();
        env.git(&["remote", "add", "origin", origin]);
        if old {
            env.write("b.txt", "b\n");
            env.commit_all("old");
        }
        env.write("a.txt", &numbered(10, &[(3, "three")]));
        let head = env.commit_all("change");
        let base = env.git(&["rev-parse", "main"]);
        let run = start_review(&env, review, lock);
        Self {
            env,
            run,
            head,
            base,
        }
    }

    /// The publish scenarios' branch, with its run open.
    pub fn reviewed(origin: &str) -> Self {
        Self::branch(origin, &PUBLISH, false, false)
    }

    /// Like [`Setup::reviewed`] with the pass locked.
    pub fn locked(origin: &str) -> Self {
        Self::branch(origin, &PUBLISH, true, false)
    }

    /// The sync scenarios' branch, with its run ended.
    pub fn synced_branch(origin: &str) -> Self {
        let setup = Self::branch(origin, &SYNC, false, true);
        setup.env.run(&["run", "end", "--run", &setup.run]);
        setup
    }
}

/// Starts a run, takes one full pass of `review` and returns the run id. The pass ends locked
/// when `lock` is set.
pub fn start_review(env: &Env, review: &Review, lock: bool) -> String {
    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    add_pass(env, &run, review);
    let mut end = vec!["pass", "end", "--run", &run, "--pass", "c1p0"];
    if lock {
        end.push("--lock");
    }
    env.run(&end);
    run
}

/// Starts the full pass `c1p0` of `run` with `review`'s reviewer and adds its findings.
pub fn add_pass(env: &Env, run: &str, review: &Review) {
    env.run_stdin(
        &[
            "pass",
            "start",
            "--run",
            run,
            "--kind",
            "full",
            "--reviewers",
            "-",
        ],
        review.reviewers,
    );
    env.run_stdin(
        &[
            "finding",
            "add",
            "--run",
            run,
            "--pass",
            "c1p0",
            "--reviewer",
            "r1",
        ],
        review.findings,
    );
}

/// The text a comment carries so a later sync recognizes it as this store's own.
pub fn marked(text: &str, local: &str) -> String {
    format!("{text}\n<!-- agent-review id={local} -->")
}

/// The text a failed write sent for the first comment of `thread`, as the host shows it.
pub fn sent(env: &Env, thread_id: &str) -> String {
    let list = env.run(&["list"]);
    thread(&list, thread_id)["comments"][0]["sent"]
        .as_str()
        .unwrap()
        .to_owned()
}

pub fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| (*arg).to_owned()).collect()
}

pub fn strs(list: &[String]) -> Vec<&str> {
    list.iter().map(String::as_str).collect()
}

/// Forge calls whose arguments are exactly `list`.
pub fn count_calls(env: &Env, list: &[String]) -> usize {
    let wanted = list.join(" ");
    env.forge_calls()
        .iter()
        .filter(|call| call.split('\t').nth(1) == Some(wanted.as_str()))
        .count()
}

/// The JSON bodies sent with the calls whose arguments are exactly `call`.
pub fn input_of(inputs: &[(String, String)], call: &[String]) -> Vec<Value> {
    let wanted = call.join(" ");
    inputs
        .iter()
        .filter(|(args, _)| *args == wanted)
        .map(|(_, input)| serde_json::from_str(input).unwrap())
        .collect()
}

pub fn thread<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["id"] == id)
        .unwrap_or_else(|| panic!("no thread {id} in {list}"))
}

pub fn ids(list: &Value) -> Vec<String> {
    list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["id"].as_str().unwrap().to_owned())
        .collect()
}

pub fn open_ids(status: &Value) -> Vec<String> {
    status["open_threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["id"].as_str().unwrap().to_owned())
        .collect()
}

/// The `kind` of each entry of `output[field]`.
pub fn kinds(output: &Value, field: &str) -> Vec<String> {
    output[field]
        .as_array()
        .unwrap()
        .iter()
        .map(|write| write["kind"].as_str().unwrap().to_owned())
        .collect()
}

/// The kinds of the writes of a dry run that carry `item`.
pub fn writes_for(dry: &Value, item: &str) -> Vec<String> {
    dry["writes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|write| {
            write["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|id| id == item)
        })
        .map(|write| write["kind"].as_str().unwrap().to_owned())
        .collect()
}

/// The `item` of each entry of a dry run's `skipped`.
pub fn skipped_items(dry: &Value) -> Vec<String> {
    dry["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|skip| skip["item"].as_str().unwrap().to_owned())
        .collect()
}

/// The text a write of a dry run carries, from `field` of its input.
pub fn write_text<'a>(dry: &'a Value, field: &str, wanted: &str) -> &'a str {
    dry["writes"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|write| {
            write["input"][field]
                .as_str()
                .filter(|text| text.contains(wanted))
        })
        .unwrap_or_else(|| panic!("no write with {wanted} in {dry}"))
}

/// The text of the write of a dry run whose `field` starts with `prefix`.
pub fn write_starting<'a>(dry: &'a Value, field: &str, prefix: &str) -> &'a str {
    dry["writes"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|write| {
            write["input"][field]
                .as_str()
                .filter(|text| text.starts_with(prefix))
        })
        .unwrap_or_else(|| panic!("no write starting {prefix} in {dry}"))
}

pub fn assert_never_posted(env: &Env) {
    for (call, input) in env.forge_inputs() {
        for text in NEVER_POSTED {
            assert!(!input.contains(text), "{call} posted {text}: {input}");
        }
    }
}

/// Local work on top of the review: a general comment of mine and someone else's, a fix reply
/// and resolve of f1, a reopened imported thread, an answer to an imported general comment.
pub fn local_changes(setup: &Setup, reopen: &str, answer: &str) {
    let env = &setup.env;
    let f1 = setup.finding(1);
    env.run(&[
        "comment",
        "--body",
        "General note from me.",
        "--author",
        "me",
    ]);
    env.run(&[
        "comment",
        "--body",
        "Someone else's text.",
        "--author",
        "alice",
    ]);
    env.run(&[
        "reply",
        &f1,
        "--body",
        "Fixed in the same commit.",
        "--model",
        "m2",
        "--effort",
        "high",
        "--harness",
        "claude-code",
        "--harness-version",
        "2.1.300",
        "--run",
        &setup.run,
        "--pass",
        "c1p0",
        "--reviewer",
        "fix",
        "--session",
        "dcec84d5-4c7a-492f-ba78-a9ddd3d1bc7f",
    ]);
    env.run(&[
        "resolve",
        &f1,
        "--triage",
        "fixed",
        "--resolution",
        &format!("fixed {}", setup.head),
    ]);
    env.run(&["resolve", reopen, "--reopen"]);
    env.run(&["reply", answer, "--body", "Thanks, done.", "--author", "me"]);
    env.run(&["run", "end", "--run", &setup.run]);
}

/// The summary text a publish of `setup` sends.
pub fn assert_summary(setup: &Setup, summary: &str) {
    let short = &setup.head[..7];
    let expected = [
        format!("**⚠️ Agent review: 1 open at {short}.** 2 findings: 1 fixed, 1 open."),
        "<details><summary>📝 2 findings: 1 fixed, 1 open</summary>".to_owned(),
        format!("| ✅ Fixed in {short} |"),
        format!(
            "1. 🔍 Reviewer agent a0b5ad82 read the whole branch at {short} and found 2 issues"
        ),
        format!("2. 🛠️ Main session dcec84d5 fixed 1, in {short}."),
        "3. ⚠️ 1 still open.".to_owned(),
        format!(
            "> | 🔍 Reviewer r1, {} | m1 xhigh | Claude Code 2.1.296 | agent a0b5ad828155531d2 |",
            setup.run
        ),
        "> | 🛠️ Main session: fixes, replies | m2 high | Claude Code 2.1.300 | session \
         dcec84d5-4c7a-492f-ba78-a9ddd3d1bc7f |"
            .to_owned(),
        format!("> <sub>Posted by agt {}.", env!("CARGO_PKG_VERSION")),
        format!(
            "<!-- review-record v2 id={} head={} reviews-at-head=1 open=1 -->",
            setup.run, setup.head
        ),
    ];
    for text in expected {
        assert!(summary.contains(&text), "{text} missing from {summary}");
    }
    assert!(summary.starts_with("**⚠️ Agent review: "), "{summary}");
    assert!(
        summary
            .rsplit_once('\n')
            .unwrap()
            .1
            .starts_with("<!-- agent-review id=summary attempt="),
        "{summary}"
    );
    assert!(summary.ends_with(" -->"), "{summary}");
}
