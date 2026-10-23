//! GitLab findings on a changed file outside the diff hunks: a diff discussion on the
//! unchanged line, a file discussion for the whole file, and a general thread when the host
//! refuses the position.

use serde_json::{Value, json};

use crate::support::forge::gitlab::{Gitlab, MR, write};
use crate::support::forge::{Call, FakeService, Remote};
use crate::support::scenario::{Review, Setup, kinds, sent, thread};
use crate::support::strs;

const OUTSIDE: Review = Review {
    reviewers: r#"[{"model":"m1","effort":"xhigh","harness":"codex"}]"#,
    findings: r#"[
  {"type":"nit","path":"a.txt","line":9,"scenario":"Outside the hunks."},
  {"type":"nit","path":"a.txt","scenario":"About the whole file."}
]"#,
};

const LINE_ONLY: Review = Review {
    reviewers: OUTSIDE.reviewers,
    findings: r#"[{"type":"nit","path":"a.txt","line":9,"scenario":"Outside the hunks."}]"#,
};

fn write_of<'a>(dry: &'a Value, finding: &str) -> &'a Value {
    dry["writes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|write| {
            write["items"] == json!([format!("{finding}/c1")]) && write["kind"] != "publish-draft"
        })
        .unwrap_or_else(|| panic!("{dry}"))
}

#[test]
fn an_unchanged_line_outside_the_hunks_posts_a_diff_discussion_with_both_line_numbers() {
    let svc = Gitlab::new();
    let setup = Setup::branch(svc.origin(), &OUTSIDE, false, false);
    let env = &setup.env;
    let (line, whole) = (setup.finding(1), setup.finding(2));
    svc.link(&setup);
    let dry = env.run(&["publish", "--pr", "3", "--dry-run"]);
    assert_eq!(dry["skipped"], json!([]), "{dry}");

    let draft = write_of(&dry, &line);
    assert_eq!(draft["kind"], "draft", "{dry}");
    let position = &draft["input"]["position"];
    assert_eq!(position["position_type"], "text", "{dry}");
    assert_eq!(position["new_path"], "a.txt", "{dry}");
    assert_eq!(position["old_path"], "a.txt", "{dry}");
    assert_eq!(position["new_line"], 9, "{dry}");
    assert_eq!(position["old_line"], 9, "{dry}");
    assert_eq!(position["head_sha"], setup.head.as_str(), "{dry}");
    assert_eq!(position["base_sha"], setup.base.as_str(), "{dry}");
    assert!(position["start_sha"].is_string(), "{dry}");

    let file = write_of(&dry, &whole);
    assert_eq!(file["kind"], "thread", "{dry}");
    assert!(file["call"].to_string().contains("/discussions"), "{dry}");
    let position = &file["input"]["position"];
    assert_eq!(position["position_type"], "file", "{dry}");
    assert_eq!(position["new_path"], "a.txt", "{dry}");
    assert_eq!(position["head_sha"], setup.head.as_str(), "{dry}");
    assert_eq!(position["base_sha"], setup.base.as_str(), "{dry}");
    let body = file["input"]["body"].as_str().unwrap();
    assert!(
        body.starts_with("**✏️ nit**\n\nAbout the whole file."),
        "{body}"
    );
}

#[test]
fn a_refused_position_falls_back_to_a_general_thread_that_names_the_line() {
    let svc = Gitlab::new();
    let setup = Setup::branch(svc.origin(), &LINE_ONLY, false, false);
    let env = &setup.env;
    let line = setup.finding(1);
    svc.link(&setup);
    env.fail_call_with(
        &strs(&svc.call(Call::PostInline)),
        "glab: 400 Bad Request (HTTP 400)",
    );
    env.respond(
        &strs(&svc.call(Call::PostThread)),
        r#"{"id":"d-1","notes":[{"id":5003}]}"#,
    );
    svc.accept(env, Call::PostNote, 5020);
    let out = env.run(&["publish", "--pr", "3"]);
    let refused = out["posted"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "draft")
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(refused["fallback"], "thread", "{out}");
    assert!(refused["error"].is_string(), "{out}");
    assert_eq!(svc.count(env, Call::PostThread), 1, "{out}");
    assert!(
        kinds(&out, "posted").contains(&"thread".to_owned()),
        "{out}"
    );

    let body = env
        .forge_inputs()
        .into_iter()
        .filter(|(args, _)| args.contains("/discussions"))
        .map(|(_, input)| serde_json::from_str::<Value>(&input).unwrap()["body"].clone())
        .next()
        .unwrap_or_else(|| panic!("{out}"));
    let body = body.as_str().unwrap();
    assert!(body.starts_with("`a.txt:9`"), "{body}");
    assert!(body.contains("Outside the hunks."), "{body}");
    let list = env.run(&["list"]);
    let comment = &thread(&list, &line)["comments"][0];
    assert_eq!(comment["post"], "posted", "{comment}");
}

#[test]
fn a_refused_draft_publish_drops_the_draft_and_posts_a_general_thread() {
    let svc = Gitlab::new();
    let setup = Setup::branch(svc.origin(), &LINE_ONLY, false, false);
    let env = &setup.env;
    let line = setup.finding(1);
    svc.link(&setup);
    svc.accept(env, Call::PostInline, 77);
    env.fail_call_with(
        &strs(&svc.call(Call::PublishDraft)),
        "glab: 400 Bad Request (HTTP 400)",
    );
    let delete = write("DELETE", &format!("{MR}/draft_notes/77"));
    env.respond(&strs(&delete), "");
    env.respond(
        &strs(&svc.call(Call::PostThread)),
        r#"{"id":"d-1","notes":[{"id":5003}]}"#,
    );
    svc.accept(env, Call::PostNote, 5020);
    let out = env.run(&["publish", "--pr", "3"]);
    let refused = out["posted"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "publish-draft")
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(refused["fallback"], "thread", "{out}");
    assert_eq!(svc.count(env, Call::PostThread), 1, "{out}");
    let deletes = env
        .forge_calls()
        .iter()
        .filter(|call| call.contains("DELETE"))
        .count();
    assert_eq!(deletes, 1, "{out}");
    let list = env.run(&["list"]);
    let comment = &thread(&list, &line)["comments"][0];
    assert_eq!(comment["post"], "posted", "{comment}");
    assert!(comment["draft_id"].is_null(), "{comment}");
}

#[test]
fn a_draft_left_by_an_earlier_attempt_falls_back_when_the_host_refuses_it() {
    let svc = Gitlab::new();
    let setup = Setup::branch(svc.origin(), &LINE_ONLY, false, false);
    let env = &setup.env;
    let line = setup.finding(1);
    svc.link(&setup);
    // The draft is created, but publishing it fails for a reason worth retrying.
    svc.accept(env, Call::PostInline, 77);
    env.fail_call_with(
        &strs(&svc.call(Call::PublishDraft)),
        "glab: 502 Bad Gateway (HTTP 502)",
    );
    env.fail(&["publish", "--pr", "3"], None);
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &line)["comments"][0]["draft_id"], "77");

    // The host still holds the draft.
    let mut remote = Remote::new();
    remote
        .drafts
        .push(json!({ "id": 77, "note": sent(env, &line) }));
    svc.sync(&setup, &remote);
    env.fail_call_with(
        &strs(&svc.call(Call::PublishDraft)),
        "glab: 400 Bad Request (HTTP 400)",
    );
    env.respond(&strs(&write("DELETE", &format!("{MR}/draft_notes/77"))), "");
    env.respond(
        &strs(&svc.call(Call::PostThread)),
        r#"{"id":"d-1","notes":[{"id":5003}]}"#,
    );
    svc.accept(env, Call::PostNote, 5020);
    let out = env.run(&["publish", "--pr", "3"]);
    let refused = out["posted"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"] == "publish-draft")
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(refused["fallback"], "thread", "{out}");
    assert_eq!(svc.count(env, Call::PostThread), 1, "{out}");
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &line)["comments"][0]["post"], "posted");
}
