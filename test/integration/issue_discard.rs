//! `agt issue discard`: drops one issue's queued changes locally and never calls a host.

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
