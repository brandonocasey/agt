//! Review threads that name an issue: `resolve --issue` leaves a finding for a follow-up issue,
//! `comment --issue` and `qa finding --issue` record a related one. The issue must be on the
//! host, so the PR/MR never names a private record.

use serde_json::{Value, json};

use crate::support::Env;
use crate::support::forge::FakeService;
use crate::support::forge::github::Github;
use crate::support::issue::{Github as GithubIssues, IssueFake, Spec};
use crate::support::scenario::{Setup, thread, write_text};

/// A reviewed branch whose repository also tracks the issues of `o/r`, with issue 1 on the host.
fn setup_with_issue() -> Setup {
    let setup = Setup::reviewed(Github::new().origin());
    let env = &setup.env;
    let tracker: &dyn IssueFake = &GithubIssues;
    env.write_config(&tracker.config(false));
    tracker.serve(env, &[Spec::new(1, "Follow up the login race")]);
    env.issue(&["sync"]);
    setup
}

fn qa(env: &Env, args: &[&str]) -> Value {
    let output = env.bin(&env.repo).arg("qa").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "agt qa {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn resolve_issue_moves_a_finding_to_a_follow_up_issue() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let f1 = setup.finding(1);
    let target = env.run(&["status"])["target"].as_str().unwrap().to_owned();
    let resolved = env.run(&["resolve", &f1, "--issue", "o/r#1"])["thread"].clone();
    assert_eq!(resolved["resolved"], true);
    assert_eq!(resolved["resolution"], "moved to o/r#1");
    let reply = resolved["comments"].as_array().unwrap().last().unwrap();
    assert_eq!(reply["body"], "📋 Moved to o/r#1.");
    // The issue points back at the review.
    let tracker = GithubIssues.tracker();
    let issue = env.issue_doc(tracker, "1");
    assert_eq!(issue["links"]["reviews"], json!([target]), "{issue}");

    // The finding was never posted, and the summary keeps it as work that remains.
    let svc = Github::new();
    svc.link(&setup);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert!(
        !dry["skipped"].to_string().contains(&f1),
        "moved before it was posted, so silent: {dry}"
    );
    let summary = write_text(&dry, "body", "review-record");
    assert!(
        summary.contains("**📋 Follow-up issues**\n\n1. o/r#1 · In-diff text of f1."),
        "{summary}"
    );
    assert!(
        !summary.contains("closed finding"),
        "moved is not closed: {summary}"
    );
}

#[test]
fn resolve_issue_refuses_an_issue_that_is_not_on_the_host() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let f1 = setup.finding(1);
    let made = env.issue(&[
        "create",
        "--tracker",
        GithubIssues.tracker(),
        "--title",
        "Only here",
    ]);
    let key = made["key"].as_str().unwrap().to_owned();
    let error = env.fail(&["resolve", &f1, "--issue", &key], None);
    assert!(
        error.contains("is not on the host yet; run `agt issue push` first"),
        "{error}"
    );
    assert!(error.contains(&key), "{error}");
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f1)["resolved"], false, "{list}");
    // One outcome per resolve: a usage error, not a silently dropped flag.
    let output = env.exec(
        &["resolve", &f1, "--fixed", "HEAD", "--issue", "o/r#1"],
        None,
        &[],
    );
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("cannot be used with"), "{error}");
    let error = env.fail(&["resolve", &f1, "--issue", "o/r#99"], None);
    assert!(error.contains("no issue o/r#99"), "{error}");
}

#[test]
fn comment_and_qa_finding_issue_name_a_related_issue() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let made = env.run(&[
        "comment",
        "--body",
        "Same race as before.",
        "--issue",
        "o/r#1",
    ]);
    let comment = &made["thread"];
    assert_eq!(comment["issue"], "o/r#1", "{made}");
    assert_eq!(comment["resolved"], false);

    let run = qa(env, &["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let check = qa(
        env,
        &[
            "check",
            "--run",
            &run,
            "--name",
            "login",
            "--outcome",
            "fail",
        ],
    )["check"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let found = qa(
        env,
        &[
            "finding",
            "--check",
            &check,
            "--type",
            "bug",
            "--severity",
            "high",
            "--body",
            "Login hangs.",
            "--issue",
            "o/r#1",
        ],
    );
    assert_eq!(found["thread"]["issue"], "o/r#1", "{found}");

    // A QA run counts for the PR only when it ends after the PR is linked.
    Github::new().link(&setup);
    qa(env, &["run", "end", "--run", &run]);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    let posted = write_text(&dry, "body", "Same race as before.");
    assert!(posted.contains("\n\nRelated: o/r#1"), "{posted}");
    let summary = write_text(&dry, "body", "qa-record");
    assert!(
        summary.contains("3. 🧪 **Login hangs.**\n   High · QA: login · whole PR · Related: o/r#1"),
        "{summary}"
    );
}
