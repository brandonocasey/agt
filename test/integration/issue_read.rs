//! `agt issue` reading: sync, the local reads, list lookups, link and private notes. Each scenario runs against
//! GitHub and GitLab.

use serde_json::{Value, json};

use crate::support::issue::{FIRST_SINCE, Github, Gitlab, IssueFake, Spec};
use crate::support::{Env, strs};

type Fake<'a> = &'a dyn IssueFake;

/// One `#[test]` per scenario and service.
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
    first_sync_imports_issues_comments_and_links,
    incremental_sync_lists_from_the_watermark_and_refetches_only_changes,
    sync_pages_of_a_listing_are_joined,
    reads_need_no_host,
    list_filters_by_state_label_assignee_and_author,
    show_has_the_documented_shape,
    status_counts_per_tracker,
    status_push_follows_the_current_mode,
    search_matches_title_body_comments_and_notes,
    find_by_pr_branch_and_title,
    link_and_note_stay_local,
    a_second_tracker_is_found_by_key_or_repo,
);

pub fn synced(fake: Fake, specs: &[Spec]) -> Env {
    let env = Env::new();
    env.write_config(&fake.config(false));
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

fn two_issues() -> Vec<Spec> {
    vec![
        Spec::new(1, "Login fails")
            .labels(&["bug"])
            .assignees(&["ann"])
            .comment(10, "bob", "Seen on 2.1")
            .comment(11, "ann", "Fix in progress")
            .pr(5, "fix/1-login"),
        Spec::new(2, "Old thing")
            .closed()
            .updated("2026-10-11T10:00:00Z"),
    ]
}

fn first_sync_imports_issues_comments_and_links(fake: Fake) {
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(&env, &two_issues());
    let report = env.issue(&["sync"]);
    let tracker = &report["trackers"][0];
    assert_eq!(tracker["tracker"], fake.tracker());
    assert_eq!(
        (
            tracker["listed"].as_u64(),
            tracker["new"].as_u64(),
            tracker["updated"].as_u64()
        ),
        (Some(2), Some(2), Some(0))
    );
    let doc = env.issue_doc(fake.tracker(), "1");
    assert_eq!(doc["key"], format!("{}#1", fake.repo()));
    assert_eq!(doc["fields"]["title"], "Login fails");
    assert_eq!(doc["fields"]["labels"], json!(["bug"]));
    assert_eq!(doc["fields"]["assignees"], json!(["ann"]));
    assert_eq!(doc["base"], doc["fields"]);
    let comments: Vec<&str> = doc["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|comment| comment["body"].as_str().unwrap())
        .collect();
    assert_eq!(comments, ["Seen on 2.1", "Fix in progress"]);
    assert_eq!(doc["comments"][0]["id"], "remote-10");
    assert_eq!(doc["links"]["prs"][0]["number"], 5);
    assert_eq!(doc["links"]["prs"][0]["branch"], "fix/1-login");
    assert_eq!(doc["links"]["prs"][0]["source"], "remote");
    let tracker_doc = env.issue_doc(fake.tracker(), "tracker");
    assert_eq!(tracker_doc["login"], "me");
    assert_eq!(tracker_doc["watermark"], "2026-10-11T10:00:00Z");
    // Reading and listing only: the sync wrote nothing to the host.
    assert!(env.forge_writes(fake).is_empty());
}

fn incremental_sync_lists_from_the_watermark_and_refetches_only_changes(fake: Fake) {
    let env = synced(fake, &two_issues());
    let before = env.forge_calls().len();
    let changed = Spec::new(2, "Old thing renamed")
        .closed()
        .updated("2026-10-12T10:00:00Z");
    let mut unchanged = two_issues().remove(0);
    unchanged.updated = "2026-10-10T10:00:00Z".to_owned();
    // The watermark is 2026-10-11T10:00:00Z; the listing starts five minutes before.
    fake.serve_since(&env, "2026-10-11T09%3A55%3A00Z", &[unchanged, changed]);
    let report = env.issue(&["sync"]);
    let tracker = &report["trackers"][0];
    assert_eq!(
        (
            tracker["new"].as_u64(),
            tracker["updated"].as_u64(),
            tracker["unchanged"].as_u64()
        ),
        (Some(0), Some(1), Some(1))
    );
    let calls = env.forge_calls();
    let fetched = |number| {
        calls[before..]
            .iter()
            .filter(|call| call.contains(&fake.issue_call(number).join(" ")))
            .count()
    };
    assert_eq!((fetched(1), fetched(2)), (0, 1));
    assert_eq!(
        env.issue_doc(fake.tracker(), "2")["fields"]["title"],
        "Old thing renamed"
    );
    assert_eq!(
        env.issue_doc(fake.tracker(), "tracker")["watermark"],
        "2026-10-12T10:00:00Z"
    );
    let first = fake.list_call(FIRST_SINCE).join(" ");
    assert_eq!(
        calls.iter().filter(|call| call.contains(&first)).count(),
        1,
        "only the first sync lists from the since_days window"
    );
}

fn sync_pages_of_a_listing_are_joined(fake: Fake) {
    // serve() sends everything after the first issue as a second page.
    let env = synced(
        fake,
        &[
            Spec::new(1, "One"),
            Spec::new(2, "Two"),
            Spec::new(3, "Three"),
        ],
    );
    assert_eq!(env.issue(&["list", "--state", "all"])["count"], 3);
}

fn reads_need_no_host(fake: Fake) {
    let env = synced(fake, &two_issues());
    for args in [
        vec!["list"],
        vec!["show", "1"],
        vec!["list", "--text", "login"],
        vec!["status"],
        vec!["list", "--pr", "5"],
    ] {
        env.issue_offline(&args);
    }
}

fn list_filters_by_state_label_assignee_and_author(fake: Fake) {
    let env = synced(fake, &two_issues());
    let key = |number: u64| format!("{}#{number}", fake.repo());
    assert_eq!(keys(&env.issue(&["list"])), [key(1)]);
    assert_eq!(keys(&env.issue(&["list", "--state", "closed"])), [key(2)]);
    assert_eq!(
        keys(&env.issue(&["list", "--state", "all"])),
        [key(1), key(2)]
    );
    assert_eq!(keys(&env.issue(&["list", "--label", "bug"])), [key(1)]);
    assert!(keys(&env.issue(&["list", "--label", "nope"])).is_empty());
    assert_eq!(keys(&env.issue(&["list", "--assignee", "ann"])), [key(1)]);
    assert_eq!(
        keys(&env.issue(&["list", "--state", "all", "--author", "alice"])),
        [key(1), key(2)]
    );
    assert!(keys(&env.issue(&["list", "--pending"])).is_empty());
    let summary = &env.issue(&["list"])["issues"][0];
    assert_eq!(summary["state"], "open");
    assert_eq!(summary["comments"], 2);
    assert_eq!(summary["pending"], false);
    assert!(summary["url"].as_str().unwrap().contains("/issues/1"));
}

fn show_has_the_documented_shape(fake: Fake) {
    let env = synced(fake, &two_issues());
    for reference in ["1", "#1", &format!("{}#1", fake.repo())] {
        let shown = env.issue(&["show", reference]);
        assert_eq!(shown["number"], 1);
        assert_eq!(shown["body"], "Body of Login fails");
        assert_eq!(shown["comments"].as_array().unwrap().len(), 2);
        assert_eq!(shown["comments"][1]["author"], "ann");
        assert_eq!(shown["comments"][1]["post"], "posted");
        assert_eq!(shown["links"]["prs"][0]["number"], 5);
        assert_eq!(shown["queued"]["create"], false);
        assert_eq!(shown["queued"]["comments"], json!([]));
        assert_eq!(shown["errors"], json!([]));
        assert_eq!(shown["conflicts"], json!([]));
    }
    let error = env.issue_fail(&["show", "99"]);
    assert!(error.starts_with("agt: error: no issue 99"), "{error}");
    assert!(
        env.issue_fail(&["show", "nope"])
            .contains("not an issue key")
    );
}

fn status_counts_per_tracker(fake: Fake) {
    let env = synced(fake, &two_issues());
    let status = env.issue(&["status"]);
    let tracker = &status["trackers"][0];
    assert_eq!(tracker["tracker"], fake.tracker());
    assert_eq!(
        (
            tracker["issues"].as_u64(),
            tracker["open"].as_u64(),
            tracker["closed"].as_u64()
        ),
        (Some(2), Some(1), Some(1))
    );
    assert_eq!(tracker["push"], false);
    assert_eq!(tracker["pending"], json!([]));
    assert_eq!(tracker["synced_at"], "2026-10-17T12:00:00Z");
}

fn status_push_follows_the_current_mode(fake: Fake) {
    // Synced while push was on, then turned off: status must not still say true.
    let env = Env::new();
    env.write_config(&fake.config(true));
    fake.serve(&env, &two_issues());
    env.issue(&["sync"]);
    assert_eq!(env.issue(&["status"])["trackers"][0]["push"], true);
    env.write_config(&fake.config(false));
    assert_eq!(env.issue(&["status"])["trackers"][0]["push"], false);
}

fn search_matches_title_body_comments_and_notes(fake: Fake) {
    let env = synced(fake, &two_issues());
    env.issue(&["comment", "2", "remember the rollout", "--private"]);
    let by = |text: &str| env.issue(&["list", "--text", text]);
    let found = by("LOGIN");
    assert_eq!(found["count"], 1);
    assert_eq!(found["issues"][0]["matched"], json!(["title", "body"]));
    assert_eq!(by("seen on")["issues"][0]["matched"], json!(["comments"]));
    assert_eq!(by("rollout")["issues"][0]["matched"], json!(["notes"]));
    assert_eq!(by("zzz")["count"], 0);
}

fn find_by_pr_branch_and_title(fake: Fake) {
    let env = synced(fake, &two_issues());
    let key = format!("{}#1", fake.repo());
    let found = env.issue(&["list", "--pr", "5"]);
    assert_eq!(keys(&found), std::slice::from_ref(&key));
    assert_eq!(found["issues"][0]["via"], json!(["link"]));
    assert!(keys(&env.issue(&["list", "--pr", "6"])).is_empty());
    let url = match fake.name() {
        "github" => "https://github.com/o/r/pull/5",
        _ => "https://gitlab.com/g/p/-/merge_requests/5",
    };
    assert_eq!(
        keys(&env.issue(&["list", "--pr", url])),
        std::slice::from_ref(&key)
    );
    assert!(keys(&env.issue(&["list", "--pr", &url.replace(fake.repo(), "x/y")])).is_empty());

    let by_pr_branch = env.issue(&["list", "--branch", "fix/1-login"]);
    assert_eq!(
        by_pr_branch["issues"][0]["via"],
        json!(["pr-branch", "branch-name"])
    );
    let by_name = env.issue(&["list", "--branch", "feature/2-cleanup"]);
    assert_eq!(keys(&by_name), [format!("{}#2", fake.repo())]);
    assert_eq!(by_name["issues"][0]["via"], json!(["branch-name"]));
    assert!(keys(&env.issue(&["list", "--branch", "main"])).is_empty());

    let titled = env.issue(&[
        "list",
        "--pr",
        "77",
        "--title",
        &format!("Fix #2 and {}#1", fake.repo()),
    ]);
    assert_eq!(titled["count"], 2);
    assert_eq!(titled["issues"][0]["via"], json!(["title"]));
    let other_repo = env.issue(&["list", "--pr", "77", "--title", "Fix other/repo#2"]);
    assert_eq!(other_repo["count"], 0);
}

fn link_and_note_stay_local(fake: Fake) {
    let env = synced(fake, &two_issues());
    let calls = env.forge_calls().len();
    env.issue(&["link", "2", "--pr", "9"]);
    env.issue(&["link", "2", "--branch", "fix/2-old"]);
    env.issue(&["link", "2", "--pr", "9"]);
    let note = env.issue(&["comment", "2", "private", "--private"]);
    assert_eq!(note["pending"], false, "a note and links are never sent");
    let doc = env.issue_doc(fake.tracker(), "2");
    assert_eq!(doc["links"]["prs"].as_array().unwrap().len(), 1);
    assert_eq!(doc["links"]["prs"][0]["source"], "local");
    assert_eq!(doc["links"]["branches"], json!(["fix/2-old"]));
    assert_eq!(doc["notes"][0]["text"], "private");
    assert_eq!(
        env.forge_calls().len(),
        calls,
        "link and note never call a host"
    );
    let found = env.issue(&["list", "--branch", "fix/2-old"]);
    assert_eq!(
        found["issues"][0]["via"],
        json!(["linked-branch", "branch-name"])
    );
    // Once the host reports the PR, the local link becomes the remote one, not a duplicate.
    let linked = Spec::new(2, "Old thing")
        .closed()
        .pr(9, "fix/2-old")
        .updated("2026-10-13T10:00:00Z");
    fake.serve_since(&env, "2026-10-11T09%3A55%3A00Z", &[linked]);
    env.issue(&["sync"]);
    let doc = env.issue_doc(fake.tracker(), "2");
    assert_eq!(doc["links"]["prs"].as_array().unwrap().len(), 1);
    assert_eq!(doc["links"]["prs"][0]["source"], "remote");
    assert_eq!(doc["notes"][0]["text"], "private");
}

fn a_second_tracker_is_found_by_key_or_repo(fake: Fake) {
    let env = synced(fake, &two_issues());
    let status = env.issue(&["--tracker", fake.repo(), "status"]);
    assert_eq!(status["trackers"].as_array().unwrap().len(), 1);
    let by_key = env.issue(&["--tracker", fake.tracker(), "list"]);
    assert_eq!(by_key["count"], 1);
    assert!(
        env.issue_fail(&["--tracker", "nope/nope", "list"])
            .contains("no synced tracker matches nope/nope")
    );
}

#[test]
fn github_pull_requests_are_not_issues() {
    let env = Env::new();
    env.write_config(&Github.config(false));
    let specs = [Spec::new(1, "Real"), Spec::new(2, "A PR").pull_request()];
    (&Github as Fake).serve(&env, &specs);
    let report = env.issue(&["sync"]);
    assert_eq!(report["trackers"][0]["listed"], 1);
    assert_eq!(env.issue(&["list", "--state", "all"])["count"], 1);
}

#[test]
fn involved_scope_lists_each_way_a_user_is_involved_and_dedupes() {
    for (fake, config, calls) in [
        (
            &Github as Fake,
            "[[issues.tracker]]\nforge = \"github\"\nhost = \"github.com\"\nrepo = \"o/r\"\n",
            vec!["&creator=me", "&assignee=me", "&mentioned=me"],
        ),
        (
            &Gitlab as Fake,
            "[[issues.tracker]]\nforge = \"gitlab\"\nhost = \"gitlab.com\"\nrepo = \"g/p\"\n",
            vec![],
        ),
    ] {
        let env = Env::new();
        env.write_config(config);
        let spec = Spec::new(1, "Shared");
        env.respond(
            &strs(&fake.user_call()),
            if fake.name() == "github" {
                r#"{"login":"me"}"#
            } else {
                r#"{"username":"me"}"#
            },
        );
        let listing = Value::Array(vec![fake.issue_json(&spec)]).to_string();
        let since = FIRST_SINCE;
        let base = fake.list_call(since);
        let path = base.last().unwrap().clone();
        let variants: Vec<String> = if fake.name() == "github" {
            calls
                .iter()
                .map(|filter| format!("{path}{filter}"))
                .collect()
        } else {
            ["created_by_me", "assigned_to_me"]
                .iter()
                .map(|scope| path.replace("scope=all", &format!("scope={scope}")))
                .collect()
        };
        for variant in &variants {
            let mut call = base.clone();
            *call.last_mut().unwrap() = variant.clone();
            env.respond(&strs(&call), &listing);
        }
        fake.serve_issue(&env, &spec);
        let report = env.issue(&["sync"]);
        assert_eq!(report["trackers"][0]["listed"], 1, "{}", fake.name());
        assert_eq!(report["trackers"][0]["new"], 1);
        let listed = env
            .forge_calls()
            .iter()
            .filter(|call| call.contains("/issues?"))
            .count();
        assert_eq!(listed, variants.len(), "{}", fake.name());
    }
}

#[test]
fn a_missing_config_or_unknown_tracker_is_an_error_naming_the_file() {
    let env = Env::new();
    let error = env.issue_fail(&["sync"]);
    assert!(error.contains("no tracker serves issues"), "{error}");
    assert!(error.contains("config.toml"), "{error}");
    for text in [
        "[[tracker]]\nforge = \"github\"\nhost = \"github.com\"\nrepo = \"o/r\"\nbogus = 1\n",
        // The old key loads through the mapping, so its mistakes are named too.
        "[[issues.tracker]]\nforge = \"github\"\nhost = \"github.com\"\nrepo = \"o/r\"\nbogus = 1\n",
    ] {
        env.write_config(text);
        let error = env.issue_fail(&["sync"]);
        assert!(
            error.contains("config.toml") && error.contains("bogus"),
            "{error}"
        );
    }
    assert_eq!(env.issue(&["list"])["count"], 0, "reading needs no config");
}

#[test]
fn a_newer_issue_document_is_refused_with_an_upgrade_hint() {
    let fake: Fake = &Github;
    let env = synced(fake, &[Spec::new(1, "One")]);
    let path = env.issue_state().join(fake.tracker()).join("1.json");
    let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    doc["version"] = json!(2);
    std::fs::write(&path, doc.to_string()).unwrap();
    let error = env.issue_fail(&["list"]);
    assert!(error.contains("upgrade agt"), "{error}");
}

#[test]
fn tracker_identity_collisions_preserve_documents_and_avoid_forge_calls() {
    for args in [vec!["sync"], vec!["push"], vec!["push", "--dry-run"]] {
        let env = Env::new();
        let config = Github.config(true).replace("o/r", "a-b/c");
        env.write_config(&config);
        env.issue(&["create", "--tracker", "a-b/c", "--title", "Keep me"]);
        let before = env.issue_files();
        env.write_config(
            &config
                .replace("a-b/c", "a/b-c")
                .replace("\"manual\"", "\"off\""),
        );
        let error = env.issue_fail(&args);
        assert!(
            error.contains("tracker identity collision"),
            "{args:?}: {error}"
        );
        assert!(
            error.contains("a-b/c") && error.contains("a/b-c"),
            "{error}"
        );
        assert!(error.contains("gh-github.com-a-b-c"), "{error}");
        assert_eq!(env.issue_files(), before, "{args:?}");
        assert!(env.forge_calls().is_empty(), "{args:?}");
    }
}

#[test]
fn configured_tracker_collisions_fail_before_creating_documents() {
    let env = Env::new();
    let config = Github.config(true).replace("o/r", "a-b/c");
    env.write_config(&format!("{}\n{}", config, config.replace("a-b/c", "a/b-c")));
    let error = env.issue_fail(&["sync"]);
    assert!(error.contains("tracker identity collision"), "{error}");
    assert!(env.issue_files().is_empty());
    assert!(env.forge_calls().is_empty());
}

#[test]
fn tracker_identity_accepts_case_changes_and_rejects_corrupt_empty_hosts() {
    let env = Env::new();
    env.write_config(&Github.config(false));
    env.issue(&["create", "--tracker", "o/r", "--title", "Keep me"]);
    env.write_config(
        &Github
            .config(false)
            .replace("github.com", "GITHUB.COM")
            .replace("o/r", "O/R"),
    );
    // Push is off for this tracker: it is skipped without a call, after its identity matched.
    env.issue(&["push"]);
    let path = env.issue_state().join("gh-github.com-o-r/tracker.json");
    let mut doc = env.issue_doc("gh-github.com-o-r", "tracker");
    doc["host"] = json!("");
    std::fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();
    let before = env.issue_files();
    for args in [vec!["sync"], vec!["push", "--dry-run"]] {
        assert!(env.issue_fail(&args).contains("tracker identity collision"));
        assert_eq!(env.issue_files(), before);
    }
    assert!(env.forge_calls().is_empty());
}
