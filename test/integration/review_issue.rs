//! Review threads that name an issue: `resolve --issue` leaves a finding for a follow-up issue,
//! `comment --issue` and `qa finding --issue` record a related one. The issue must be on the
//! host, so the PR/MR never names a private record.

use serde_json::{Value, json};

use crate::support::Env;
use crate::support::forge::github::Github;
use crate::support::forge::github::own_review;
use crate::support::forge::{Call, Comment, FakeService, Remote};
use crate::support::issue::{Github as GithubIssues, IssueFake, Spec};
use crate::support::scenario::{Review, Setup, sent, start_review, thread, write_text};

/// A reviewed branch whose repository also tracks the issues of `o/r`, with issue 1 on the host.
fn setup_with_issue() -> Setup {
    let setup = Setup::reviewed(Github::new().origin());
    let env = &setup.env;
    let tracker: &dyn IssueFake = &GithubIssues;
    // The review's origin is a tracker too; its issues are not the ones under test.
    env.write_config(&format!(
        "[sync]\npush = \"manual\"\n{}\n[[tracker]]\nhost = \"github.com\"\nrepo = \"me/repo\"\nissues = false\n",
        tracker.config(false)
    ));
    tracker.serve(
        env,
        &[
            Spec::new(1, "Follow up the login race"),
            Spec::new(2, "Slow checkout"),
        ],
    );
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
            "o/r#2",
        ],
    );
    assert_eq!(found["thread"]["issue"], "o/r#2", "{found}");

    // The issue names the target as it was stored when the thread was written.
    let target = env.run(&["status"])["target"].as_str().unwrap().to_owned();
    // A QA run counts for the PR only when it ends after the PR is linked.
    Github::new().link(&setup);
    qa(env, &["run", "end", "--run", &run]);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    let posted = write_text(&dry, "body", "Same race as before.");
    assert!(posted.contains("\n\nRelated: o/r#1"), "{posted}");
    let summary = write_text(&dry, "body", "qa-record");
    assert!(
        summary.contains("3. 🧪 **Login hangs.** · High · QA: login · whole PR · Related: o/r#2\n"),
        "{summary}"
    );
    // Each thread points its issue back at the review.
    for number in ["1", "2"] {
        let issue = env.issue_doc(GithubIssues.tracker(), number);
        assert_eq!(issue["links"]["reviews"], json!([target]), "{issue}");
    }
}

/// A review finding in the `--findings` JSON can name an issue as `comment --issue` does.
#[test]
fn a_finding_in_the_pass_input_names_a_related_issue() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let target = env.run(&["status"])["target"].as_str().unwrap().to_owned();
    let run = start_review(
        env,
        &Review {
            reviewers: r#"[{"model":"m1","effort":"xhigh","harness":"codex"}]"#,
            findings: r#"[{"type":"bug","severity":"high","path":"a.txt","line":3,
              "scenario":"Races the login again.","issue":"o/r#1"}]"#,
        },
        false,
    );
    let list = env.run(&["list"]);
    let linked = thread(&list, &format!("{run}.c1p0.r1.f1"));
    assert_eq!(linked["issue"], "o/r#1", "{list}");
    assert_eq!(linked["resolved"], false);
    let issue = env.issue_doc(GithubIssues.tracker(), "1");
    assert_eq!(issue["links"]["reviews"], json!([target]), "{issue}");

    // The summary covers a run once it has ended.
    Github::new().link(&setup);
    env.run(&["run", "end", "--run", &run]);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    let posted = dry["writes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|write| write["input"]["comments"].as_array().into_iter().flatten())
        .map(|comment| comment["body"].as_str().unwrap())
        .find(|body| body.contains("Races the login again."))
        .unwrap();
    assert!(posted.contains("\n\nRelated: o/r#1"), "{posted}");
    let summary = write_text(&dry, "body", "review-record");
    assert!(
        summary.contains("**Races the login again.** · High · `a.txt:3` · Related: o/r#1\n"),
        "{summary}"
    );
}

/// An issue the host does not have, or on a finding that is never posted, stops `pass end`
/// before anything is stored.
#[test]
fn a_finding_issue_that_cannot_be_posted_is_refused() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let run = env.run_stdin(
        &["pass", "start", "--kind", "full", "--reviewers", "-"],
        r#"[{"model":"m1","effort":"xhigh","harness":"codex"}]"#,
    )["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let end = |findings: &str| {
        env.fail(
            &[
                "pass",
                "end",
                "--run",
                &run,
                "--pass",
                "c1p0",
                "--findings",
                "-",
                "--reviewer",
                "r1",
            ],
            Some(findings),
        )
    };
    let error = end(r#"[{"type":"nit","scenario":"x","issue":"o/r#99"}]"#);
    assert!(error.contains("no issue o/r#99"), "{error}");
    let error =
        end(r#"[{"type":"nit","scenario":"x","issue":"o/r#1","killed":true,"reason":"not real"}]"#);
    assert!(error.contains("takes no issue"), "{error}");
    let list = env.run(&["list"]);
    assert!(
        !list.to_string().contains(&format!("{run}.c1p0.r1.f")),
        "nothing was stored: {list}"
    );
}

/// The issue is linked before the thread is stored: when the link fails (the issue document
/// carries a field this build does not know), nothing is stored, so a retry cannot duplicate.
#[test]
fn a_comment_whose_issue_cannot_be_linked_stores_no_thread() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let path = env
        .issue_state()
        .join(GithubIssues.tracker())
        .join("1.json");
    let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    doc["field_from_a_newer_build"] = json!(true);
    std::fs::write(&path, doc.to_string()).unwrap();
    let before = env.run(&["list"]);

    let args = [
        "comment",
        "--body",
        "Same race as before.",
        "--issue",
        "o/r#1",
    ];
    let error = env.fail(&args, None);
    assert!(error.contains("field_from_a_newer_build"), "{error}");
    assert_eq!(
        env.run(&["list"]),
        before,
        "the failed comment stored a thread"
    );
}

/// A killed finding is never posted, so `finding kill` refuses one that names an issue, as
/// `pass end` does, and leaves it in place.
#[test]
fn finding_kill_refuses_a_finding_that_names_an_issue() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let run = start_review(
        env,
        &Review {
            reviewers: r#"[{"model":"m1","effort":"xhigh","harness":"codex"}]"#,
            findings: r#"[{"type":"bug","severity":"high","path":"a.txt","line":3,
              "scenario":"Races the login again.","issue":"o/r#1"}]"#,
        },
        false,
    );
    let id = format!("{run}.c1p0.r1.f1");
    let before = env.run(&["list"]);
    let error = env.fail(&["finding", "kill", &id, "--reason", "not real"], None);
    assert!(
        error.contains(&format!(
            "finding {id}: a killed finding is never posted, so it takes no issue"
        )),
        "{error}"
    );
    assert_eq!(
        env.run(&["list"]),
        before,
        "the refused kill changed the target"
    );
}

/// A finding that was posted and then answered is listed among the closed findings, with the
/// issue it names.
#[test]
fn a_closed_finding_names_its_related_issue() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let run = start_review(
        env,
        &Review {
            reviewers: r#"[{"model":"m1","effort":"xhigh","harness":"codex"}]"#,
            findings: r#"[{"type":"question","path":"a.txt","line":3,
              "scenario":"Why so? More.","issue":"o/r#1"}]"#,
        },
        false,
    );
    let id = format!("{run}.c1p0.r1.f1");
    let svc = Github::new();
    svc.link(&setup);
    env.run(&["run", "end", "--run", &run]);
    // The host loses the response to the review; a sync finds the comments by their markers.
    svc.lose(env, Call::PostInline);
    env.fail(&["publish", "--pr", "42"], None);
    let mut remote = Remote::new();
    for (remote_id, number, finding) in [
        ("PRRT_f1", 501, setup.finding(1)),
        ("PRRT_q", 502, id.clone()),
    ] {
        remote.thread(
            remote_id,
            false,
            3,
            vec![Comment::new(number, "me", &sent(env, &finding))],
        );
    }
    remote.reviews.push(own_review(&setup.head));
    svc.sync(&setup, &remote);
    env.pull(&["sync", "--pr", "42"]);
    svc.restore(env, Call::PostInline);
    env.run(&["resolve", &id, "--answered"]);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    let summary = write_text(&dry, "body", "closed finding");
    let row = summary
        .lines()
        .find(|line| line.contains("Why so?"))
        .unwrap_or_else(|| panic!("no closed row: {summary}"));
    assert!(row.ends_with("· Related: o/r#1 |"), "{row}");
}
