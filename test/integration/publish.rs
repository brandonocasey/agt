//! Publish scenarios that belong to one service: its payload shapes, its crash recovery and
//! its approval calls. What holds for every service is in `matrix.rs`.

use serde_json::{Value, json};

use crate::support::forge::github::{self, Github};
use crate::support::forge::gitlab::{self, Gitlab};
use crate::support::forge::{Call, Comment, FakeService, Remote};
use crate::support::scenario::{
    PUBLISH, Setup, args, assert_never_posted, count_calls, ids, kinds, open_ids, sent, strs,
    thread,
};
use crate::support::{Env, fixture, numbered};

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
    let (f1, f2) = (setup.finding(1), setup.finding(2));
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
    assert_eq!(thread(&list, &f2)["comments"][0]["post"], "posting");

    remote.general("601", Comment::new(601, "me", &sent(env, &f2)));
    svc.sync(&setup, &remote);
    svc.restore(env, Call::PostNote);
    svc.accept(env, Call::PostNote, 604);
    let published = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&published, "posted"), ["summary"]);
    assert_eq!(svc.count(env, Call::PostInline), 1);
    assert_eq!(
        svc.count(env, Call::PostNote),
        2,
        "f2 once, the summary once"
    );
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f2)["comments"][0]["remote"]["id"], "601");
    assert_never_posted(env);
}

/// The summary is one comment: the next run's publish edits it in place, and when the comment
/// is gone from the PR the summary is posted anew.
#[test]
fn the_summary_is_edited_in_place_and_posted_anew_when_deleted() {
    // A run whose one pass finds nothing.
    let quiet_run = |env: &Env| {
        let run = env.run(&["run", "start"])["run"]
            .as_str()
            .unwrap()
            .to_owned();
        env.run_stdin(
            &[
                "pass",
                "start",
                "--run",
                &run,
                "--kind",
                "full",
                "--reviewers",
                "-",
            ],
            PUBLISH.reviewers,
        );
        env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
        env.run(&["run", "end", "--run", &run]);
        run
    };
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    for finding in [setup.finding(1), setup.finding(2)] {
        env.run(&["resolve", &finding, "--triage", "stale"]);
    }
    svc.link(&setup);
    svc.accept(env, Call::PostNote, 604);
    let published = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&published, "posted"), ["summary"]);
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
    assert!(edit.contains("last run "), "{edit}");
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

#[test]
fn concurrent_publish_posts_a_pending_comment_once() {
    use std::fs::{self, OpenOptions, Permissions};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
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
    fs::write(&wrapper, r#"#!/bin/sh
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
"#).unwrap();
    fs::set_permissions(&wrapper, Permissions::from_mode(0o700)).unwrap();
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
    let branch_key = env.run(&["run", "start"])["target"]
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
    env.run(&["resolve", &setup.finding(1), "--triage", "stale"]);
    env.run(&["finding", "kill", &setup.finding(2), "--reason", "x"]);
    let id = env.run(&["approve", "--formal"])["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut remote = Remote::by("alice");
    svc.sync(&setup, &remote);
    env.run(&["sync", "--pr", "42"]);
    env.run(&["run", "end", "--run", &setup.run]);
    // The review lands on GitHub, but its response is lost.
    svc.lose(env, Call::Approve);
    env.fail(&["publish", "--pr", "42", "--not-own", "--only", &id], None);
    remote
        .reviews
        .push(github::review(701, "me", "APPROVED", &sent(env, &id)));
    svc.sync(&setup, &remote);
    svc.restore(env, Call::Approve);
    let out = env.run(&["publish", "--pr", "42", "--not-own", "--only", &id]);
    assert_eq!(svc.count(env, Call::Approve), 1, "{out}");
    assert!(!out["skipped"].to_string().contains(&id), "{out}");
}

#[test]
fn own_github_upgrade_to_formal_is_reported() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    env.run(&["resolve", &setup.finding(1), "--triage", "stale"]);
    env.run(&["finding", "kill", &setup.finding(2), "--reason", "x"]);
    let id = env.run(&["approve"])["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    svc.link(&setup);
    let comments = svc.call(Call::PostNote);
    env.respond_nth(&strs(&comments), 1, r#"{"id":610,"html_url":"u"}"#);
    env.respond_nth(&strs(&comments), 2, r#"{"id":611,"html_url":"u"}"#);
    env.run(&["publish", "--pr", "42"]);
    env.run(&["approve", "--formal"]);
    let dry = env.run(&["publish", "--pr", "42", "--dry-run"]);
    assert!(
        dry["skipped"].to_string().contains(&format!("{id}/formal")),
        "{dry}"
    );
    assert_eq!(dry["writes"], json!([]), "{dry}");
}

// GitLab

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
        &["run", "start", "--kind", "full", "--reviewers", "-"],
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
    env.run(&["resolve", &setup.finding(1), "--triage", "stale"]);
    if kill_f2 {
        env.run(&["finding", "kill", &setup.finding(2), "--reason", "x"]);
    }
    let approval = if formal {
        env.run(&["approve", "--formal"])
    } else {
        env.run(&["approve"])
    };
    let id = approval["thread"]["id"].as_str().unwrap().to_owned();
    svc.sync(&setup, &Remote::new());
    env.run(&["sync", "--pr", "3"]);
    env.run(&["run", "end", "--run", &setup.run]);
    (setup, id)
}

#[test]
fn gitlab_formal_approval_approves_then_comments() {
    let svc = Gitlab::new();
    let (setup, _) = approved(&svc, false, true);
    let env = &setup.env;
    let dry = env.run(&["publish", "--pr", "3", "--dry-run"]);
    let kinds = kinds(&dry, "writes");
    let at = kinds
        .iter()
        .position(|kind| kind == "approve")
        .unwrap_or_else(|| panic!("{dry}"));
    assert_eq!(kinds[at + 1], "thread");
    let approve = &dry["writes"][at];
    assert_eq!(approve["input"]["sha"], setup.head.as_str());
    assert!(
        approve["call"]
            .to_string()
            .contains("merge_requests/3/approve")
    );
}

#[test]
fn gitlab_approval_is_a_note_and_a_refused_approve_blocks_nothing() {
    let svc = Gitlab::new();
    let (setup, _) = approved(&svc, false, true);
    let env = &setup.env;
    let approve = svc.call(Call::Approve);
    // GitLab refuses: already approved, or the author may not approve.
    env.fail_call_with(&strs(&approve), "glab: 403 Forbidden (HTTP 403)");
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
    // The approval comment and the final record both went out as notes.
    assert_eq!(svc.count(env, Call::PostNote), 2, "{out}");
    svc.restore(env, Call::Approve);
    svc.accept(env, Call::Approve, 0);
    env.run(&["publish", "--pr", "3"]);
    assert_eq!(
        svc.count(env, Call::Approve),
        1,
        "a refusal is not retried by itself"
    );
    // `approve --formal` again retries the host's approval alone, without a second note.
    env.run(&["approve", "--formal"]);
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
        2,
        "the approval comment posted once"
    );
}

#[test]
fn gitlab_accepted_approve_is_not_resent_after_a_failed_comment() {
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
    env.run(&["approve", "--formal"]);
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
