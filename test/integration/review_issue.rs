//! Review threads that name an issue: `resolve --issue` leaves a finding for a follow-up issue,
//! `comment --issue` and `qa finding --issue` record a related one. The issue must be on the
//! host, so the PR/MR never names a private record.

use serde_json::{Value, json};

use crate::support::forge::github::Github;
use crate::support::forge::github::own_review;
use crate::support::forge::{Call, Comment, FakeService, Remote};
use crate::support::issue::{Github as GithubIssues, IssueFake, Spec};
use crate::support::scenario::{Review, Setup, sent, start_review, thread, write_text};
use crate::support::{Env, snapshot};

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
            Spec::new(3, "Flaky coupon test"),
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

const ONE_REVIEWER: &str = r#"[{"model":"m1","effort":"xhigh","harness":"codex"}]"#;

/// Starts a second full-pass run of the reviewed branch with `extra` flags and ends its pass.
fn another_review_run(env: &Env, extra: &[&str]) -> String {
    let mut args = vec!["pass", "start", "--kind", "full", "--reviewers", "-"];
    args.extend(extra);
    let run = env.run_stdin(&args, ONE_REVIEWER)["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let end = ["pass", "end", "--run", &run, "--pass", "c1p0"];
    env.run_stdin(
        &[&end[..], &["--findings", "-", "--reviewer", "r1"]].concat(),
        "[]",
    );
    run
}

fn occurrences(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}

/// A review run that names an issue stores it, links the review back and names it once in the
/// posted summary. A later pass may repeat it but not change it or add one the run lacks.
#[test]
fn a_review_run_names_an_issue_from_pass_start() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let target = env.run(&["status"])["target"].as_str().unwrap().to_owned();

    // The branch's first run has no issue, so no later pass can add one.
    let error = env.fail(
        &[
            "pass", "start", "--kind", "full", "--run", &setup.run, "--issue", "o/r#1",
        ],
        None,
    );
    assert!(
        error.contains("only the pass that starts the run can set --issue"),
        "{error}"
    );
    let untouched = env.issue_doc(GithubIssues.tracker(), "1");
    assert!(
        untouched["links"]["reviews"]
            .as_array()
            .is_none_or(|reviews| reviews.is_empty()),
        "a refused pass linked the review: {untouched}"
    );

    let run = another_review_run(env, &["--issue", "1"]);
    let shown = env.run(&["show", &run]);
    assert_eq!(shown["run"]["issue"], "o/r#1", "{shown}");
    let issue = env.issue_doc(GithubIssues.tracker(), "1");
    assert_eq!(issue["links"]["reviews"], json!([target]), "{issue}");
    let different = env.fail(
        &[
            "pass", "start", "--kind", "full", "--run", &run, "--issue", "o/r#2",
        ],
        None,
    );
    assert!(different.contains("has issue o/r#1"), "{different}");

    Github::new().link(&setup);
    env.run(&["run", "end", "--run", &run]);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    let summary = write_text(&dry, "body", "review-record");
    assert_eq!(occurrences(summary, "Related: o/r#1"), 1, "{summary}");
}

/// A QA run and a QA check that name an issue store it and link the review back. The summary
/// names each run's issue once, even when a review run and another QA run name the same one, and
/// the check's issue in its table row only.
#[test]
fn a_qa_run_and_a_qa_check_name_an_issue() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let target = env.run(&["status"])["target"].as_str().unwrap().to_owned();
    let review_run = another_review_run(env, &["--issue", "o/r#1"]);
    let started = qa(env, &["run", "start", "--issue", "o/r#3"]);
    assert_eq!(started["issue"], "o/r#3", "{started}");
    let run = started["run"].as_str().unwrap().to_owned();
    let same = qa(env, &["run", "start", "--issue", "1"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let checked = qa(
        env,
        &[
            "check",
            "--run",
            &run,
            "--name",
            "login",
            "--outcome",
            "fail",
            "--issue",
            "2",
        ],
    );
    assert_eq!(checked["check"]["issue"], "o/r#2", "{checked}");
    assert_eq!(qa(env, &["list"])["runs"][0]["issue"], "o/r#3");
    assert_eq!(qa(env, &["list"])["checks"][0]["issue"], "o/r#2");
    for number in ["1", "2", "3"] {
        let expected = json!([target]);
        let issue = env.issue_doc(GithubIssues.tracker(), number);
        assert_eq!(issue["links"]["reviews"], expected, "{issue}");
    }

    Github::new().link(&setup);
    env.run(&["run", "end", "--run", &review_run]);
    qa(env, &["run", "end", "--run", &run]);
    qa(env, &["run", "end", "--run", &same]);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    let summary = write_text(&dry, "body", "qa-record");
    for issue in ["o/r#1", "o/r#2", "o/r#3"] {
        assert_eq!(
            occurrences(summary, &format!("Related: {issue}")),
            1,
            "{summary}"
        );
    }
    let row = summary
        .lines()
        .find(|line| line.starts_with("| login"))
        .unwrap();
    assert!(row.contains("fail · Related: o/r#2 |"), "{row}");
}

/// An issue the host does not have is refused by each flag before anything is written.
#[test]
fn a_bad_run_or_check_issue_is_refused_before_anything_is_written() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let run = qa(env, &["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let before = (
        env.run(&["list"]),
        env.run(&["status", "--full"]),
        qa(env, &["list"]),
    );
    let pass = [
        "pass",
        "start",
        "--kind",
        "full",
        "--reviewers",
        "-",
        "--issue",
        "o/r#99",
    ];
    for dry_run in [false, true] {
        let args = [&pass[..], if dry_run { &["--dry-run"] } else { &[] }].concat();
        let error = env.fail(&args, Some(ONE_REVIEWER));
        assert!(error.contains("no issue o/r#99"), "{error}");
    }
    for args in [
        vec!["run", "start", "--issue", "o/r#99"],
        vec![
            "check",
            "--run",
            &run,
            "--name",
            "n",
            "--outcome",
            "pass",
            "--issue",
            "o/r#99",
        ],
    ] {
        let output = env.bin(&env.repo).arg("qa").args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("no issue o/r#99"), "{error}");
    }
    let after = (
        env.run(&["list"]),
        env.run(&["status", "--full"]),
        qa(env, &["list"]),
    );
    assert_eq!(after, before, "a refused flag stored something");
}

/// Everything the commands could have written: the review store and the issue documents.
fn stored(env: &Env) -> Vec<(std::path::PathBuf, Vec<u8>, std::time::SystemTime)> {
    let mut all = snapshot(&env.state());
    all.extend(snapshot(&env.issue_state()));
    all
}

/// Runs `agt qa <args>` expecting exit 1, returning stderr.
fn qa_fails(env: &Env, args: &[&str]) -> String {
    let output = env.bin(&env.repo).arg("qa").args(args).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{args:?}");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A command its run refuses links nothing: the issue is resolved and the run checked before
/// the review is linked, so a missing run leaves the store and the issue documents untouched.
#[test]
fn a_pass_or_check_on_a_missing_run_links_no_issue() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let before = stored(env);

    let error = env.fail(
        &[
            "pass",
            "start",
            "--run",
            "rv-zzzz",
            "--kind",
            "full",
            "--reviewers",
            "-",
            "--issue",
            "o/r#3",
        ],
        Some(ONE_REVIEWER),
    );
    assert!(error.contains("run rv-zzzz not found"), "{error}");
    let error = qa_fails(
        env,
        &[
            "check",
            "--run",
            "qa-zzzz",
            "--name",
            "ghost",
            "--outcome",
            "pass",
            "--issue",
            "o/r#2",
        ],
    );
    assert!(error.contains("QA run qa-zzzz not found"), "{error}");
    assert_eq!(stored(env), before, "a refused command wrote");
}

/// An ended run refuses a new pass or check, and so links nothing.
#[test]
fn a_pass_or_check_on_an_ended_run_links_no_issue() {
    let setup = setup_with_issue();
    let env = &setup.env;
    env.run(&["run", "end", "--run", &setup.run]);
    let qa_run = qa(env, &["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    qa(env, &["run", "end", "--run", &qa_run]);
    let before = stored(env);

    let error = env.fail(
        &[
            "pass",
            "start",
            "--run",
            &setup.run,
            "--kind",
            "full",
            "--reviewers",
            "-",
            "--issue",
            "o/r#3",
        ],
        Some(ONE_REVIEWER),
    );
    assert!(error.contains("already ended"), "{error}");
    let error = qa_fails(
        env,
        &[
            "check",
            "--run",
            &qa_run,
            "--name",
            "late",
            "--outcome",
            "pass",
            "--issue",
            "o/r#2",
        ],
    );
    assert!(error.contains("already ended"), "{error}");
    assert_eq!(stored(env), before, "a refused command wrote");
}

const ISSUE_FINDING: &str = r#"[{"type":"bug","severity":"high","path":"a.txt","line":3,
  "scenario":"Races the login again.","issue":"o/r#1"}]"#;

/// Ends `pass` of `run` with a finding that names issue 1, expecting a refusal that contains
/// `message`, and checks the store and the issue documents did not change.
fn refused_pass_end(env: &Env, (run, pass, reviewer): (&str, &str, &str), message: &str) {
    let before = stored(env);
    let error = env.fail(
        &[
            "pass",
            "end",
            "--run",
            run,
            "--pass",
            pass,
            "--findings",
            "-",
            "--reviewer",
            reviewer,
        ],
        Some(ISSUE_FINDING),
    );
    assert!(error.contains(message), "{error}");
    assert_eq!(stored(env), before, "a refused pass end wrote");
}

/// A pass end its run or pass refuses links no issue the findings name: the run, the pass and
/// the reviewer are checked before the review is linked, so the store and the issue documents
/// stay byte-identical.
#[test]
fn a_pass_end_on_a_missing_run_links_no_issue() {
    let setup = setup_with_issue();
    refused_pass_end(
        &setup.env,
        ("rv-zzzz", "c1p0", "r1"),
        "run rv-zzzz not found",
    );
}

#[test]
fn a_pass_end_on_a_missing_pass_links_no_issue() {
    let setup = setup_with_issue();
    let open = another_pass_start(&setup.env);
    refused_pass_end(&setup.env, (&open, "c9p9", "r1"), "pass");
}

#[test]
fn a_pass_end_on_an_ended_pass_links_no_issue() {
    let setup = setup_with_issue();
    refused_pass_end(&setup.env, (&setup.run, "c1p0", "r1"), "already ended");
}

#[test]
fn a_pass_end_by_an_unknown_reviewer_links_no_issue() {
    let setup = setup_with_issue();
    let open = another_pass_start(&setup.env);
    refused_pass_end(
        &setup.env,
        (&open, "c1p0", "r9"),
        "reviewer r9 is not in pass",
    );
}

/// A valid pass end still links before the write.
#[test]
fn a_valid_pass_end_still_links_the_issue() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let target = env.run(&["status"])["target"].as_str().unwrap().to_owned();
    let open = another_pass_start(env);
    env.run_stdin(
        &[
            "pass",
            "end",
            "--run",
            &open,
            "--pass",
            "c1p0",
            "--findings",
            "-",
            "--reviewer",
            "r1",
        ],
        ISSUE_FINDING,
    );
    let issue = env.issue_doc(GithubIssues.tracker(), "1");
    assert_eq!(issue["links"]["reviews"], json!([target]), "{issue}");
}

/// Starts a second full-pass run of the reviewed branch and leaves its pass open.
fn another_pass_start(env: &Env) -> String {
    env.run_stdin(
        &["pass", "start", "--kind", "full", "--reviewers", "-"],
        ONE_REVIEWER,
    )["run"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// A finding or QA comment on a check that does not exist, or by an author the QA rules refuse,
/// links nothing; neither does a QA run that cannot take its tester.
#[test]
fn a_qa_command_its_target_refuses_links_no_issue() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let before = stored(env);

    let error = qa_fails(
        env,
        &[
            "finding",
            "--check",
            "qa-zzzz.k1",
            "--type",
            "bug",
            "--severity",
            "low",
            "--body",
            "x",
            "--issue",
            "o/r#3",
        ],
    );
    assert!(error.contains("check qa-zzzz.k1 not found"), "{error}");
    let error = qa_fails(
        env,
        &[
            "comment",
            "--check",
            "qa-zzzz.k1",
            "--body",
            "x",
            "--issue",
            "o/r#3",
        ],
    );
    assert!(error.contains("check qa-zzzz.k1 not found"), "{error}");
    let error = qa_fails(
        env,
        &["run", "start", "--reviewer", "r1", "--issue", "o/r#3"],
    );
    assert!(error.contains("--reviewer"), "{error}");
    assert_eq!(stored(env), before, "a refused command wrote");
}

/// The earlier order still holds for a valid run: the review is linked, then the write.
#[test]
fn a_valid_run_still_links_before_the_write() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let target = env.run(&["status"])["target"].as_str().unwrap().to_owned();
    let run = another_review_run(env, &["--issue", "o/r#3"]);
    assert_eq!(env.run(&["show", &run])["run"]["issue"], "o/r#3");
    let qa_run = qa(env, &["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    qa(
        env,
        &[
            "check",
            "--run",
            &qa_run,
            "--name",
            "ok",
            "--outcome",
            "pass",
            "--issue",
            "o/r#2",
        ],
    );
    for number in ["2", "3"] {
        let issue = env.issue_doc(GithubIssues.tracker(), number);
        assert_eq!(issue["links"]["reviews"], json!([target]), "{issue}");
    }
}

const BAD_REVIEWERS: [(&str, &str); 3] = [
    (r#"[{"harness":"codex"}]"#, "needs a model"),
    (
        r#"[{"id":"x1","model":"m1","harness":"codex"}]"#,
        "reviewer id must look like",
    ),
    (
        r#"[{"id":"r1","model":"m1","harness":"codex"},{"id":"r1","model":"m1","harness":"codex"}]"#,
        "reviewer r1 is listed twice",
    ),
];

/// Reviewers the pass refuses (no model, a bad id, a duplicate) leave the first pass's issue
/// unlinked and the store without the run that was never created.
#[test]
fn a_first_pass_with_bad_reviewers_links_no_issue() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let before = stored(env);
    for (reviewers, message) in BAD_REVIEWERS {
        let error = env.fail(
            &[
                "pass",
                "start",
                "--kind",
                "full",
                "--reviewers",
                "-",
                "--issue",
                "o/r#3",
            ],
            Some(reviewers),
        );
        assert!(error.contains(message), "{reviewers}: {error}");
        assert_eq!(stored(env), before, "{reviewers}: a refused pass wrote");
    }
}

/// A dry run refuses the reviewers a real start refuses, with the same message.
#[test]
fn a_pass_start_dry_run_refuses_the_reviewers_a_real_start_refuses() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let run = another_review_run(env, &[]);
    let before = stored(env);
    for (reviewers, message) in BAD_REVIEWERS {
        let args = [
            "pass",
            "start",
            "--run",
            &run,
            "--kind",
            "fix-check",
            "--reviewers",
            "-",
        ];
        let real = env.fail(&args, Some(reviewers));
        assert!(real.contains(message), "{reviewers}: {real}");
        let dry = env.fail(&[&args[..], &["--dry-run"]].concat(), Some(reviewers));
        assert_eq!(dry, real, "{reviewers}");
    }
    assert_eq!(stored(env), before, "a refused pass wrote");
}

/// The same holds for the pass that starts the run.
#[test]
fn a_first_pass_dry_run_refuses_the_reviewers_a_real_start_refuses() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let before = stored(env);
    for (reviewers, message) in BAD_REVIEWERS {
        let args = ["pass", "start", "--kind", "full", "--reviewers", "-"];
        let real = env.fail(&args, Some(reviewers));
        assert!(real.contains(message), "{reviewers}: {real}");
        let dry = env.fail(&[&args[..], &["--dry-run"]].concat(), Some(reviewers));
        assert_eq!(dry, real, "{reviewers}");
    }
    assert_eq!(stored(env), before, "a refused pass wrote");
}

/// A check whose reviewed state cannot be read links nothing.
#[test]
fn a_qa_check_whose_tree_cannot_be_read_links_no_issue() {
    let setup = setup_with_issue();
    let env = &setup.env;
    let run = qa(env, &["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    // A required clean filter that fails makes `git add -A`, which reads the tree, fail.
    std::fs::write(env.repo.join(".gitattributes"), "boom.txt filter=boom\n").unwrap();
    std::fs::write(env.repo.join("boom.txt"), "x").unwrap();
    for (key, value) in [
        ("filter.boom.clean", "false"),
        ("filter.boom.required", "true"),
    ] {
        env.git(&["config", key, value]);
    }
    let before = stored(env);
    let error = qa_fails(
        env,
        &[
            "check",
            "--run",
            &run,
            "--name",
            "login",
            "--outcome",
            "pass",
            "--issue",
            "o/r#3",
        ],
    );
    assert!(error.contains("boom"), "{error}");
    assert_eq!(stored(env), before, "a refused check wrote");
}
