//! `agt review sync` and `agt issue sync`: always pull, then push only where `push` allows it;
//! review sync without `--pr` refreshes the reviews already stored and imports nothing new.

use crate::support::forge::assert_read_only;
use crate::support::forge::github::{self, Github};
use crate::support::forge::{Call, FakeService};
use crate::support::scenario::{Setup, kinds};

#[test]
fn review_sync_without_pr_never_imports_a_new_pr() {
    let setup = Setup::synced_branch(github::ORIGIN);
    github::sync_fixtures(&setup);
    let env = &setup.env;
    let before = env.run(&["status", "--full"])["target"].clone();
    let report = env.run(&["sync"]);
    assert_eq!(report["synced"].as_array().unwrap().len(), 0, "{report}");
    assert_eq!(env.run(&["status", "--full"])["target"], before);
    assert!(env.forge_calls().is_empty(), "{:?}", env.forge_calls());
    // `--pr` still starts the review.
    let imported = env.run(&["sync", "--pr", "42"]);
    assert_eq!(imported["target"], "gh-42");
    assert_eq!(env.run(&["status", "--full"])["target"], "gh-42");
}

#[test]
fn review_sync_refreshes_a_stored_review_and_keeps_the_push_local_by_default() {
    let setup = Setup::synced_branch(github::ORIGIN);
    github::sync_fixtures(&setup);
    let env = &setup.env;
    env.write_config("");
    env.run(&["sync", "--pr", "42"]);
    let before = env.forge_calls().len();
    let output = env.exec(&["sync"], None, &[]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("kept local"), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["synced"][0]["target"], "gh-42", "{report}");
    assert_eq!(report["push"]["kept_local"], true, "{report}");
    assert!(env.forge_calls().len() > before);
    assert_read_only(&Github::new(), env);
}

#[test]
fn review_sync_publishes_where_push_is_manual() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    env.write_config("[sync]\npush = \"off\"\n");
    svc.link(&setup);
    svc.accept(env, Call::PostNote, 604);
    env.write_config("[sync]\npush = \"manual\"\n");
    for finding in [setup.finding(1), setup.finding(2)] {
        env.run(&["resolve", &finding, "--stale"]);
    }
    let report = env.run(&["sync"]);
    assert_eq!(report["push"]["kept_local"], false, "{report}");
    assert_eq!(
        kinds(&report["push"]["published"][0], "posted"),
        ["summary"],
        "{report}"
    );
}

#[test]
fn review_sync_sends_nothing_with_push_off() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    env.write_config("[sync]\npush = \"off\"\n");
    svc.link(&setup);
    let output = env.exec(&["sync"], None, &[]);
    assert!(output.status.success());
    assert_read_only(&svc, env);
}
