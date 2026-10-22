//! `agt qa`: checks, their staleness, QA findings, the QA approval and the QA part of the
//! summary that `publish` posts and `sync` reads back.

use std::process::Output;

use serde_json::Value;

use crate::support::forge::github::{self, Github};
use crate::support::forge::{Call, Comment, FakeService, Remote};
use crate::support::scenario::{Review, Setup, write_text};
use crate::support::{Env, numbered};

fn exec(env: &Env, args: &[&str]) -> Output {
    env.bin(&env.repo).arg("qa").args(args).output().unwrap()
}

fn qa(env: &Env, args: &[&str]) -> Value {
    let output = exec(env, args);
    assert!(
        output.status.success(),
        "agt qa {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn qa_fail(env: &Env, args: &[&str]) -> String {
    let output = exec(env, args);
    assert_eq!(output.status.code(), Some(1), "agt qa {args:?} should fail");
    String::from_utf8(output.stderr).unwrap()
}

fn start(env: &Env, extra: &[&str]) -> String {
    let mut args = vec!["run", "start"];
    args.extend(extra);
    qa(env, &args)["run"].as_str().unwrap().to_owned()
}

fn check(env: &Env, run: &str, name: &str, outcome: &str, extra: &[&str]) -> String {
    let mut args = vec!["check", "--run", run, "--name", name, "--outcome", outcome];
    args.extend(extra);
    qa(env, &args)["check"]["id"].as_str().unwrap().to_owned()
}

fn changed_branch() -> Env {
    let env = Env::with_branch().offline();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    env
}

#[test]
fn the_latest_check_of_a_name_counts_until_the_content_changes() {
    let env = changed_branch();
    let run = start(&env, &["--env", "browser=firefox", "--env", "os=linux"]);
    let login = check(
        &env,
        &run,
        "login",
        "pass",
        &[
            "--env",
            "browser=chrome",
            "--evidence",
            "https://example.com/1.png",
            "--evidence",
            "shot.png",
        ],
    );
    assert_eq!(login, format!("{run}.k1"));
    check(&env, &run, "logout", "fail", &[]);

    let status = qa(&env, &["status"]);
    assert_eq!(status["qa"]["pass"], 1);
    assert_eq!(status["qa"]["fail"], 1);
    assert_eq!(status["qa"]["approvable"], false);
    let checks = status["qa"]["checks"].as_array().unwrap();
    assert_eq!(
        checks[0]["env"],
        serde_json::json!(["os=linux", "browser=chrome"])
    );
    assert!(
        status["qa"]["blockers"][0]
            .as_str()
            .unwrap()
            .contains("fail: logout"),
        "{status}"
    );

    let retest = check(&env, &run, "logout", "pass", &[]);
    let status = qa(&env, &["status"]);
    assert_eq!(status["qa"]["checks"][1]["id"], retest.as_str());
    assert_eq!(status["qa"]["approvable"], true, "{status}");

    // New content makes both checks stale; a dirty work tree does too, in both statuses.
    env.write("a.txt", &numbered(10, &[(3, "four")]));
    let status = qa(&env, &["status"]);
    assert_eq!(status["qa"]["stale"], 2);
    assert_eq!(status["qa"]["approvable"], false);
    assert_eq!(env.run(&["status"])["qa"], status["qa"]);
    // `--head` evaluates the commit, never the work tree.
    let at_head = qa(&env, &["status", "--head", "HEAD"]);
    assert_eq!(at_head["qa"]["stale"], 0);
    assert_eq!(env.run(&["status", "--head", "HEAD"])["qa"], at_head["qa"]);
    assert!(qa_fail(&env, &["approve"]).contains("uncommitted"));
    env.commit_all("more");
    assert!(qa_fail(&env, &["approve"]).contains("is stale: login"));
}

#[test]
fn an_open_finding_blocks_approval_until_resolved() {
    let env = changed_branch();
    let run = start(&env, &["--model", "m1", "--harness", "claude-code"]);
    let k1 = check(&env, &run, "checkout", "pass", &[]);
    assert!(
        qa_fail(
            &env,
            &["finding", "--check", &k1, "--type", "bug", "--body", "x"]
        )
        .contains("severity")
    );
    let thread = qa(
        &env,
        &[
            "finding",
            "--check",
            &k1,
            "--type",
            "bug",
            "--severity",
            "high",
            "--body",
            "The total ignores the coupon.",
        ],
    )["thread"]
        .clone();
    let id = thread["id"].as_str().unwrap().to_owned();
    assert_eq!(thread["check"], k1.as_str());
    // The finder defaults to the run's tester, tied to the QA run.
    assert_eq!(thread["comments"][0]["author"]["run"], run.as_str());

    assert!(qa_fail(&env, &["approve"]).contains(&format!("{id} is open")));
    // The review sees the QA thread as open too, but never counts a check as a review.
    let review = env.run(&["status"]);
    assert_eq!(review["open_threads"][0]["id"], id.as_str());
    assert_eq!(review["reviewed"], false);
    assert_eq!(review["qa"]["open_threads"][0], id.as_str());

    env.run(&["resolve", &id, "--rejected", "works as designed"]);
    let approved = qa(&env, &["approve"]);
    assert_eq!(approved["approved"]["run"], run.as_str());
    assert_eq!(qa(&env, &["status"])["qa"]["approved"], true);
    assert_eq!(qa(&env, &["list"])["threads"][0]["open"], false);
}

#[test]
fn show_finds_qa_runs_and_checks_and_ended_runs_take_no_checks() {
    let env = changed_branch();
    let run = start(&env, &["--env", "device=pixel"]);
    assert!(qa_fail(&env, &["run", "start", "--env", "novalue"]).contains("key=value"));
    let k1 = check(&env, &run, "share", "skipped", &["--steps", "tap share"]);
    let shown = qa(&env, &["show", &k1]);
    assert_eq!(shown["kind"], "check");
    assert_eq!(shown["check"]["steps"], "tap share");
    assert_eq!(shown["env"], serde_json::json!(["device=pixel"]));
    let ended = qa(&env, &["run", "end", "--run", &run]);
    assert!(ended["run"]["ended_at"].is_string());
    assert_eq!(ended["status"]["qa"]["skipped"], 1);
    assert_eq!(qa(&env, &["show", &run])["kind"], "qa-run");
    assert!(
        qa_fail(
            &env,
            &["check", "--run", &run, "--name", "x", "--outcome", "pass"]
        )
        .contains("already ended")
    );
}

/// A review run with no findings, linked to PR 42, then an ended QA run with one failing check
/// and, with `finding`, a bug it found. `before_end` runs just before the QA run ends.
fn published_setup(svc: &Github, finding: bool, before_end: impl FnOnce(&Env)) -> (Setup, String) {
    let setup = Setup::branch(
        svc.origin(),
        &Review {
            reviewers: r#"[{"model":"m1","effort":"xhigh","harness":"codex"}]"#,
            findings: "[]",
        },
        false,
        false,
    );
    svc.link(&setup);
    let env = &setup.env;
    let run = start(env, &[]);
    let k1 = check(
        env,
        &run,
        "pay with a card",
        "fail",
        &[
            "--evidence",
            "https://example.com/log",
            "--evidence",
            "local.log",
        ],
    );
    if finding {
        qa(
            env,
            &[
                "finding",
                "--check",
                &k1,
                "--type",
                "bug",
                "--severity",
                "high",
                "--body",
                "Payment hangs. The spinner never stops.",
            ],
        );
    }
    before_end(env);
    qa(env, &["run", "end", "--run", &run]);
    (setup, run)
}

#[test]
fn publish_adds_a_qa_section_to_the_summary() {
    let svc = Github::new();
    let (setup, run) = published_setup(&svc, true, |_| {});
    let dry = qa(&setup.env, &["publish", "--pr", "42", "--dry-run"]);
    assert_eq!(
        qa(
            &setup.env,
            &["publish", "--pr", "42", "--dry-run", "--not-own"]
        )["writes"],
        dry["writes"]
    );
    // A QA finding has no place in the diff; GitHub cannot resolve a general comment, so the
    // summary lists it with its whole text.
    let skipped = dry["skipped"].as_array().unwrap();
    assert!(
        skipped.iter().any(|skip| skip["reason"]
            == "listed in the summary: a general comment here cannot be resolved"),
        "{dry}"
    );
    assert!(
        !dry["writes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|write| write["kind"] == "thread"),
        "{dry}"
    );
    let summary = write_text(&dry, "body", "qa-record");
    assert!(
        summary.starts_with("**⚠️ 1 to fix** · agent review and QA at `"),
        "{summary}"
    );
    assert!(
        summary.contains("1. **🐛 bug (high)** · QA: pay with a card · Payment hangs."),
        "{summary}"
    );
    assert!(
        summary.contains("\n   > Payment hangs. The spinner never stops."),
        "{summary}"
    );
    assert!(summary.contains("⚠ QA 0 of 1 pass"), "{summary}");
    assert!(
        summary.contains("| QA check | Result | Environment | Evidence |"),
        "{summary}"
    );
    assert!(
        summary.contains("| pay with a card | ❌ fail |  | [1](https://example.com/log) 1 local |"),
        "{summary}"
    );
    assert!(!summary.contains("local.log"), "{summary}");
    assert!(
        summary.contains(&format!(
            "<!-- qa-record v1 id={run} head={} -->",
            setup.head
        )),
        "{summary}"
    );
}

#[test]
fn sync_claims_the_qa_record_of_its_own_summary() {
    let svc = Github::new();
    let (setup, run) = published_setup(&svc, false, |_| {});
    let env = &setup.env;
    svc.lose(env, Call::PostNote);
    exec(env, &["publish", "--pr", "42"]);
    let record = &qa(env, &["show", &run])["run"]["record"];
    assert_eq!(record["post"], "posting", "{record}");
    let sent = record["sent"].as_str().unwrap().to_owned();

    let mut remote = Remote::new();
    remote.general("9001", Comment::new(9001, "me", &sent));
    svc.sync(&setup, &remote);
    env.run(&["sync", "--pr", "42"]);
    let record = &qa(env, &["show", &run])["run"]["record"];
    assert_eq!(record["post"], "posted", "{record}");
    assert_eq!(record["remote_id"], "9001");
    let list = env.run(&["list"]);
    assert!(
        !list["threads"]
            .as_array()
            .unwrap()
            .iter()
            .any(|thread| thread["comments"][0]["body"]
                .as_str()
                .is_some_and(|body| body.contains("Agent QA"))),
        "the summary is not imported as a thread: {list}"
    );
}

/// The automatic publish of an ended QA run posts and counts the findings of its checks, also
/// those of a human tester, which carry no agent run.
#[test]
fn auto_publish_of_a_qa_run_carries_its_human_findings() {
    let svc = Github::new();
    let (setup, run) = published_setup(&svc, true, |env| {
        env.write_config("[sync]\nauto_push = true\n");
        github::pulls(env, "me/repo", r#"[{"number":42,"state":"open"}]"#);
        svc.accept(env, Call::PostNote, 802);
    });
    let env = &setup.env;
    let tester = &qa(env, &["show", &run])["run"]["tester"];
    assert_eq!(tester["kind"], "human", "{tester}");
    let inputs: Vec<String> = env
        .forge_inputs()
        .into_iter()
        .map(|(_, input)| input)
        .collect();
    assert!(
        inputs.iter().any(|input| input.contains("Payment hangs.")),
        "{inputs:?}"
    );
    let record = inputs
        .iter()
        .find(|input| input.contains("qa-record v1"))
        .unwrap_or_else(|| panic!("no QA record posted: {inputs:?}"));
    assert!(record.contains("⚠️ 1 to fix"), "{record}");
    assert!(record.contains("Payment hangs."), "{record}");
    assert!(!record.contains("✅ Nothing to fix"), "{record}");
}

/// A target whose branch is gone has no head; both statuses still show its QA state.
#[test]
fn a_headless_target_still_reports_qa() {
    let env = changed_branch();
    env.git(&["switch", "-q", "-c", "gone"]);
    let run = start(&env, &[]);
    check(&env, &run, "login", "pass", &[]);
    let key = qa(&env, &["status"])["target"].as_str().unwrap().to_owned();
    env.git(&["switch", "-q", "feat"]);
    env.git(&["branch", "-q", "-D", "gone"]);
    let review = env.run(&["--target", &key, "status"]);
    assert_eq!(review["head"], Value::Null, "{review}");
    assert_eq!(review["qa"], qa(&env, &["--target", &key, "status"])["qa"]);
    assert_eq!(review["qa"]["stale"], 1, "{review}");
}
