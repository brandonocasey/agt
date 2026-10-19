//! `agt issue` queued writes: what each command changes locally. Nothing here reaches a host;
//! `push` is in `issue_push`.

use serde_json::{Value, json};

use crate::issue_read::synced;
use crate::support::Env;
use crate::support::issue::{IssueFake, Spec};

type Fake<'a> = &'a dyn IssueFake;

macro_rules! matrix {
    ($($scenario:ident),* $(,)?) => {
        $(
            mod $scenario {
                #[test]
                fn github() {
                    super::$scenario(&crate::support::issue::Github);
                }

                #[test]
                fn gitlab() {
                    super::$scenario(&crate::support::issue::Gitlab);
                }
            }
        )*
    };
}

matrix!(
    comment_close_and_edit_queue_locally_and_show_as_queued,
    close_with_a_reason_only_where_the_service_takes_one,
    assignees_follow_the_service_capability,
    labels_add_and_remove_by_set,
    link_comment_queues_a_comment_and_a_local_link_once,
    create_queues_a_local_issue_that_takes_more_changes,
    queue_commands_reject_bad_input,
);

fn issue() -> Spec {
    Spec::new(1, "Login fails").labels(&["bug"])
}

fn doc(env: &Env, fake: Fake) -> Value {
    env.issue_doc(fake.tracker(), "1")
}

fn comment_close_and_edit_queue_locally_and_show_as_queued(fake: Fake) {
    let env = synced(fake, &[issue()]);
    let calls = env.forge_calls().len();
    let commented = env.issue(&["comment", "1", "Thanks for the report"]);
    assert_eq!(commented["pending"], true);
    env.issue_stdin(&["comment", "1", "-"], "From stdin\n");
    env.issue(&["edit", "1", "--state", "closed", "--title", "Login breaks"]);
    env.issue_stdin(&["edit", "1", "--body", "-"], "New body\n");
    let shown = env.issue(&["show", "1"]);
    assert_eq!(shown["state"], "closed");
    assert_eq!(shown["queued"]["title"], "Login breaks");
    assert_eq!(shown["queued"]["body"], "New body\n");
    assert_eq!(
        shown["queued"]["state"],
        json!({"state": "closed", "reason": if fake.name() == "github" { json!("completed") } else { Value::Null }})
    );
    assert_eq!(shown["queued"]["comments"], json!(["c1", "c2"]));
    assert_eq!(shown["comments"][0]["post"], "pending");
    assert_eq!(shown["comments"][1]["body"], "From stdin\n");
    // The synced copy is kept as the base the merge compares against.
    let stored = doc(&env, fake);
    assert_eq!(stored["base"]["title"], "Login fails");
    assert_eq!(stored["base"]["state"], "open");
    assert_eq!(
        env.issue(&["list", "--pending", "--state", "all"])["count"],
        1
    );
    assert_eq!(
        env.issue(&["status"])["trackers"][0]["pending"],
        json!([format!("{}#1", fake.repo())])
    );
    env.issue(&["edit", "1", "--state", "open"]);
    assert_eq!(env.issue(&["show", "1"])["queued"]["state"], Value::Null);
    assert_eq!(
        env.forge_calls().len(),
        calls,
        "queued writes never call a host"
    );
}

fn close_with_a_reason_only_where_the_service_takes_one(fake: Fake) {
    let env = synced(fake, &[issue()]);
    if fake.name() == "github" {
        let shown = env.issue(&["edit", "1", "--state", "closed", "--reason", "not-planned"]);
        assert_eq!(shown["reason"], "not_planned");
    } else {
        let error = env.issue_fail(&["edit", "1", "--state", "closed", "--reason", "completed"]);
        assert!(error.contains("takes no close reason"), "{error}");
        assert_eq!(
            env.issue(&["edit", "1", "--state", "closed"])["state"],
            "closed"
        );
    }
    // A close without a reason is `completed` on a service that has reasons.
    let env = synced(fake, &[issue()]);
    let reason = env.issue(&["edit", "1", "--state", "closed"])["reason"].clone();
    assert_eq!(
        reason,
        if fake.name() == "github" {
            json!("completed")
        } else {
            Value::Null
        }
    );
}

fn assignees_follow_the_service_capability(fake: Fake) {
    let env = synced(fake, &[issue().assignees(&["zed"])]);
    if fake.name() == "github" {
        let shown = env.issue(&[
            "edit",
            "1",
            "--add-assignee",
            "ann",
            "--add-assignee",
            "bob",
        ]);
        assert_eq!(shown["assignees"], json!(["ann", "bob", "zed"]));
        let shown = env.issue(&[
            "edit",
            "1",
            "--remove-assignee",
            "zed",
            "--remove-assignee",
            "ann",
        ]);
        assert_eq!(shown["assignees"], json!(["bob"]));
    } else {
        let error = env.issue_fail(&["edit", "1", "--add-assignee", "ann"]);
        assert!(error.contains("one assignee"), "{error}");
        assert_eq!(
            env.issue(&["edit", "1", "--remove-assignee", "zed"])["assignees"],
            json!([])
        );
        assert_eq!(
            env.issue(&["edit", "1", "--add-assignee", "ann"])["assignees"],
            json!(["ann"])
        );
        // Removals apply first, so one call swaps the single assignee.
        let swapped = env.issue(&[
            "edit",
            "1",
            "--remove-assignee",
            "ann",
            "--add-assignee",
            "bob",
        ]);
        assert_eq!(swapped["assignees"], json!(["bob"]));
    }
}

fn labels_add_and_remove_by_set(fake: Fake) {
    let env = synced(fake, &[issue()]);
    env.issue(&[
        "edit",
        "1",
        "--add-label",
        "triage",
        "--add-label",
        "bug",
        "--add-label",
        "p1",
    ]);
    assert_eq!(
        doc(&env, fake)["fields"]["labels"],
        json!(["bug", "p1", "triage"])
    );
    env.issue(&[
        "edit",
        "1",
        "--remove-label",
        "bug",
        "--remove-label",
        "absent",
    ]);
    let stored = doc(&env, fake);
    assert_eq!(stored["fields"]["labels"], json!(["p1", "triage"]));
    assert_eq!(stored["base"]["labels"], json!(["bug"]));
}

fn link_comment_queues_a_comment_and_a_local_link_once(fake: Fake) {
    let env = synced(fake, &[issue()]);
    let url = match fake.name() {
        "github" => "https://github.com/o/r/pull/7",
        _ => "https://gitlab.com/g/p/-/merge_requests/7",
    };
    // A PR linked first without `--comment` still gets its comment later.
    env.issue(&["link", "1", "--pr", "7"]);
    assert_eq!(doc(&env, fake)["comments"], json!([]));
    env.issue(&["link", "1", "--pr", "7", "--comment"]);
    env.issue(&["link", "1", "--pr", url, "--comment"]);
    let stored = doc(&env, fake);
    assert_eq!(stored["links"]["prs"].as_array().unwrap().len(), 1);
    assert_eq!(stored["links"]["prs"][0]["url"], url);
    assert_eq!(stored["links"]["prs"][0]["source"], "local");
    assert_eq!(stored["comments"].as_array().unwrap().len(), 1);
    assert_eq!(stored["comments"][0]["body"], format!("Linked: {url}"));
    let elsewhere = url
        .replace("github.com", "git.example.com")
        .replace("gitlab.com", "git.example.com");
    let error = env.issue_fail(&["link", "1", "--pr", &elsewhere, "--comment"]);
    assert!(error.contains("not on the tracker's host"), "{error}");
}

fn create_queues_a_local_issue_that_takes_more_changes(fake: Fake) {
    // Local behavior: the tracker is configured but never synced, and no read may call a host.
    let env = Env::new().offline();
    env.write_config(&fake.config(true));
    let created = env.issue(&[
        "create",
        "--title",
        "Brand new",
        "--body",
        "Text",
        "--label",
        "bug",
    ]);
    let key = created["key"].as_str().unwrap().to_owned();
    assert!(key.starts_with("local-"), "{key}");
    assert_eq!(created["number"], Value::Null);
    assert_eq!(created["pending"], true);
    env.issue(&["comment", &key, "First!"]);
    env.issue(&["edit", &key, "--add-label", "p1"]);
    env.issue(&["edit", &key, "--state", "closed"]);
    let shown = env.issue(&["show", &key]);
    assert_eq!(shown["queued"]["create"], true);
    assert_eq!(shown["labels"], json!(["bug", "p1"]));
    assert_eq!(shown["state"], "closed");
    assert_eq!(
        env.issue(&["list"])["count"],
        0,
        "closed, so not in the open list"
    );
    assert_eq!(env.issue(&["list", "--state", "all"])["count"], 1);
    assert_eq!(
        env.issue(&["status"])["trackers"][0]["pending"],
        json!([key])
    );
    assert!(env.forge_calls().is_empty());
}

fn queue_commands_reject_bad_input(fake: Fake) {
    let env = synced(fake, &[issue()]);
    assert!(
        env.issue_fail(&["comment", "1", "  "])
            .contains("comment must not be empty")
    );
    assert!(
        env.issue_fail(&["comment", "9", "x"])
            .contains("no issue 9")
    );
    assert!(
        env.issue_fail(&["edit", "1", "--title", ""])
            .contains("title must not be empty")
    );
    assert!(
        env.issue_fail(&["edit", "1"])
            .contains("edit needs a change")
    );
    assert!(
        env.issue_fail(&["edit", "1", "--state", "open", "--reason", "completed"])
            .contains("needs --state closed")
    );
    assert!(
        env.issue_fail(&["link", "1", "--branch", "b", "--comment"])
            .contains("--pr")
    );
    let long = "x".repeat(70_000);
    assert!(env.issue_fail(&["comment", "1", &long]).contains("exceeds"));
    let two = Env::new();
    two.write_config(&format!("{}\n{}", fake.config(false), other_tracker(fake)));
    assert_eq!(two.issue(&["create", "--title", "T"])["tracker"], "local");
    let error = two.issue_fail(&["create", "--tracker", "absent", "--title", "T"]);
    assert!(error.contains("no configured tracker matches"), "{error}");
}

fn other_tracker(fake: Fake) -> String {
    let own = fake.config(false);
    match fake.name() {
        "github" => own.replace("o/r", "o/other"),
        _ => own.replace("g/p", "g/other"),
    }
}

#[test]
fn a_local_issue_needs_no_configuration_or_origin_and_keeps_private_content() {
    let env = Env::new();
    let made = env.issue(&["create", "--title", "Local work"]);
    let key = made["key"].as_str().unwrap();
    assert_eq!(made["tracker"], "local");
    assert_eq!(env.issue_doc("local", "tracker")["forge"], Value::Null);
    env.issue(&["note", key, "Private note"]);
    env.issue(&["link", key, "--branch", "feat/local"]);
    env.issue(&["link", key, "--pr", "https://github.com/o/r/pull/7"]);
    let stored = env.issue_doc("local", key);
    assert_eq!(stored["notes"][0]["text"], "Private note");
    assert_eq!(stored["links"]["branches"], json!(["feat/local"]));
    assert_eq!(stored["comments"], json!([]));
    assert!(env.forge_calls().is_empty());
}

#[test]
fn offline_flags_deny_explicit_sync_and_push() {
    let env = Env::new();
    for args in [vec!["--offline", "sync"], vec!["push", "--offline"]] {
        assert!(env.issue_fail(&args).contains("offline mode forbids"));
    }
    let env = Env::new().offline();
    assert!(env.issue_fail(&["sync"]).contains("offline mode forbids"));
    assert!(env.issue_fail(&["push"]).contains("offline mode forbids"));
    assert!(env.forge_calls().is_empty());
}
