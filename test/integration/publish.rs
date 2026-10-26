//! Publish scenarios that belong to one service: its payload shapes, its crash recovery and
//! its approval calls. What holds for every service is in `matrix.rs`.

use serde_json::{Value, json};

use crate::support::forge::github::{self, Github};
use crate::support::forge::gitlab::{self, Gitlab};
use crate::support::forge::{Call, Comment, FakeService, Remote};
use crate::support::scenario::{
    PUBLISH, Review, Setup, assert_never_posted, count_calls, ids, kinds, open_ids, sent, thread,
    write_starting, write_text, writes_for,
};
use crate::support::{Env, args, executable, fixture, numbered, strs};

// GitHub

/// A comment marked `posting` is stored and shown without a `began_at` key: agent-review 0.4.0
/// refuses to write documents with fields it does not know.
#[test]
fn review_documents_and_output_carry_no_began_at() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    svc.link(&setup);
    svc.lose(env, Call::PostInline);
    env.fail(&["publish", "--pr", "42"], None);
    let list = env.run(&["list"]);
    assert_eq!(
        thread(&list, &setup.finding(1))["comments"][0]["post"],
        "posting"
    );
    assert!(!list.to_string().contains("began_at"), "{list}");
    let mut stored = String::new();
    let mut pending = vec![env.state()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if let Ok(content) = std::fs::read_to_string(&path) {
                stored.push_str(&content);
            }
        }
    }
    assert!(stored.contains("posting"), "the marked comment is stored");
    assert!(!stored.contains("began_at"), "{stored}");
}

/// A write that reached GitHub but lost its response is matched by its marker on the next
/// run, never posted again.
#[test]
fn github_crash_after_write_does_not_duplicate() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f1 = setup.finding(1);
    // A general finding is not posted to GitHub, so the lost general comment is a note.
    env.run(&["comment", "--body", "A note of mine.", "--author", "me"]);
    svc.link(&setup);
    svc.lose(env, Call::PostInline);
    env.fail(&["publish", "--pr", "42"], None);
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f1)["comments"][0]["post"], "posting");
    assert!(thread(&list, &f1)["comments"][0]["attempt"].is_string());

    let mut remote = Remote::new();
    remote.thread(
        "PRRT_f1",
        false,
        3,
        vec![Comment::new(501, "me", &sent(env, &f1))],
    );
    remote.reviews.push(github::own_review(&setup.head));
    svc.sync(&setup, &remote);
    svc.lose(env, Call::PostNote);
    env.fail(&["publish", "--pr", "42"], None);
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f1)["comments"][0]["remote"]["id"], "501");
    assert_eq!(thread(&list, "h1")["comments"][0]["post"], "posting");

    remote.general("601", Comment::new(601, "me", &sent(env, "h1")));
    svc.sync(&setup, &remote);
    svc.restore(env, Call::PostNote);
    svc.accept(env, Call::PostNote, 604);
    let published = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&published, "posted"), ["summary"]);
    assert_eq!(svc.count(env, Call::PostInline), 1);
    assert_eq!(
        svc.count(env, Call::PostNote),
        2,
        "the note once, the summary once"
    );
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, "h1")["comments"][0]["remote"]["id"], "601");
    assert_never_posted(env);
}

/// The summary is one comment: the next run's publish edits it in place, and when the comment
/// is gone from the PR the summary is posted anew.
#[test]
fn the_summary_is_edited_in_place_and_posted_anew_when_deleted() {
    // A run whose one pass finds nothing.
    let quiet_run = |env: &Env| {
        let run = env.run_stdin(
            &["pass", "start", "--kind", "full", "--reviewers", "-"],
            PUBLISH.reviewers,
        )["run"]
            .as_str()
            .unwrap()
            .to_owned();
        env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
        env.run(&["run", "end", "--run", &run]);
        run
    };
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    for finding in [setup.finding(1), setup.finding(2)] {
        env.run(&["resolve", &finding, "--stale"]);
    }
    svc.link(&setup);
    svc.accept(env, Call::PostNote, 604);
    let published = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&published, "posted"), ["summary"]);
    let sent = summary_sent(env);
    assert!(
        !sent.contains("not posted inline"),
        "a finding withdrawn before it was posted gets no note with its text: {sent}"
    );
    assert!(!sent.contains("General text of f2"), "{sent}");
    let remote_ids = |env: &Env| -> Vec<Value> {
        env.run(&["status", "--full"])["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|run| run["record"]["remote_id"].clone())
            .collect()
    };

    let patch = [
        "api",
        "--method",
        "PATCH",
        "repos/me/repo/issues/comments/604",
        "--input",
        "-",
    ];
    env.respond(
        &patch,
        r#"{"id":604,"html_url":"https://github.com/me/repo/pull/42#issuecomment-604"}"#,
    );
    let second = quiet_run(env);
    let published = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&published, "posted"), ["summary"]);
    assert_eq!(
        svc.count(env, Call::PostNote),
        1,
        "edited, not posted again"
    );
    let inputs = env.forge_inputs();
    let (_, edit) = inputs
        .iter()
        .rfind(|(call, _)| call.contains("PATCH"))
        .unwrap();
    let edit: Value = serde_json::from_str(edit).unwrap();
    let edit = edit["body"].as_str().unwrap();
    for run in [&setup.run, &second] {
        assert!(
            edit.contains(&format!("<!-- review-record v2 id={run} ")),
            "{edit}"
        );
    }
    assert!(edit.contains("2 reviewers, m1"), "{edit}");
    assert_eq!(remote_ids(env), [json!("604"), json!("604")]);
    let again = env.run(&["publish", "--pr", "42"]);
    assert_eq!(
        again["posted"],
        json!([]),
        "an unchanged summary is not edited"
    );

    env.fail_call_with(&patch, "gh: Not Found (HTTP 404)");
    svc.accept(env, Call::PostNote, 605);
    quiet_run(env);
    let published = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&published, "posted"), ["summary"]);
    assert_eq!(svc.count(env, Call::PostNote), 2);
    assert_eq!(remote_ids(env), [json!("605"), json!("605"), json!("605")]);
}

/// The text the summary was last sent with.
fn summary_sent(env: &Env) -> String {
    env.run(&["status", "--full"])["runs"][0]["record"]["sent"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// While the host still shows the old summary, the failed edit stays due and goes out again.
#[test]
fn a_failed_summary_edit_is_sent_again() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    for finding in [setup.finding(1), setup.finding(2)] {
        env.run(&["resolve", &finding, "--stale"]);
    }
    svc.link(&setup);
    svc.accept(env, Call::PostNote, 604);
    env.run(&["publish", "--pr", "42"]);
    let first = summary_sent(env);
    // One more commit changes the summary text.
    env.write("c.txt", "c\n");
    let head = env.commit_all("more");
    let mut remote = Remote::new().head(head);
    remote.general("c604", Comment::new(604, "me", &first));
    svc.sync(&setup, &remote);
    let patch = [
        "api",
        "--method",
        "PATCH",
        "repos/me/repo/issues/comments/604",
        "--input",
        "-",
    ];
    env.fail_call_with(&patch, "gh: Server Error (HTTP 502)");
    env.fail(&["publish", "--pr", "42"], None);
    assert_ne!(summary_sent(env), first);

    env.pass_call(&patch);
    env.respond(
        &patch,
        r#"{"id":604,"html_url":"https://github.com/me/repo/pull/42#issuecomment-604"}"#,
    );
    let published = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&published, "posted"), ["summary"]);
    let record = &env.run(&["status", "--full"])["runs"][0]["record"];
    assert_eq!(record["post"], "posted", "{record}");
    assert_eq!(record.get("earlier"), None, "{record}");
}

/// A finding that quotes a marker leaves the summary's own id alone.
#[test]
fn a_marker_quoted_in_a_finding_is_not_the_summary_s() {
    const QUOTING: crate::support::scenario::Review = crate::support::scenario::Review {
        reviewers: r#"[{"model":"m1","effort":"xhigh","harness":"claude-code"}]"#,
        findings: r#"[
  {"type":"bug","severity":"low","scenario":"`<!-- agent-review id=x -->` is read first."}
]"#,
    };
    let svc = Github::new();
    let setup = Setup::branch(svc.origin(), &QUOTING, false, false);
    let env = &setup.env;
    // The finding has no place in the diff, so GitHub lists its whole text in the summary.
    svc.link(&setup);
    svc.accept(env, Call::PostNote, 604);
    env.run(&["publish", "--pr", "42"]);
    let sent = summary_sent(env);
    assert!(sent.contains("`<!-- agent-review id=x -->`"), "{sent}");
    let mut remote = Remote::new();
    remote.general("c604", Comment::new(604, "me", &sent));
    svc.sync(&setup, &remote);
    svc.accept(env, Call::PostNote, 605);
    let again = env.run(&["publish", "--pr", "42"]);
    assert_eq!(
        again["posted"],
        json!([]),
        "an unchanged summary is not posted again"
    );
}

/// A general comment on GitHub cannot be resolved, so a finding without a place in the diff is
/// listed in the summary with its whole text instead of being posted.
#[test]
fn github_a_general_finding_is_skipped_and_listed_in_the_summary() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f2 = setup.finding(2);
    svc.link(&setup);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert_eq!(
        dry["skipped"],
        json!([{
            "item": f2,
            "reason": "listed in the summary: a general comment here cannot be resolved",
        }])
    );
    assert_eq!(kinds(&dry, "writes"), ["review", "summary"], "{dry}");
    let summary = dry["writes"][1]["input"]["body"].as_str().unwrap();
    assert!(summary.starts_with("**⚠️ 2 to fix** at `"), "{summary}");
    assert!(
        summary.contains("2. ✏️ **General text of f2.**\n   whole PR\n"),
        "{summary}"
    );
    assert!(!summary.contains("   > General text of f2."), "{summary}");
    for write in dry["writes"].as_array().unwrap() {
        assert!(!write["items"].to_string().contains(&f2), "{dry}");
    }

    // Published, the finding stays off the PR and out of the posted threads.
    svc.accept(env, Call::PostInline, 700);
    env.respond(
        &[
            "api",
            "--method",
            "GET",
            "repos/me/repo/pulls/42/reviews/700/comments",
        ],
        &json!([{
            "id": 501,
            "body": crate::support::scenario::marked("f1", &format!("{}/c1", setup.finding(1))),
            "html_url": "https://github.com/me/repo/pull/42#discussion_r501",
        }])
        .to_string(),
    );
    svc.accept(env, Call::PostNote, 604);
    let published = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&published, "posted"), ["review", "summary"]);
    assert_eq!(
        svc.count(env, Call::PostThread),
        svc.count(env, Call::PostNote)
    );
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f2)["comments"][0]["post"], "pending");
}

/// An agent never posts a Conversation comment: on someone else's PR a finding with no diff line
/// goes in the body of the review that carries the in-diff comments, and sync leaves it posted.
#[test]
fn github_findings_without_a_diff_line_go_in_the_review_body_on_someone_elses_pr() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let (f1, f2) = (setup.finding(1), setup.finding(2));
    let mut remote = Remote::by("alice");
    svc.sync(&setup, &remote);
    env.run(&["sync", "--pr", "42"]);
    env.run(&["run", "end", "--run", &setup.run]);
    let only = format!("{f1},{f2}");
    let args = [
        "publish",
        "--pr",
        "42",
        "--not-own",
        "--only",
        only.as_str(),
    ];
    let dry = env.run(&[args.as_slice(), &["--dry-run"]].concat());
    assert_eq!(kinds(&dry, "writes"), ["review"], "{dry}");
    let review = &dry["writes"][0];
    assert_eq!(
        review["items"],
        json!([format!("{f1}/c1"), format!("{f2}/c1")]),
        "{dry}"
    );
    assert_eq!(review["input"]["comments"].as_array().unwrap().len(), 1);
    let body = review["input"]["body"].as_str().unwrap().to_owned();
    assert_eq!(
        body,
        format!(
            "**✏️ nit**\n\nGeneral text of f2.\n\n> <sub>🔍 agt review · m1</sub>\n\
             <!-- agent-review id={f2}/c1 attempt=<attempt> -->\n\n\
             <!-- agent-review id=review/{} -->",
            setup.head
        )
    );

    svc.accept(env, Call::PostInline, 700);
    env.respond(
        &[
            "api",
            "--method",
            "GET",
            "repos/me/repo/pulls/42/reviews/700/comments",
        ],
        &json!([{
            "id": 501,
            "body": crate::support::scenario::marked("f1", &format!("{f1}/c1")),
            "html_url": "https://github.com/me/repo/pull/42#discussion_r501",
        }])
        .to_string(),
    );
    let published = env.run(&args);
    assert_eq!(kinds(&published, "posted"), ["review"], "{published}");
    let list = env.run(&["list"]);
    let comment = &thread(&list, &f2)["comments"][0];
    assert_eq!(comment["post"], "posted", "{comment}");
    assert_eq!(comment["remote"]["id"], "700", "{comment}");

    // The review reads back as one body: nothing is imported, deleted or posted again.
    remote
        .reviews
        .push(github::review(700, "me", "COMMENTED", &body));
    svc.sync(&setup, &remote);
    env.run(&["sync", "--pr", "42"]);
    let list = env.run(&["list"]);
    assert!(!ids(&list).iter().any(|id| id.starts_with("gh-")), "{list}");
    assert_eq!(thread(&list, &f2)["comments"][0]["deleted_remote"], false);
    let again = env.run(&[args.as_slice(), &["--dry-run"]].concat());
    assert_eq!(again["writes"], json!([]), "{again}");
}

/// On your own PR an agent's comment with no diff line has nowhere to go: it is skipped.
#[test]
fn github_an_agents_plain_comment_outside_the_diff_is_skipped_on_your_own_pr() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    svc.link(&setup);
    let note = env.run(&[
        "comment",
        "--body",
        "Agent note.",
        "--reviewer",
        &format!("{}.c1p0.fix", setup.run),
        "--model",
        "m1",
        "--effort",
        "high",
        "--harness",
        "codex",
    ])["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert!(writes_for(&dry, &format!("{note}/c1")).is_empty(), "{dry}");
    assert!(
        dry["skipped"].as_array().unwrap().iter().any(|item| {
            item["item"] == note.as_str()
                && item["reason"]
                    .as_str()
                    .unwrap()
                    .contains("does not post a general comment on your own PR")
        }),
        "{dry}"
    );
}

/// An agent's reply to a Conversation comment is not posted (a Conversation comment has no
/// reply); yours is a quote reply.
#[test]
fn github_only_you_answer_a_conversation_comment() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let mut remote = Remote::by("me");
    remote.general("900", Comment::new(900, "alice", "Alice asks why.\nMore."));
    svc.sync(&setup, &remote);
    env.run(&["sync", "--pr", "42"]);
    env.run(&["run", "end", "--run", &setup.run]);
    let alice = svc.local_general(900);
    env.run(&["reply", &alice, "--body", "Because of X.", "--author", "me"]);
    let agent = env.run(&[
        "reply",
        &alice,
        "--body",
        "Agent answer.",
        "--reviewer",
        &format!("{}.c1p0.fix", setup.run),
        "--model",
        "m1",
        "--effort",
        "high",
        "--harness",
        "codex",
    ]);
    let agent = agent["thread"]["comments"][2]["id"].as_str().unwrap();
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    let reply = write_starting(&dry, "body", "> Alice asks why.");
    assert!(
        reply.starts_with("> Alice asks why.\n> — @alice, ["),
        "{reply}"
    );
    assert!(
        reply.contains("\n\nBecause of X.\n<!-- agent-review id="),
        "{reply}"
    );
    assert!(!reply.contains("More."), "{reply}");
    assert!(
        dry["skipped"].as_array().unwrap().iter().any(|item| {
            item["item"] == agent
                && item["reason"]
                    .as_str()
                    .unwrap()
                    .contains("does not post a general comment as a reply")
        }),
        "{dry}"
    );
}

const FILE_LEVEL: crate::support::scenario::Review = crate::support::scenario::Review {
    reviewers: r#"[{"model":"m1","effort":"xhigh","harness":"codex"}]"#,
    findings: r#"[
  {"type":"nit","path":"a.txt","line":9,"scenario":"Outside the hunks."},
  {"type":"nit","path":"a.txt","scenario":"About the whole file."}
]"#,
};

/// A finding on a file the PR changes whose lines are not in the diff, or on the whole file, is a
/// comment on the file, which a review thread can resolve.
#[test]
fn github_a_finding_on_a_changed_file_outside_the_hunks_posts_a_file_comment() {
    let svc = Github::new();
    let setup = Setup::branch(svc.origin(), &FILE_LEVEL, false, false);
    let env = &setup.env;
    let (line, whole) = (setup.finding(1), setup.finding(2));
    svc.link(&setup);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert_eq!(dry["skipped"], json!([]), "{dry}");
    assert_eq!(
        kinds(&dry, "writes"),
        ["thread", "thread", "summary"],
        "{dry}"
    );
    let comments = json!([
        "api",
        "--method",
        "POST",
        "repos/me/repo/pulls/42/comments",
        "--input",
        "-"
    ]);
    let posts = &dry["writes"][0];
    assert_eq!(posts["items"], json!([format!("{line}/c1")]));
    assert_eq!(posts["call"], comments);
    let input = &posts["input"];
    assert_eq!(input["subject_type"], "file");
    assert_eq!(input["path"], "a.txt");
    assert_eq!(input["commit_id"], setup.head.as_str());
    let body = input["body"].as_str().unwrap();
    assert!(
        body.starts_with("`a.txt:9`\n\n**✏️ nit**\n\nOutside the hunks."),
        "{body}"
    );

    let whole_post = &dry["writes"][1];
    assert_eq!(whole_post["items"], json!([format!("{whole}/c1")]));
    assert_eq!(whole_post["call"], comments);
    assert_eq!(whole_post["input"]["subject_type"], "file");
    let body = whole_post["input"]["body"].as_str().unwrap();
    assert!(
        body.starts_with("**✏️ nit**\n\nAbout the whole file."),
        "a whole-file anchor has no line to name: {body}"
    );
    assert!(
        dry["writes"][2]["input"]["body"]
            .as_str()
            .unwrap()
            .contains("Outside the hunks.")
    );

    // Published, each is a comment of a review thread once the host lists it.
    let call = [
        "api",
        "--method",
        "POST",
        "repos/me/repo/pulls/42/comments",
        "--input",
        "-",
    ];
    env.respond_nth(
        &call,
        1,
        r#"{"id":701,"html_url":"https://github.com/me/repo/pull/42#discussion_r701"}"#,
    );
    env.respond_nth(
        &call,
        2,
        r#"{"id":702,"html_url":"https://github.com/me/repo/pull/42#discussion_r702"}"#,
    );
    svc.accept(env, Call::PostNote, 604);
    let published = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&published, "posted"), ["thread", "thread", "summary"]);
    let list = env.run(&["list"]);
    let first = &thread(&list, &line)["comments"][0];
    assert_eq!(first["post"], "posted", "{first}");
    assert_eq!(first["remote"]["id"], "701");
    // The host lists each as a thread of a review; sync claims it by its marker and knows it as
    // a review-thread (the expected remote kind ReviewThread), to which replies and resolves go natively.
    let mut remote = Remote::new();
    remote.thread(
        "PRRT_line",
        false,
        9,
        vec![Comment::new(701, "me", &sent(env, &line))],
    );
    remote.thread(
        "PRRT_whole",
        false,
        1,
        vec![Comment::new(702, "me", &sent(env, &whole))],
    );
    svc.sync(&setup, &remote);
    env.run(&["sync", "--pr", "42"]);
    let list = env.run(&["list"]);
    for (id, remote_id) in [(&line, "PRRT_line"), (&whole, "PRRT_whole")] {
        let remote = &thread(&list, id)["remote"];
        assert_eq!(remote["id"], remote_id, "{remote}");
        assert_eq!(remote["kind"], "review-thread", "{remote}");
    }
}

#[test]
fn concurrent_publish_posts_a_pending_comment_once() {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let svc = Gitlab::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    svc.sync(&setup, &Remote::new());
    env.run(&["sync", "--pr", "3"]);
    env.run(&["comment", "--body", "Post once"]);
    env.respond(
        &strs(&svc.call(Call::PostThread)),
        r#"{"id":"d-h1","notes":[{"id":5001}]}"#,
    );
    let mut remote = Remote::new();
    remote.general("d-h1", Comment::new(5001, "me", "{{LAST_BODY}}"));
    svc.sync(&setup, &remote);
    env.reset_calls();

    let wrapper = env.path("publish-forge.sh");
    executable(
        &wrapper,
        r#"#!/bin/sh
set -eu
case "$*" in
  'api -X GET --paginate projects/me%2Fproj/merge_requests/3/discussions?per_page=100')
    if [ ! -f "$FAKE_FORGE_DIR/posted" ]; then
      printf '[]\n'
      exit 0
    fi
    ;;
  'api -X POST projects/me%2Fproj/merge_requests/3/discussions -H Content-Type: application/json --input -')
    printf '%s\n' "$$" >> "$FAKE_FORGE_DIR/arrivals"
    IFS= read -r release < "$FAKE_FORGE_DIR/release"
    "$FAKE_FORGE_REAL" "$@"
    touch "$FAKE_FORGE_DIR/posted"
    exit 0
    ;;
esac
exec "$FAKE_FORGE_REAL" "$@"
"#,
    );
    let fifo = env.path("forge/release");
    assert!(
        env.command("mkfifo", &env.repo)
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let mut release = OpenOptions::new()
        .read(true)
        .write(true)
        .open(fifo)
        .unwrap();
    let spawn = || {
        env.agt(&env.repo)
            .args(["--target", "gl-3", "publish", "--only", "h1"])
            .env("AGT_GLAB_BIN", &wrapper)
            .env(
                "FAKE_FORGE_REAL",
                concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixtures/fake-forge.sh"),
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let wait_for = |check: &dyn Fn() -> bool, limit: Duration| {
        let until = Instant::now() + limit;
        while Instant::now() < until {
            if check() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    };
    let arrivals = || {
        fs::read_to_string(env.path("forge/arrivals"))
            .unwrap_or_default()
            .lines()
            .count()
    };
    let first = spawn();
    let first_ready = wait_for(&|| arrivals() >= 1, Duration::from_secs(5));
    let second = spawn();
    let authenticated = wait_for(
        &|| count_calls(env, &args(&["api", "-X", "GET", "user"])) >= 2,
        Duration::from_secs(5),
    );
    wait_for(&|| arrivals() >= 2, Duration::from_secs(1));
    release.write_all(b"go\ngo\n").unwrap();
    for child in [first, second] {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        first_ready && authenticated,
        "both publishers must reach the controlled race"
    );
    assert_eq!(svc.count(env, Call::PostThread), 1);
    assert_eq!(
        thread(&env.run(&["list"]), "h1")["comments"][0]["post"],
        "posted"
    );
}

#[test]
fn own_review_body_is_not_imported() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let mut remote = Remote::new();
    remote.reviews.push(github::own_review(&setup.head));
    svc.sync(&setup, &remote);
    env.run(&["sync", "--pr", "42"]);
    assert!(!ids(&env.run(&["list"])).contains(&"gh-r700".to_owned()));
}

#[test]
fn closed_unrelated_pr_is_not_attached_or_pruned() {
    let svc = Github::new();
    let env = Env::with_branch();
    env.git(&["remote", "add", "origin", svc.origin()]);
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let base = env.git(&["rev-parse", "main"]);
    let branch_key = env.run(&["pass", "start", "--kind", "full"])["target"]
        .as_str()
        .unwrap()
        .to_owned();
    github::login(&env);
    github::pulls(&env, "me/repo", r#"[{"number":5,"state":"closed"}]"#);
    github::empty_pr(
        &env,
        "me/repo",
        "5",
        "CLOSED",
        "\"2020-01-01T00:00:00Z\"",
        "1111111111111111111111111111111111111111",
        &base,
    );
    let synced = env.run(&["sync"]);
    assert_eq!(synced["target"], "gh-5");
    assert_eq!(synced["pruned"], json!([]), "synced just now");
    let status = env.run(&["status", "--full"]);
    assert_eq!(status["target"], branch_key, "the branch target stays");
    assert_eq!(status["runs"].as_array().unwrap().len(), 1);
    assert_eq!(status["review"], Value::Null);
}

#[test]
fn a_synced_rejected_reply_resolves_a_general_finding() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    svc.link(&setup);
    let f9 = format!("{}.c1p0.r1.f9", setup.run);
    let first = format!(
        "**bug (low):** A general finding.\n\nm1 xhigh codex {}.c1p0.r1\n\
         <!-- review-finding v1 id={f9} sha={} -->\n<!-- agent-review id={f9}/c1 attempt=at-1 -->",
        setup.run, setup.head
    );
    let mut remote = Remote::new();
    remote.general("901", Comment::new(901, "me", &first));
    remote.general(
        "902",
        Comment::new(
            902,
            "me",
            &format!(
                "Rejected: Intended behavior.\n\nme\n<!-- agent-review id={f9}/c2 attempt=at-1 -->\nRe: {f9}"
            ),
        ),
    );
    svc.sync(&setup, &remote);
    env.run(&["sync", "--pr", "42"]);
    let list = env.run(&["list"]);
    let synced = thread(&list, &f9);
    assert_eq!(synced["resolved"], true, "{synced}");
    assert_eq!(synced["resolution"], "rejected: Intended behavior");
    assert!(!open_ids(&env.run(&["status"])).contains(&f9));
}

#[test]
fn github_approve_review_claimed_by_sync_is_not_sent_again() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    env.run(&["resolve", &setup.finding(1), "--stale"]);
    env.run(&["finding", "kill", &setup.finding(2), "--reason", "x"]);
    let mut remote = Remote::by("alice");
    svc.sync(&setup, &remote);
    env.run(&["sync", "--pr", "42"]);
    let id = env.approve("42", &["--formal", "--not-own"]);
    env.run(&["run", "end", "--run", &setup.run]);
    // The review lands on GitHub, but its response is lost.
    svc.lose(env, Call::Approve);
    env.fail(&["publish", "--pr", "42", "--not-own", "--only", &id], None);
    remote
        .reviews
        .push(github::review(701, "me", "APPROVED", &sent(env, &id)));
    svc.sync(&setup, &remote);
    // The posted review holds only the marker; the approval text stays in the store.
    env.run(&["sync", "--pr", "42"]);
    let list = env.run(&["list"]);
    let approval = &thread(&list, &id)["comments"][0];
    assert_eq!(approval["remote"]["id"], "701", "{approval}");
    assert!(
        approval["body"].as_str().unwrap().contains("Approved at"),
        "{approval}"
    );
    svc.restore(env, Call::Approve);
    let out = env.run(&["publish", "--pr", "42", "--not-own", "--only", &id]);
    assert_eq!(svc.count(env, Call::Approve), 1, "{out}");
    assert!(!out["skipped"].to_string().contains(&id), "{out}");
}

#[test]
fn own_github_formal_approval_posts_a_comment_review() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    env.run(&["resolve", &setup.finding(1), "--stale"]);
    env.run(&["finding", "kill", &setup.finding(2), "--reason", "x"]);
    svc.link(&setup);
    let id = env.approve("42", &[]);
    let comments = svc.call(Call::PostNote);
    env.respond_nth(&strs(&comments), 1, r#"{"id":610,"html_url":"u"}"#);
    env.respond_nth(&strs(&comments), 2, r#"{"id":611,"html_url":"u"}"#);
    env.run(&["publish", "--pr", "42"]);
    env.approve("42", &["--formal"]);
    let text = thread(&env.run(&["list"]), &id)["comments"][0]["body"].clone();
    // GitHub does not let the author approve their own PR: a comment review carries it.
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert!(
        dry["skipped"].to_string().contains(&format!("{id}/formal")),
        "{dry}"
    );
    let writes = dry["writes"].as_array().unwrap();
    assert_eq!(writes.len(), 1, "{dry}");
    assert_eq!(writes[0]["items"], json!([format!("{id}/c1")]), "{dry}");
    assert_eq!(writes[0]["input"]["event"], "COMMENT", "{dry}");
    let body = writes[0]["input"]["body"].as_str().unwrap();
    assert!(body.starts_with("Approved at"), "{dry}");
    svc.accept(env, Call::Approve, 702);
    env.run(&["publish", "--pr", "42"]);
    assert_eq!(svc.count(env, Call::Approve), 1);
    // Sync claims the review and keeps the approval text as stored.
    let mut remote = Remote::new();
    remote
        .reviews
        .push(github::review(702, "me", "COMMENTED", &sent(env, &id)));
    svc.sync(&setup, &remote);
    env.run(&["sync", "--pr", "42"]);
    let list = env.run(&["list"]);
    let approval = &thread(&list, &id)["comments"][0];
    assert_eq!(approval["remote"]["id"], "702", "{approval}");
    assert_eq!(approval["body"], text, "{approval}");
    let again = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert_eq!(again["writes"], json!([]), "{again}");
    assert!(!again["skipped"].to_string().contains(&id), "{again}");
}

/// Commits no review read are the whole headline when nothing is open, and are named below.
#[test]
fn github_unreviewed_commits_head_the_summary_when_nothing_is_open() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    for finding in [setup.finding(1), setup.finding(2)] {
        env.run(&["resolve", &finding, "--stale"]);
    }
    svc.link(&setup);
    // A push after the review ended.
    env.write("b.txt", "b\n");
    let later = env.commit_all("later");
    let pushed = Setup {
        head: later.clone(),
        ..setup
    };
    svc.sync(&pushed, &Remote::new());
    let env = &pushed.env;
    env.run(&["sync", "--pr", "42"]);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    let summary = write_text(&dry, "body", "review-record");
    assert!(
        summary.starts_with(&format!(
            "**⏳ 1 new commit not reviewed** at `{}`\n\nNot reviewed: `{}`\n",
            &later[..7],
            &later[..7]
        )),
        "{summary}"
    );
}

/// A response lost after a review with body comments landed leaves them unclaimed, unless each
/// carries its own marker in the body: sync then claims them, and they are not posted again.
#[test]
fn github_review_body_comments_are_claimed_by_sync_after_a_lost_response() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f2 = setup.finding(2);
    let mut remote = Remote::by("alice");
    svc.sync(&setup, &remote);
    env.run(&["sync", "--pr", "42"]);
    env.run(&["run", "end", "--run", &setup.run]);
    let args = ["publish", "--pr", "42", "--not-own", "--only", f2.as_str()];
    svc.lose(env, Call::PostInline);
    env.fail(&args, None);
    let body = format!(
        "{}\n\n<!-- agent-review id=review/{} -->",
        sent(env, &f2),
        setup.head
    );
    remote
        .reviews
        .push(github::review(700, "me", "COMMENTED", &body));
    svc.sync(&setup, &remote);
    svc.restore(env, Call::PostInline);
    env.run(&["sync", "--pr", "42"]);
    let list = env.run(&["list"]);
    let comment = &thread(&list, &f2)["comments"][0];
    assert_eq!(comment["post"], "posted", "{comment}");
    assert_eq!(comment["remote"]["id"], "700", "{comment}");
    let again = env.run(&[args.as_slice(), &["--dry-run"]].concat());
    assert_eq!(again["writes"], json!([]), "{again}");
}

/// Body comments that pass the size limit of one review body go in more reviews.
#[test]
fn github_review_body_comments_are_split_across_reviews_at_the_size_limit() {
    let svc = Github::new();
    let finding = |number: u32| {
        format!(
            r#"{{"type":"nit","scenario":"Finding {number}. {}"}}"#,
            "x".repeat(25_000)
        )
    };
    let findings = format!("[{}]", (1..=5).map(finding).collect::<Vec<_>>().join(","));
    let setup = Setup::branch(
        svc.origin(),
        &Review {
            reviewers: PUBLISH.reviewers,
            findings: &findings,
        },
        false,
        false,
    );
    let env = &setup.env;
    svc.sync(&setup, &Remote::by("alice"));
    env.run(&["sync", "--pr", "42"]);
    env.run(&["run", "end", "--run", &setup.run]);
    let only: Vec<String> = (1..=5).map(|number| setup.finding(number)).collect();
    let dry = env.run(&[
        "publish",
        "--pr",
        "42",
        "--not-own",
        "--only",
        &only.join(","),
        "--dry-run",
    ]);
    assert_eq!(
        kinds(&dry, "writes"),
        ["review", "review", "review"],
        "{dry}"
    );
    for write in dry["writes"].as_array().unwrap() {
        let body = write["input"]["body"].as_str().unwrap();
        assert!(body.chars().count() <= 65_536, "{}", body.chars().count());
    }
    for finding in &only {
        assert_eq!(writes_for(&dry, &format!("{finding}/c1")), ["review"]);
    }
}

/// A PR head the local repository lacks is not claimed as reviewed: the headline names the
/// newest head a run read, and the head is listed as not reviewed.
#[test]
fn github_a_head_missing_locally_is_not_claimed_as_reviewed() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    for finding in [setup.finding(1), setup.finding(2)] {
        env.run(&["resolve", &finding, "--stale"]);
    }
    svc.link(&setup);
    let reviewed = setup.head.clone();
    // A commit made on the host only, such as a committed suggestion.
    let host = "f".repeat(40);
    let pushed = Setup {
        head: host.clone(),
        ..setup
    };
    svc.sync(&pushed, &Remote::new());
    let env = &pushed.env;
    env.run(&["sync", "--pr", "42"]);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    let summary = write_text(&dry, "body", "review-record");
    assert!(
        summary.starts_with(&format!(
            "**⏳ 1 new commit not reviewed** at `{}`\n\nNot reviewed: `{}` (not in the local repository)\n",
            &reviewed[..7],
            &host[..7]
        )),
        "{summary}"
    );
}

/// A summary past the size limit continues in part comments, each posted once and then edited
/// in place, and the summary points at them once their urls are known.
#[test]
fn github_a_long_summary_continues_in_parts() {
    let svc = Github::new();
    let finding = format!(
        r#"{{"type":"nit","scenario":"Sentence one. {}"}}"#,
        "long text ".repeat(190)
    );
    let findings = format!("[{}]", vec![finding; 45].join(","));
    let setup = Setup::branch(
        svc.origin(),
        &Review {
            reviewers: PUBLISH.reviewers,
            findings: &findings,
        },
        false,
        false,
    );
    let env = &setup.env;
    svc.link(&setup);
    for (nth, id) in [(1, 601), (2, 602)] {
        env.respond_nth(
            &strs(&svc.call(Call::PostNote)),
            nth,
            &format!(
                r#"{{"id":{id},"html_url":"https://github.com/me/repo/pull/42#issuecomment-{id}"}}"#
            ),
        );
    }
    let published = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&published, "posted"), ["summary", "summary"]);
    assert_eq!(published["posted"][0]["items"], json!(["summary"]));
    assert_eq!(published["posted"][1]["items"], json!(["summary/p2"]));
    let record = env.run(&["status", "--full"])["runs"][0]["record"].clone();
    assert_eq!(record["remote_id"], "601", "{record}");
    assert_eq!(record["parts"][0]["remote_id"], "602", "{record}");
    let summary = record["sent"].as_str().unwrap();
    assert!(summary.chars().count() <= 65_000);
    assert!(summary.starts_with("**⚠️ 45 to fix** at `"), "{summary}");
    assert!(
        summary.contains("continue in part 2 below."),
        "the url of part 2 is not known yet: {summary}"
    );
    let part = record["parts"][0]["sent"].as_str().unwrap();
    assert!(part.starts_with("Part 2 of the agt summary.\n\n"), "{part}");
    assert!(part.contains("45. ✏️ **Sentence one.**"), "{part}");
    assert!(!part.contains("to fix"), "a part holds no headline: {part}");
    assert!(
        part.contains("<!-- agent-review id=summary/p2 attempt="),
        "{part}"
    );

    // With the urls known the next publish links the two, then nothing is left to send.
    for id in ["601", "602"] {
        env.respond(
            &[
                "api",
                "--method",
                "PATCH",
                &format!("repos/me/repo/issues/comments/{id}"),
                "--input",
                "-",
            ],
            &format!(
                r#"{{"id":{id},"html_url":"https://github.com/me/repo/pull/42#issuecomment-{id}"}}"#
            ),
        );
    }
    let again = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&again, "posted"), ["summary", "summary"], "{again}");
    let record = env.run(&["status", "--full"])["runs"][0]["record"].clone();
    let summary = record["sent"].as_str().unwrap();
    assert!(
        summary
            .contains("continue in [part 2](https://github.com/me/repo/pull/42#issuecomment-602)."),
        "{summary}"
    );
    let part = record["parts"][0]["sent"].as_str().unwrap();
    assert!(
        part.starts_with(
            "Part 2 of the [agt summary](https://github.com/me/repo/pull/42#issuecomment-601)."
        ),
        "{part}"
    );
    let last = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&last, "posted"), Vec::<String>::new(), "{last}");
    assert_eq!(svc.count(env, Call::PostNote), 2);
}

// GitLab

/// GitLab shows no edits, so a summary whose edit failed still shows text this store sent:
/// it is not imported as a thread, and the edit goes out again.
#[test]
fn gitlab_a_failed_summary_edit_keeps_the_summary_trusted() {
    let svc = Gitlab::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    for finding in [setup.finding(1), setup.finding(2)] {
        env.run(&["resolve", &finding, "--stale"]);
    }
    svc.link(&setup);
    svc.accept(env, Call::PostNote, 6002);
    env.run(&["publish", "--pr", "3"]);
    let first = summary_sent(env);
    env.write("c.txt", "c\n");
    let head = env.commit_all("more");
    let mut remote = Remote::new().head(head.clone());
    remote.general("d-s", Comment::new(6002, "me", &first));
    svc.sync(&setup, &remote);
    let put = gitlab::write("PUT", &format!("{}/notes/6002", gitlab::MR));
    let put = strs(&put);
    env.fail_call_with(&put, "glab: 502 Bad Gateway (HTTP 502)");
    env.fail(&["publish", "--pr", "3"], None);
    env.run(&["sync", "--pr", "3"]);
    assert!(!ids(&env.run(&["list"])).contains(&"gl-n6002".to_owned()));

    // The fake host does not apply edits, so it lists the note again once it is edited.
    env.pass_call(&put);
    env.respond(&put, r#"{"id":6002}"#);
    svc.sync(&setup, &Remote::new().head(head.clone()));
    let published = env.run(&["publish", "--pr", "3"]);
    assert_eq!(kinds(&published, "posted"), ["summary"]);
    assert_eq!(
        svc.count(env, Call::PostNote),
        1,
        "edited, not posted again"
    );
    let mut remote = Remote::new().head(head);
    remote.general("d-s", Comment::new(6002, "me", &summary_sent(env)));
    svc.sync(&setup, &remote);
    let synced = env.run(&["sync", "--pr", "3"]);
    assert_eq!(synced["records"], 1, "{synced}");
    assert!(open_ids(&env.run(&["status"])).is_empty());
}

/// A run's own record comment from before the summary stays trusted once the summary holds
/// the run.
#[test]
fn gitlab_a_record_from_before_the_summary_stays_trusted() {
    let svc = Gitlab::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    for finding in [setup.finding(1), setup.finding(2)] {
        env.run(&["resolve", &finding, "--stale"]);
    }
    svc.link(&setup);
    // The record an earlier build posted as note 6001.
    let old = format!(
        "**Review {run}:** reviewed at {short}. 1 cycles, 2 found, 0 fixed, 0 open.\n\
         <!-- review-record v2 id={run} head={head} reviews-at-head=1 open=0 -->\n\
         <!-- agent-review id={run}/record attempt=at-old -->",
        run = setup.run,
        head = setup.head,
        short = &setup.head[..7]
    );
    let mut files = vec![env.state()];
    let file = std::iter::from_fn(|| {
        while let Some(path) = files.pop() {
            if path.is_dir() {
                files.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            } else if std::fs::read_to_string(&path).is_ok_and(|text| text.contains("\"runs\"")) {
                return Some(path);
            }
        }
        None
    })
    .next()
    .unwrap();
    let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    doc["runs"][0]["record"] = json!({
        "post": "posted", "attempt": "at-old", "sent": old, "head": setup.head,
        "remote_id": "6001", "url": null
    });
    std::fs::write(&file, doc.to_string()).unwrap();
    let mut remote = Remote::new();
    remote.general("d-r", Comment::new(6001, "me", &old));
    svc.sync(&setup, &remote);
    svc.accept(env, Call::PostNote, 6002);
    let published = env.run(&["publish", "--pr", "3"]);
    assert_eq!(kinds(&published, "posted"), ["summary"]);
    let synced = env.run(&["sync", "--pr", "3"]);
    assert_eq!(synced["records"], 1, "{synced}");
    assert!(!ids(&env.run(&["list"])).contains(&"gl-n6001".to_owned()));
}

/// A draft created without its id saved is published, not created again; a discussion
/// posted without its id saved is matched, not posted again.
#[test]
fn gitlab_crash_after_write_does_not_duplicate() {
    let svc = Gitlab::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let (f1, f2) = (setup.finding(1), setup.finding(2));
    svc.sync(&setup, &Remote::new());
    env.run(&["sync", "--pr", "3"]);
    env.run(&["run", "end", "--run", &setup.run]);
    svc.lose(env, Call::PostInline);
    env.fail(&["publish", "--pr", "3"], None);

    let mut remote = Remote::new();
    remote
        .drafts
        .push(json!({ "id": 77, "note": sent(env, &f1) }));
    svc.sync(&setup, &remote);
    env.respond(&strs(&svc.call(Call::PublishDraft)), "");
    svc.lose(env, Call::PostThread);
    env.fail(&["publish", "--pr", "3"], None);
    assert_eq!(svc.count(env, Call::PostInline), 1);
    assert_eq!(svc.count(env, Call::PublishDraft), 1);
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f2)["comments"][0]["post"], "posting");

    let mut remote = Remote::new();
    remote.thread(
        "d-f1",
        false,
        3,
        vec![Comment::new(5001, "me", "{{LAST_NOTE}}")],
    );
    remote.items.push(crate::support::forge::Item::General {
        id: "d-f2".to_owned(),
        discussion: true,
        comments: vec![Comment::new(5002, "me", &sent(env, &f2))],
    });
    svc.sync(&setup, &remote);
    svc.accept(env, Call::PostNote, 5020);
    let published = env.run(&["publish", "--pr", "3"]);
    assert_eq!(kinds(&published, "posted"), ["summary"]);
    assert_eq!(svc.count(env, Call::PostInline), 1);
    assert_eq!(svc.count(env, Call::PublishDraft), 1);
    assert_eq!(svc.count(env, Call::PostThread), 1);
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f1)["comments"][0]["remote"]["id"], "5001");
    assert_eq!(thread(&list, &f1)["comments"][0]["draft_id"], Value::Null);
    assert_eq!(thread(&list, &f2)["comments"][0]["remote"]["id"], "5002");
    assert_never_posted(env);
}

/// The host refusing a general comment of mine fails the publish; only file threads and the
/// fallbacks of findings are reported and left unposted.
#[test]
fn gitlab_a_refused_general_comment_fails_the_publish() {
    let svc = Gitlab::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    svc.link(&setup);
    env.run(&[
        "comment",
        "--body",
        "General note from me.",
        "--author",
        "me",
    ]);
    let list = env.run(&["list"]);
    let note = list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["comments"][0]["body"] == "General note from me.")
        .map(|thread| thread["id"].as_str().unwrap().to_owned())
        .unwrap();
    env.fail_call_with(
        &strs(&svc.call(Call::PostThread)),
        "glab: 403 Forbidden (HTTP 403)",
    );
    let failed = env.fail(&["publish", "--pr", "3", "--only", &note], None);
    assert!(failed.contains("403"), "{failed}");
}

#[test]
fn gitlab_reply_on_general_note_keeps_one_thread() {
    let svc = Gitlab::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let mut remote = Remote::new();
    remote.general("d-g", Comment::new(4002, "alice", "Alice general text."));
    svc.sync(&setup, &remote);
    env.run(&["sync", "--pr", "3"]);
    remote.add_reply("d-g", Comment::new(4003, "bob", "Bob reply text."));
    svc.sync(&setup, &remote);
    env.run(&["sync", "--pr", "3"]);
    let list = env.run(&["list"]);
    let same: Vec<String> = ids(&list)
        .into_iter()
        .filter(|id| id.starts_with("gl-n4002"))
        .collect();
    assert_eq!(same, ["gl-n4002"], "one thread per discussion");
    assert_eq!(
        thread(&list, "gl-n4002")["comments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn gitlab_ranges_post_with_line_codes() {
    let svc = Gitlab::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let started = env.run_stdin(
        &["pass", "start", "--kind", "full", "--reviewers", "-"],
        crate::support::scenario::PUBLISH.reviewers,
    );
    let run = started["run"].as_str().unwrap();
    env.run_stdin(
        &["pass", "end", "--run", run, "--pass", "c1p0", "--findings", "-", "--reviewer", "r1"],
        r#"[{"type":"nit","path":"a.txt","start_line":2,"line":3,"scenario":"Range in one hunk."}]"#,
    );
    svc.sync(&setup, &Remote::new());
    env.run(&["sync", "--pr", "3"]);
    env.run(&["run", "end", "--run", run]);
    let dry = env.run(&["publish", "--pr", "3", "--dry-run"]);
    svc.assert_range(&dry, 2, 3, "Range in one hunk.");
    let draft = dry["writes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|write| {
            write["input"]["note"]
                .as_str()
                .is_some_and(|note| note.contains("Range in one hunk."))
        })
        .unwrap_or_else(|| panic!("{dry}"));
    let range = &draft["input"]["position"]["line_range"];
    // sha1("a.txt"), then the old and new line counters.
    let code = range["end"]["line_code"].as_str().unwrap();
    assert_eq!(
        code, "cfc7b4885384957ae445bc14914d4588f607651c_4_3",
        "{dry}"
    );
}

/// A reviewed MR whose f1 is resolved as stale and that has been approved (`formal` or not),
/// synced and with its run ended; returns the setup and the approval's thread.
fn approved(svc: &Gitlab, kill_f2: bool, formal: bool) -> (Setup, String) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    env.run(&["resolve", &setup.finding(1), "--stale"]);
    if kill_f2 {
        env.run(&["finding", "kill", &setup.finding(2), "--reason", "x"]);
    }
    svc.sync(&setup, &Remote::new());
    env.run(&["sync", "--pr", "3"]);
    env.run(&["run", "end", "--run", &setup.run]);
    let id = if formal {
        env.approve("3", &["--formal"])
    } else {
        env.approve("3", &[])
    };
    (setup, id)
}

#[test]
fn gitlab_formal_approval_only_approves() {
    let svc = Gitlab::new();
    let (setup, id) = approved(&svc, false, true);
    let env = &setup.env;
    let dry = env.run(&["publish", "--pr", "3", "--dry-run"]);
    // The approve call alone: no note carries the approval (the thread is f2's).
    assert_eq!(writes_for(&dry, &id), ["approve"], "{dry}");
    let at = kinds(&dry, "writes")
        .iter()
        .position(|kind| kind == "approve")
        .unwrap_or_else(|| panic!("{dry}"));
    let approve = &dry["writes"][at];
    assert_eq!(approve["input"]["sha"], setup.head.as_str());
    assert!(
        approve["call"]
            .to_string()
            .contains("merge_requests/3/approve")
    );
}

#[test]
fn gitlab_a_refused_approve_posts_a_comment() {
    let svc = Gitlab::new();
    let (setup, id) = approved(&svc, false, true);
    let env = &setup.env;
    let text = thread(&env.run(&["list"]), &id)["comments"][0]["body"].clone();
    let approve = svc.call(Call::Approve);
    // GitLab refuses: already approved, or the author may not approve.
    env.fail_call_with(&strs(&approve), "glab: 403 Forbidden (HTTP 403)");
    svc.accept_nth(env, Call::PostNote, 1, 5021);
    svc.accept_nth(env, Call::PostNote, 2, 5020);
    env.respond(
        &strs(&svc.call(Call::PostThread)),
        r#"{"id":"d-1","notes":[{"id":5003}]}"#,
    );
    let out = env.run(&["publish", "--pr", "3"]);
    let posted = out["posted"].as_array().unwrap();
    let refused = posted
        .iter()
        .find(|item| item["kind"] == "approve")
        .unwrap_or_else(|| panic!("{out}"));
    assert!(refused["error"].is_string(), "{out}");
    assert_eq!(refused["fallback"], "comment", "{out}");
    // A comment carries the approval text instead, then the summary goes out.
    let comment = posted
        .iter()
        .find(|item| item["items"] == json!([format!("{id}/c1")]))
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(comment["remote_id"], "5021", "{out}");
    assert!(sent(env, &id).starts_with("Approved at"), "{out}");
    assert_eq!(svc.count(env, Call::PostNote), 2, "{out}");
    // Sync claims the comment and keeps the approval text as stored.
    let mut remote = Remote::new();
    remote.general("d-5021", Comment::new(5021, "me", &sent(env, &id)));
    svc.sync(&setup, &remote);
    env.run(&["sync", "--pr", "3"]);
    let list = env.run(&["list"]);
    let approval = &thread(&list, &id)["comments"][0];
    assert_eq!(approval["remote"]["id"], "5021", "{approval}");
    assert_eq!(approval["body"], text, "{approval}");
    svc.restore(env, Call::Approve);
    svc.accept(env, Call::Approve, 0);
    env.run(&["publish", "--pr", "3"]);
    assert_eq!(
        svc.count(env, Call::Approve),
        1,
        "a refusal is not retried by itself"
    );
    // `approve --formal` again retries the host's approval alone.
    env.approve("3", &["--formal"]);
    env.run(&["publish", "--pr", "3"]);
    assert_eq!(svc.count(env, Call::Approve), 2);
    assert_eq!(svc.count(env, Call::PostNote), 2);
    env.run(&["publish", "--pr", "3"]);
    assert_eq!(
        svc.count(env, Call::Approve),
        2,
        "an accepted approval is not sent again"
    );
}

/// A refusal kept while its comment did not post: the next publish posts the comment alone.
#[test]
fn gitlab_a_refused_approve_comment_is_retried() {
    let svc = Gitlab::new();
    let (setup, id) = approved(&svc, false, true);
    let env = &setup.env;
    env.fail_call_with(
        &strs(&svc.call(Call::Approve)),
        "glab: 403 Forbidden (HTTP 403)",
    );
    env.fail_call_with(
        &strs(&svc.call(Call::PostNote)),
        "glab: 503 Service Unavailable",
    );
    env.respond(
        &strs(&svc.call(Call::PostThread)),
        r#"{"id":"d-1","notes":[{"id":5003}]}"#,
    );
    env.fail(&["publish", "--pr", "3"], None);
    svc.restore(env, Call::PostNote);
    svc.accept(env, Call::PostNote, 5021);
    let dry = env.run(&["publish", "--pr", "3", "--dry-run"]);
    assert_eq!(writes_for(&dry, &format!("{id}/c1")), ["thread"], "{dry}");
    assert_eq!(writes_for(&dry, &id), Vec::<String>::new(), "{dry}");
    env.run(&["publish", "--pr", "3"]);
    assert_eq!(svc.count(env, Call::Approve), 1, "a refusal is not retried");
    assert!(
        thread(&env.run(&["list"]), &id)["comments"][0]["remote"]["id"] == "5021",
        "the comment posted"
    );
}

/// GitLab refuses an approve whose head moved (409): no comment approves the new head, then or
/// later, as for a refusal an older agt kept without a comment.
#[test]
fn gitlab_a_head_moved_refusal_posts_no_comment() {
    let svc = Gitlab::new();
    let (setup, id) = approved(&svc, false, true);
    let env = &setup.env;
    env.fail_call_with(
        &strs(&svc.call(Call::Approve)),
        "glab: 409 Conflict (HTTP 409)",
    );
    svc.accept(env, Call::PostNote, 5020);
    env.respond(
        &strs(&svc.call(Call::PostThread)),
        r#"{"id":"d-1","notes":[{"id":5003}]}"#,
    );
    let out = env.run(&["publish", "--pr", "3"]);
    let refused = out["posted"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "approve")
        .unwrap_or_else(|| panic!("{out}"));
    assert!(refused["error"].is_string(), "{out}");
    assert!(refused["fallback"].is_null(), "{out}");
    // Only the summary went out.
    assert_eq!(svc.count(env, Call::PostNote), 1, "{out}");
    let dry = env.run(&["publish", "--pr", "3", "--dry-run"]);
    assert_eq!(
        writes_for(&dry, &format!("{id}/c1")),
        Vec::<String>::new(),
        "{dry}"
    );
}

#[test]
fn gitlab_approve_is_retried_after_a_transient_failure() {
    let svc = Gitlab::new();
    let (setup, _) = approved(&svc, false, true);
    let env = &setup.env;
    env.fail_call_with(
        &strs(&svc.call(Call::Approve)),
        "glab: 503 Service Unavailable",
    );
    svc.accept(env, Call::PostNote, 5020);
    env.respond(
        &strs(&svc.call(Call::PostThread)),
        r#"{"id":"d-1","notes":[{"id":5003}]}"#,
    );
    let out = env.run(&["publish", "--pr", "3"]);
    let failed = out["posted"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "approve")
        .unwrap_or_else(|| panic!("{out}"));
    assert!(failed["error"].is_string(), "{out}");
    svc.restore(env, Call::Approve);
    svc.accept(env, Call::Approve, 0);
    env.run(&["publish", "--pr", "3"]);
    assert_eq!(
        svc.count(env, Call::Approve),
        2,
        "a failure that is not a refusal is retried by the next publish"
    );
    env.run(&["publish", "--pr", "3"]);
    assert_eq!(
        svc.count(env, Call::Approve),
        2,
        "an accepted approval is not sent again"
    );
    assert_eq!(
        svc.count(env, Call::PostNote),
        1,
        "the approval posts no note, the summary once"
    );
}

#[test]
fn gitlab_accepted_approve_is_not_resent_after_a_failed_summary() {
    let svc = Gitlab::new();
    let (setup, _) = approved(&svc, false, true);
    let env = &setup.env;
    svc.accept(env, Call::Approve, 0);
    env.respond(
        &strs(&svc.call(Call::PostThread)),
        r#"{"id":"d-1","notes":[{"id":5003}]}"#,
    );
    svc.lose(env, Call::PostNote);
    env.fail(&["publish", "--pr", "3"], None);
    svc.restore(env, Call::PostNote);
    svc.accept(env, Call::PostNote, 5020);
    env.run(&["publish", "--pr", "3"]);
    assert_eq!(svc.count(env, Call::Approve), 1);
}

#[test]
fn gitlab_finding_changed_by_a_fix_posts_with_its_location() {
    let svc = Gitlab::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f1 = setup.finding(1);
    svc.sync(&setup, &Remote::new());
    env.run(&["sync", "--pr", "3"]);
    env.write("a.txt", &numbered(10, &[(3, "THREE fixed")]));
    let fixed = env.commit_all("fix");
    env.respond(
        &strs(&gitlab::read(gitlab::MR)),
        &fixture(
            "gitlab/mr.json",
            &[
                ("HOST", "gitlab.com"),
                ("REPO", "me/proj"),
                ("HEAD", &fixed),
                ("BASE", &setup.base),
                ("AUTHOR", "me"),
            ],
        ),
    );
    env.run(&["sync", "--pr", "3"]);
    env.run(&["run", "end", "--run", &setup.run]);
    let dry = env.run(&["publish", "--pr", "3", "--dry-run"]);
    let write = dry["writes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|write| write["items"] == json!([format!("{f1}/c1")]))
        .unwrap_or_else(|| panic!("{dry}"));
    assert!(write["call"].to_string().contains("/discussions"), "{dry}");
    let text = write["input"]["body"].as_str().unwrap();
    assert!(text.starts_with("`a.txt:3`"), "{text}");
}

#[test]
fn a_pending_formal_approval_waits_for_only_to_name_it() {
    let svc = Gitlab::new();
    let (setup, id) = approved(&svc, true, false);
    let env = &setup.env;
    svc.accept_nth(env, Call::PostNote, 1, 5030);
    svc.accept_nth(env, Call::PostNote, 2, 5031);
    env.run(&["publish", "--pr", "3"]);
    env.approve("3", &["--formal"]);
    let other = env.run(&["comment", "--body", "Unrelated note."])["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(other, id);
    let approves = |dry: &Value| {
        dry["writes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|write| write["kind"] == "approve")
            .count()
    };
    let dry = env.run(&["publish", "--pr", "3", "--only", &other, "--dry-run"]);
    assert_eq!(approves(&dry), 0, "{dry}");
    let dry = env.run(&["publish", "--pr", "3", "--only", &id, "--dry-run"]);
    assert_eq!(approves(&dry), 1, "{dry}");
}
