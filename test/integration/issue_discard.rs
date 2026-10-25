//! `agt issue discard`: drops one issue's queued changes locally and never calls a host.

use std::fs;

use serde_json::{Value, json};

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
    discarding_a_local_create_removes_it_and_push_sends_nothing,
    discarding_queued_edits_keeps_the_remote_copy_and_private_note,
    unknown_key_and_nothing_queued_fail,
    another_issues_pending_changes_are_untouched,
    discard_is_refused_while_the_push_lock_is_held,
    a_stale_posting_create_can_be_discarded_and_is_reported,
    a_stale_posting_comment_can_be_discarded_and_is_reported,
    a_document_from_a_newer_agt_says_to_upgrade,
    an_issue_whose_only_queue_is_a_conflict_can_be_discarded,
);

fn pushable(fake: Fake, specs: &[Spec]) -> Env {
    let env = Env::new();
    env.write_config(&fake.config(true));
    fake.serve(&env, specs);
    env.issue(&["sync"]);
    env
}

fn keys(list: &Value) -> Vec<String> {
    list["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|issue| issue["key"].as_str().unwrap().to_owned())
        .collect()
}

fn discarding_a_local_create_removes_it_and_push_sends_nothing(fake: Fake) {
    let env = pushable(fake, &[Spec::new(1, "Login fails")]);
    let created = env.issue(&["create", "--title", "Declined", "--tracker", fake.tracker()]);
    let key = created["key"].as_str().unwrap().to_owned();
    assert!(key.starts_with("local-"), "{key}");
    env.issue(&["comment", &key, "queued"]);
    env.issue(&["comment", &key, "--private", "note"]);
    assert_eq!(
        keys(&env.issue(&["list", "--pending"])),
        std::slice::from_ref(&key)
    );
    let calls = env.forge_calls().len();
    let out = env.issue(&["discard", &key]);
    assert_eq!(env.forge_calls().len(), calls, "discard never calls a host");
    assert_eq!(out["key"], key.as_str());
    assert_eq!(
        out["discarded"],
        json!({"issue": true, "comments": 1, "notes": 1})
    );
    assert!(keys(&env.issue(&["list", "--pending"])).is_empty());
    let report = env.issue(&["push", "--dry-run"]);
    for tracker in report["trackers"].as_array().unwrap() {
        assert_eq!(tracker["writes"], json!([]), "{report}");
    }
    assert!(env.forge_writes(fake).is_empty());
    assert!(
        env.issue_fail(&["show", &key]).contains("no issue"),
        "the record is gone"
    );
}

fn discarding_queued_edits_keeps_the_remote_copy_and_private_note(fake: Fake) {
    let env = pushable(fake, &[Spec::new(1, "Login fails").labels(&["bug"])]);
    env.issue(&["comment", "1", "--private", "mine"]);
    env.issue(&["comment", "1", "queued"]);
    env.issue(&["edit", "1", "--title", "Changed", "--add-label", "p1"]);
    let calls = env.forge_calls().len();
    let out = env.issue(&["discard", "1"]);
    assert_eq!(out["discarded"]["issue"], false);
    assert_eq!(out["discarded"]["comments"], 1);
    assert_eq!(out["discarded"]["edits"], json!(["title", "labels"]));
    assert_eq!(env.forge_calls().len(), calls, "discard never calls a host");
    let shown = env.issue(&["show", "1"]);
    assert_eq!(shown["title"], "Login fails");
    assert_eq!(shown["labels"], json!(["bug"]));
    assert_eq!(shown["pending"], false);
    assert!(keys(&env.issue(&["list", "--pending"])).is_empty());
    let doc = env.issue_doc(fake.tracker(), "1");
    assert_eq!(doc["notes"].as_array().unwrap().len(), 1);
    assert_eq!(doc["notes"][0]["text"], "mine");
    assert_eq!(doc["comments"].as_array().unwrap().len(), 0);
    let report = env.issue(&["push", "--dry-run"]);
    assert_eq!(report["trackers"][0]["writes"], json!([]), "{report}");
}

fn unknown_key_and_nothing_queued_fail(fake: Fake) {
    let env = pushable(fake, &[Spec::new(1, "Login fails")]);
    let error = env.issue_fail(&["discard", "9"]);
    assert!(
        error.contains("agt: error:") && error.contains("no issue 9"),
        "{error}"
    );
    let error = env.issue_fail(&["discard", "1"]);
    assert!(
        error.contains("agt: error:") && error.contains("nothing is queued"),
        "{error}"
    );
    let error = env.issue_fail(&["discard", "local-nope"]);
    assert!(error.contains("no issue"), "{error}");
}

fn another_issues_pending_changes_are_untouched(fake: Fake) {
    let env = pushable(fake, &[Spec::new(1, "One"), Spec::new(2, "Two")]);
    env.issue(&["edit", "1", "--title", "One edited"]);
    env.issue(&["edit", "2", "--title", "Two edited"]);
    env.issue(&["comment", "2", "queued two"]);
    let other = env.issue(&["create", "--title", "Other", "--tracker", fake.tracker()]);
    let other_key = other["key"].as_str().unwrap().to_owned();
    env.issue(&["discard", "1"]);
    let pending = keys(&env.issue(&["list", "--pending", "--state", "all"]));
    assert_eq!(pending.len(), 2, "{pending:?}");
    assert!(pending.contains(&other_key));
    let two = env.issue_doc(fake.tracker(), "2");
    assert_eq!(two["fields"]["title"], "Two edited");
    assert_eq!(two["comments"].as_array().unwrap().len(), 1);
    let one = env.issue_doc(fake.tracker(), "1");
    assert_eq!(one["fields"]["title"], "One");
}

fn write_doc(env: &Env, fake: Fake, name: &str, change: impl FnOnce(&mut Value)) {
    let mut doc = env.issue_doc(fake.tracker(), name);
    change(&mut doc);
    let path = env
        .issue_state()
        .join(fake.tracker())
        .join(format!("{name}.json"));
    fs::write(path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
}

fn discard_is_refused_while_the_push_lock_is_held(fake: Fake) {
    let env = pushable(fake, &[Spec::new(1, "Login fails")]);
    env.issue(&["edit", "1", "--title", "Changed"]);
    let lock = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(
            env.issue_state()
                .join(fake.tracker())
                .join(".push.publish.lock"),
        )
        .unwrap();
    lock.lock().unwrap();
    let error = env.issue_fail(&["discard", "1"]);
    assert!(error.contains("another agt is pushing issues"), "{error}");
    assert_eq!(
        env.issue_doc(fake.tracker(), "1")["fields"]["title"],
        "Changed"
    );
    drop(lock);
    env.issue(&["discard", "1"]);
    assert_eq!(
        env.issue_doc(fake.tracker(), "1")["fields"]["title"],
        "Login fails"
    );
}

fn a_stale_posting_create_can_be_discarded_and_is_reported(fake: Fake) {
    let env = pushable(fake, &[Spec::new(1, "Login fails")]);
    let created = env.issue(&["create", "--title", "Sent?", "--tracker", fake.tracker()]);
    let key = created["key"].as_str().unwrap().to_owned();
    write_doc(&env, fake, &key, |doc| {
        doc["create"]["post"] = json!("posting");
        doc["create"]["attempt"] = json!("attempt-1");
    });
    let out = env.issue(&["discard", &key]);
    assert_eq!(out["discarded"]["issue"], true);
    assert_eq!(
        out["discarded"]["possibly_sent"],
        json!(["create"]),
        "{out}"
    );
    assert!(env.issue_fail(&["show", &key]).contains("no issue"));
}

fn a_stale_posting_comment_can_be_discarded_and_is_reported(fake: Fake) {
    let env = pushable(fake, &[Spec::new(1, "Login fails")]);
    env.issue(&["comment", "1", "maybe sent"]);
    let mut id = String::new();
    write_doc(&env, fake, "1", |doc| {
        let comment = &mut doc["comments"][0];
        comment["post"] = json!("posting");
        comment["attempt"] = json!("attempt-1");
        id = comment["id"].as_str().unwrap().to_owned();
    });
    let out = env.issue(&["discard", "1"]);
    assert_eq!(out["discarded"]["issue"], false);
    assert_eq!(out["discarded"]["comments"], 1);
    assert_eq!(out["discarded"]["possibly_sent"], json!([id]), "{out}");
    assert_eq!(
        env.issue_doc(fake.tracker(), "1")["comments"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert!(env.forge_writes(fake).is_empty());
}

fn a_document_from_a_newer_agt_says_to_upgrade(fake: Fake) {
    let env = pushable(fake, &[Spec::new(1, "Login fails")]);
    let created = env.issue(&["create", "--title", "Mine", "--tracker", fake.tracker()]);
    let key = created["key"].as_str().unwrap().to_owned();
    write_doc(&env, fake, &key, |doc| {
        doc["from_a_newer_agt"] = json!(true);
    });
    let error = env.issue_fail(&["discard", &key]);
    assert!(error.contains("newer agt"), "{error}");
    assert!(error.contains("upgrade agt"), "{error}");
    assert!(!error.contains("try again"), "{error}");
    assert!(env.issue_doc(fake.tracker(), &key)["from_a_newer_agt"] == json!(true));
}

fn an_issue_whose_only_queue_is_a_conflict_can_be_discarded(fake: Fake) {
    let env = pushable(fake, &[Spec::new(1, "Login fails").labels(&["bug"])]);
    env.issue(&["edit", "1", "--title", "Mine"]);
    let mut remote = Spec::new(1, "Theirs")
        .labels(&["bug"])
        .updated("2026-10-12T10:00:00Z");
    remote.body = "Body of Login fails".to_owned();
    fake.serve_issue(&env, &remote);
    env.issue(&["push"]);
    let stored = env.issue_doc(fake.tracker(), "1");
    assert_eq!(stored["conflicts"].as_array().unwrap().len(), 1);
    assert_eq!(env.issue(&["show", "1"])["pending"], false);
    let out = env.issue(&["discard", "1"]);
    assert_eq!(out["discarded"]["edits"], json!([]));
    assert_eq!(out["discarded"]["comments"], 0);
    let stored = env.issue_doc(fake.tracker(), "1");
    assert_eq!(stored["conflicts"], json!([]));
    assert_eq!(stored["fields"]["title"], "Theirs");
    assert!(
        env.issue_fail(&["discard", "1"])
            .contains("nothing is queued")
    );
}
