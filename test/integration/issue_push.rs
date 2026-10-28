//! `agt issue push`: the requests each queued write becomes, the three-way merge against the
//! host, and the crash-safe delivery of comments and creates.

use serde_json::{Value, json};

use crate::support::issue::{IssueFake, Spec, Update};
use crate::support::{Env, strs};

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
    a_dry_run_renders_every_write_and_sends_none,
    push_sends_each_write_once_and_a_second_push_sends_nothing,
    only_the_remote_changed_is_taken_and_the_local_change_is_sent,
    both_changed_a_scalar_remote_wins_and_force_sends_the_local_value,
    labels_merge_when_both_sides_change,
    a_lost_comment_response_is_claimed_by_its_marker_and_not_sent_again,
    a_refusal_keeps_the_comment_queued_with_its_error,
    reopening_after_a_pushed_close_is_no_conflict,
    a_transport_failure_leaves_the_comment_possibly_sent,
    a_created_issue_is_sent_then_known_by_its_number,
    a_lost_create_is_claimed_by_its_marker_on_sync,
    a_lost_create_is_claimed_by_its_marker_on_the_next_push,
    a_lost_create_is_not_sent_again_while_the_listing_may_lag,
    a_create_whose_answer_cannot_be_read_is_not_sent_again_while_the_listing_may_lag,
    a_killed_create_is_not_sent_again_despite_an_old_create_error,
    a_close_queued_without_a_reason_is_recorded_as_sent_and_reopens_without_conflict,
    push_refuses_a_tracker_with_push_false,
    push_refuses_without_forge_post,
    no_token_reaches_a_call_a_file_or_the_output,
);

fn issue() -> Spec {
    Spec::new(1, "Login fails").labels(&["bug"])
}

fn pushable(fake: Fake, specs: &[Spec]) -> Env {
    let env = Env::new();
    env.write_config(&fake.config(true));
    fake.serve(&env, specs);
    env.issue(&["sync"]);
    env
}

fn doc(env: &Env, fake: Fake, name: &str) -> Value {
    env.issue_doc(fake.tracker(), name)
}

fn writes(report: &Value, key: &str) -> Vec<Value> {
    report["trackers"][0][key].as_array().unwrap().clone()
}

fn kinds(writes: &[Value]) -> Vec<String> {
    writes
        .iter()
        .map(|write| write["kind"].as_str().unwrap().to_owned())
        .collect()
}

/// The text a comment or issue body request carries.
fn text(body: &Value) -> String {
    ["body", "description"]
        .iter()
        .find_map(|key| body[key].as_str())
        .unwrap()
        .to_owned()
}

fn queue_everything(env: &Env, fake: Fake) {
    fake.assignee_lookups(env, &["ann"]);
    env.issue(&["comment", "1", "Thanks"]);
    env.issue(&["edit", "1", "--state", "closed"]);
    env.issue(&["edit", "1", "--add-assignee", "ann"]);
    env.issue(&["edit", "1", "--add-label", "triage"]);
    env.issue(&["edit", "1", "--title", "New title"]);
    env.issue(&["link", "1", "--pr", "7", "--comment"]);
}

fn expected_update() -> Update {
    Update {
        title: Some("New title"),
        close: Some(true),
        labels: Some(vec!["bug", "triage"]),
        assignees: Some(vec!["ann"]),
        assignee_ids: Some(vec![11]),
        ..Update::default()
    }
}

fn a_dry_run_renders_every_write_and_sends_none(fake: Fake) {
    let env = pushable(fake, &[issue()]);
    queue_everything(&env, fake);
    let before = doc(&env, fake, "1");
    let report = env.issue(&["push", "--dry-run"]);
    assert_eq!(report["dry_run"], true);
    let planned = writes(&report, "writes");
    assert_eq!(kinds(&planned), ["update", "comment", "comment"]);
    assert_eq!(planned[0]["args"], json!(fake.update_call(1)));
    assert_eq!(planned[0]["body"], fake.update_body(&expected_update()));
    assert_eq!(planned[1]["args"], json!(fake.comment_call(1)));
    let first = text(&planned[1]["body"]);
    assert!(
        first.starts_with("Thanks\n\n<!-- agent-review id=c1 attempt=at-"),
        "{first}"
    );
    assert!(first.ends_with(" -->"), "{first}");
    let second = text(&planned[2]["body"]);
    let url = if fake.name() == "github" {
        "https://github.com/o/r/pull/7"
    } else {
        "https://gitlab.com/g/p/-/merge_requests/7"
    };
    assert!(
        second.starts_with(&format!(
            "Linked: {url}\n\n<!-- agent-review id=c2 attempt=at-"
        )),
        "{second}"
    );
    assert!(env.forge_writes(fake).is_empty(), "a dry run only reads");
    assert_eq!(doc(&env, fake, "1"), before, "a dry run changes no file");
}

fn push_sends_each_write_once_and_a_second_push_sends_nothing(fake: Fake) {
    let env = pushable(fake, &[issue()]);
    queue_everything(&env, fake);
    fake.accept_update(&env, 1);
    env.respond_nth(&strs(&fake.comment_call(1)), 1, &fake.comment_response(100));
    env.respond_nth(&strs(&fake.comment_call(1)), 2, &fake.comment_response(101));
    let report = env.issue(&["push"]);
    assert_eq!(report["dry_run"], false);
    assert_eq!(
        kinds(&writes(&report, "sent")),
        ["update", "comment", "comment"]
    );
    assert_eq!(writes(&report, "failed").len(), 0);
    assert_eq!(
        env.forge_writes(fake),
        [
            fake.update_call(1).join(" "),
            fake.comment_call(1).join(" "),
            fake.comment_call(1).join(" "),
        ]
    );
    assert_eq!(
        fake.sent(&env, &fake.update_call(1)),
        [fake.update_body(&expected_update())]
    );
    let comments = fake.sent(&env, &fake.comment_call(1));
    assert!(text(&comments[0]).starts_with("Thanks\n\n"));
    assert!(text(&comments[1]).starts_with("Linked: "));

    let stored = doc(&env, fake, "1");
    assert_eq!(stored["comments"][0]["post"], "posted");
    assert_eq!(stored["comments"][0]["remote"]["id"], "100");
    assert_eq!(stored["comments"][1]["remote"]["id"], "101");
    assert_eq!(stored["base"], stored["fields"]);
    assert_eq!(stored["errors"], json!([]));
    let shown = env.issue(&["show", "1"]);
    assert_eq!(shown["pending"], false);

    let calls = env.forge_calls().len();
    let again = env.issue(&["push"]);
    assert_eq!(writes(&again, "sent").len(), 0);
    assert_eq!(
        env.forge_calls().len(),
        calls,
        "nothing queued, so no call at all"
    );

    // The host now shows the changes; a sync finds the copy in step with it.
    let mut remote = Spec::new(1, "New title")
        .labels(&["bug", "triage"])
        .assignees(&["ann"])
        .closed()
        .updated("2026-10-18T10:00:00Z")
        .comment(100, "me", "Thanks")
        .comment(101, "me", "Linked");
    remote.body = "Body of Login fails".to_owned();
    fake.serve_since(&env, "2026-10-10T09%3A55%3A00Z", &[remote]);
    let report = env.issue(&["sync"]);
    assert_eq!(report["trackers"][0]["conflicts"], 0);
    let stored = doc(&env, fake, "1");
    assert_eq!(stored["comments"].as_array().unwrap().len(), 2);
    assert_eq!(stored["base"], stored["fields"]);
    assert_eq!(env.issue(&["show", "1"])["pending"], false);
}

fn login_body(fake: Fake) -> &'static str {
    if fake.name() == "github" {
        r#"{"login":"me"}"#
    } else {
        r#"{"username":"me"}"#
    }
}

fn only_the_remote_changed_is_taken_and_the_local_change_is_sent(fake: Fake) {
    let env = pushable(fake, &[issue()]);
    env.issue(&["edit", "1", "--add-label", "triage"]);
    let mut remote = Spec::new(1, "Login breaks")
        .labels(&["bug"])
        .updated("2026-10-12T10:00:00Z");
    remote.body = "Body of Login fails".to_owned();
    fake.serve_issue(&env, &remote);
    fake.accept_update(&env, 1);
    let report = env.issue(&["push"]);
    assert_eq!(kinds(&writes(&report, "sent")), ["update"]);
    assert!(writes(&report, "conflicts").is_empty());
    let update = Update {
        labels: Some(vec!["bug", "triage"]),
        ..Update::default()
    };
    assert_eq!(
        fake.sent(&env, &fake.update_call(1)),
        [fake.update_body(&update)]
    );
    assert_eq!(doc(&env, fake, "1")["fields"]["title"], "Login breaks");
}

fn both_changed_a_scalar_remote_wins_and_force_sends_the_local_value(fake: Fake) {
    let env = pushable(fake, &[issue()]);
    env.issue(&["edit", "1", "--title", "Mine"]);
    let mut remote = Spec::new(1, "Theirs")
        .labels(&["bug"])
        .updated("2026-10-12T10:00:00Z");
    remote.body = "Body of Login fails".to_owned();
    fake.serve_issue(&env, &remote);
    let report = env.issue(&["push"]);
    assert!(writes(&report, "sent").is_empty());
    assert_eq!(
        writes(&report, "conflicts"),
        [json!({"issue": format!("{}#1", fake.repo()), "fields": ["title"]})]
    );
    assert!(env.forge_writes(fake).is_empty(), "a conflict is not sent");
    let stored = doc(&env, fake, "1");
    assert_eq!(stored["fields"]["title"], "Theirs");
    assert_eq!(stored["conflicts"][0]["local"]["title"], "Mine");
    let status = env.issue(&["status"]);
    assert_eq!(
        status["trackers"][0]["conflicts"][0]["fields"],
        json!(["title"])
    );

    fake.accept_update(&env, 1);
    let report = env.issue(&["push", "--force"]);
    assert_eq!(kinds(&writes(&report, "sent")), ["update"]);
    let update = Update {
        title: Some("Mine"),
        ..Update::default()
    };
    assert_eq!(
        fake.sent(&env, &fake.update_call(1)),
        [fake.update_body(&update)]
    );
    let stored = doc(&env, fake, "1");
    assert_eq!(stored["conflicts"], json!([]));
    assert_eq!(stored["base"]["title"], "Mine");
}

fn labels_merge_when_both_sides_change(fake: Fake) {
    let env = pushable(fake, &[issue().labels(&["bug", "old"])]);
    env.issue(&["edit", "1", "--add-label", "triage"]);
    env.issue(&["edit", "1", "--remove-label", "old"]);
    let mut remote = Spec::new(1, "Login fails")
        .labels(&["bug", "old", "docs"])
        .updated("2026-10-12T10:00:00Z");
    remote.body = "Body of Login fails".to_owned();
    fake.serve_issue(&env, &remote);
    fake.accept_update(&env, 1);
    let report = env.issue(&["push"]);
    assert!(writes(&report, "conflicts").is_empty());
    let update = Update {
        labels: Some(vec!["bug", "docs", "triage"]),
        ..Update::default()
    };
    assert_eq!(
        fake.sent(&env, &fake.update_call(1)),
        [fake.update_body(&update)]
    );
}

fn a_lost_comment_response_is_claimed_by_its_marker_and_not_sent_again(fake: Fake) {
    let env = pushable(fake, &[issue()]);
    env.issue(&["comment", "1", "Thanks"]);
    env.fail_call(&strs(&fake.comment_call(1)));
    let report = env.issue(&["push"]);
    assert_eq!(writes(&report, "failed")[0]["what"], "c1");
    assert_eq!(doc(&env, fake, "1")["comments"][0]["post"], "posting");
    // The write did land: the host lists the comment with the text it was sent.
    env.pass_call(&strs(&fake.comment_call(1)));
    let landed = issue().comment(500, "me", "{{LAST_BODY}}");
    fake.serve_issue(&env, &landed);
    let report = env.issue(&["push"]);
    assert!(writes(&report, "sent").is_empty(), "{report}");
    assert_eq!(
        fake.count(&env, &fake.comment_call(1)),
        1,
        "no duplicate comment"
    );
    let stored = doc(&env, fake, "1");
    assert_eq!(stored["comments"].as_array().unwrap().len(), 1);
    assert_eq!(stored["comments"][0]["post"], "posted");
    assert_eq!(stored["comments"][0]["remote"]["id"], "500");
    assert_eq!(stored["errors"], json!([]));
}

fn a_refusal_keeps_the_comment_queued_with_its_error(fake: Fake) {
    let env = pushable(fake, &[issue()]);
    env.issue(&["comment", "1", "Thanks"]);
    let refusal = match fake.name() {
        "github" => "gh: Validation Failed (HTTP 422)",
        _ => "glab: 403 Forbidden (HTTP 403)",
    };
    env.fail_call_with(&strs(&fake.comment_call(1)), refusal);
    let report = env.issue(&["push"]);
    let failed = writes(&report, "failed");
    assert_eq!(
        (failed[0]["what"].as_str(), failed[0]["refused"].as_bool()),
        (Some("c1"), Some(true))
    );
    let stored = doc(&env, fake, "1");
    assert_eq!(stored["comments"][0]["post"], "pending");
    assert_eq!(stored["errors"][0]["refused"], true);
    assert!(
        stored["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("HTTP")
    );
    let status = env.issue(&["status"]);
    assert_eq!(status["trackers"][0]["errors"][0]["what"], "c1");
    assert_eq!(
        env.issue(&["show", "1"])["queued"]["comments"],
        json!(["c1"])
    );

    env.pass_call(&strs(&fake.comment_call(1)));
    fake.accept_comment(&env, 1, 100);
    let report = env.issue(&["push"]);
    assert_eq!(kinds(&writes(&report, "sent")), ["comment"]);
    let stored = doc(&env, fake, "1");
    assert_eq!(stored["errors"], json!([]));
    assert_eq!(stored["comments"][0]["post"], "posted");
}

fn a_transport_failure_leaves_the_comment_possibly_sent(fake: Fake) {
    let env = pushable(fake, &[issue()]);
    env.issue(&["comment", "1", "Thanks"]);
    env.fail_call(&strs(&fake.comment_call(1)));
    let report = env.issue(&["push"]);
    assert_eq!(writes(&report, "failed")[0]["refused"], false);
    let stored = doc(&env, fake, "1");
    assert_eq!(stored["comments"][0]["post"], "posting");
    assert_eq!(stored["errors"][0]["refused"], false);
    assert!(stored["comments"][0]["sent"].as_str().is_some());
}

fn created() -> Spec {
    let mut spec = Spec::new(5, "Brand new").labels(&["bug"]);
    spec.body = "Text".to_owned();
    spec.author = "me";
    spec
}

fn a_created_issue_is_sent_then_known_by_its_number(fake: Fake) {
    let env = Env::new();
    env.write_config(&fake.config(true));
    let made = env.issue(&[
        "create",
        "--tracker",
        fake.tracker(),
        "--title",
        "Brand new",
        "--body",
        "Text",
        "--label",
        "bug",
    ]);
    let key = made["key"].as_str().unwrap().to_owned();
    env.issue(&["comment", &key, "First!"]);
    env.respond(&strs(&fake.user_call()), login_body(fake));
    let dry = env.issue(&["push", "--dry-run"]);
    let planned = writes(&dry, "writes");
    assert_eq!(kinds(&planned), ["create"]);
    assert_eq!(planned[0]["args"], json!(fake.create_call()));
    let body = text(&planned[0]["body"]);
    assert!(
        body.starts_with(&format!("Text\n\n<!-- agent-review id={key} attempt=at-")),
        "{body}"
    );
    let mut expected = fake.create_body("Brand new", "", &["bug"], &[]);
    let field = if fake.name() == "github" {
        "body"
    } else {
        "description"
    };
    expected[field] = json!(body);
    assert_eq!(planned[0]["body"], expected);
    assert!(env.forge_writes(fake).is_empty());

    let spec = created();
    env.respond(
        &strs(&fake.create_call()),
        &fake.issue_json(&spec).to_string(),
    );
    env.respond(
        &strs(&fake.user_call()),
        if fake.name() == "github" {
            r#"{"login":"me"}"#
        } else {
            r#"{"username":"me"}"#
        },
    );
    fake.accept_comment(&env, 5, 700);
    let report = env.issue(&["push"]);
    assert_eq!(kinds(&writes(&report, "sent")), ["create", "comment"]);
    assert_eq!(
        env.forge_writes(fake),
        [fake.create_call().join(" "), fake.comment_call(5).join(" ")]
    );
    assert!(
        !env.issue_state()
            .join(fake.tracker())
            .join(format!("{key}.json"))
            .exists()
    );
    let stored = doc(&env, fake, "5");
    assert_eq!(stored["key"], format!("{}#5", fake.repo()));
    assert_eq!(stored["number"], 5);
    assert_eq!(stored["create"]["post"], "posted");
    assert_eq!(stored["comments"][0]["remote"]["id"], "700");
    assert_eq!(stored["base"], stored["fields"]);
    assert_eq!(
        env.issue(&["list"])["issues"][0]["key"],
        format!("{}#5", fake.repo())
    );
    let calls = env.forge_calls().len();
    assert!(writes(&env.issue(&["push"]), "sent").is_empty());
    assert_eq!(env.forge_calls().len(), calls);
}

fn lose_a_create(fake: Fake) -> (Env, String) {
    let env = Env::new();
    env.write_config(&fake.config(true));
    let made = env.issue(&[
        "create",
        "--tracker",
        fake.tracker(),
        "--title",
        "Brand new",
        "--body",
        "Text",
        "--label",
        "bug",
    ]);
    let key = made["key"].as_str().unwrap().to_owned();
    env.respond(
        &strs(&fake.user_call()),
        if fake.name() == "github" {
            r#"{"login":"me"}"#
        } else {
            r#"{"username":"me"}"#
        },
    );
    env.fail_call(&strs(&fake.create_call()));
    let report = env.issue(&["push"]);
    assert_eq!(writes(&report, "failed")[0]["what"], "create");
    assert_eq!(doc(&env, fake, &key)["create"]["post"], "posting");
    (env, key)
}

fn landed(fake: Fake) -> Spec {
    let mut spec = created();
    spec.body = fake.created_body_placeholder().to_owned();
    spec
}

fn a_lost_create_is_claimed_by_its_marker_on_sync(fake: Fake) {
    let (env, key) = lose_a_create(fake);
    fake.serve(&env, &[landed(fake)]);
    let report = env.issue(&["sync"]);
    assert_eq!(report["trackers"][0]["claimed"], 1);
    assert!(
        !env.issue_state()
            .join(fake.tracker())
            .join(format!("{key}.json"))
            .exists()
    );
    let stored = doc(&env, fake, "5");
    assert_eq!(stored["create"]["post"], "posted");
    assert_eq!(
        stored["fields"]["body"], "Text",
        "the marker is not part of the body"
    );
    assert_eq!(stored["base"], stored["fields"]);
    assert_eq!(fake.count(&env, &fake.create_call()), 1);
    assert!(writes(&env.issue(&["push"]), "sent").is_empty());
    assert_eq!(fake.count(&env, &fake.create_call()), 1, "no second issue");
}

fn a_lost_create_is_claimed_by_its_marker_on_the_next_push(fake: Fake) {
    let (env, _key) = lose_a_create(fake);
    // Listed from five minutes before the local issue was written.
    fake.serve_since(&env, "2026-10-17T11%3A55%3A00Z", &[landed(fake)]);
    env.pass_call(&strs(&fake.create_call()));
    let report = env.issue(&["push"]);
    assert_eq!(kinds(&writes(&report, "sent")), ["claim"]);
    assert_eq!(writes(&report, "sent")[0]["number"], 5);
    assert_eq!(fake.count(&env, &fake.create_call()), 1, "no second issue");
    assert_eq!(doc(&env, fake, "5")["create"]["post"], "posted");
}

fn push_refuses_a_tracker_with_push_false(fake: Fake) {
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(&env, &[issue()]);
    env.issue(&["sync"]);
    env.issue(&["comment", "1", "Thanks"]);
    let calls = env.forge_calls().len();
    let report = env.issue(&["push"]);
    assert_eq!(report["trackers"][0]["skipped"], "push = false");
    let error = env.issue_fail(&["push", "--tracker", fake.repo()]);
    assert!(error.contains("push = false"), "{error}");
    let error = env.issue_fail(&["push", "--dry-run", "--tracker", fake.repo()]);
    assert!(error.contains("push = false"), "{error}");
    assert_eq!(env.forge_calls().len(), calls);
    assert_eq!(doc(&env, fake, "1")["comments"][0]["post"], "pending");
}

fn push_refuses_without_forge_post(fake: Fake) {
    let env = pushable(fake, &[issue()]);
    env.issue(&["comment", "1", "Thanks"]);
    env.write_config(&format!("{}\n[forge]\npost = false\n", fake.config(true)));
    let calls = env.forge_calls().len();
    let error = env.issue_fail(&["push", "--tracker", fake.repo()]);
    assert!(error.contains("[forge] post = true"), "{error}");
    assert_eq!(env.forge_calls().len(), calls);
    // A preview may read the host, as with posting on, but sends nothing.
    let preview = env.issue(&["push", "--dry-run", "--tracker", fake.repo()]);
    assert_eq!(preview["dry_run"], true);
    assert_eq!(doc(&env, fake, "1")["comments"][0]["post"], "pending");
}

fn no_token_reaches_a_call_a_file_or_the_output(fake: Fake) {
    const TOKEN: &str = "tok-SECRET-9f3a77";
    let tokens = [
        ("GH_TOKEN", TOKEN),
        ("GITHUB_TOKEN", TOKEN),
        ("GITLAB_TOKEN", TOKEN),
    ];
    let env = Env::new();
    env.write_config(&fake.config(true));
    fake.serve(&env, &[issue()]);
    let mut seen = String::new();
    let mut run = |args: &[&str]| {
        let output = env.issue_exec(args, "", &tokens);
        seen.push_str(&String::from_utf8_lossy(&output.stdout));
        seen.push_str(&String::from_utf8_lossy(&output.stderr));
    };
    run(&["sync"]);
    run(&["comment", "1", "Thanks"]);
    run(&["push", "--dry-run"]);
    // Fails with an error text, which is stored and printed.
    env.fail_call_with(&strs(&fake.comment_call(1)), "tool failed (HTTP 422)");
    run(&["push"]);
    run(&["show", "1"]);
    run(&["status"]);
    seen.push_str(&env.forge_calls().join("\n"));
    seen.push_str(&env.issue_files());
    assert!(
        seen.contains("HTTP 422"),
        "the scenario must reach the failure path"
    );
    assert!(!seen.contains(TOKEN), "a token leaked");
}

fn reopening_after_a_pushed_close_is_no_conflict(fake: Fake) {
    let env = pushable(fake, &[issue()]);
    env.issue(&["edit", "1", "--state", "closed"]);
    fake.accept_update(&env, 1);
    env.issue(&["push"]);
    // The host now shows the issue closed, with the reason it chose.
    let mut remote = issue().closed().updated("2026-10-18T10:00:00Z");
    remote.reason = Some("completed");
    fake.serve_issue(&env, &remote);
    env.issue(&["edit", "1", "--state", "open"]);
    let report = env.issue(&["push", "--dry-run"]);
    assert!(writes(&report, "conflicts").is_empty(), "{report}");
    assert_eq!(kinds(&writes(&report, "writes")), ["update"]);
    assert_eq!(
        writes(&report, "writes")[0]["body"],
        fake.update_body(&Update {
            close: Some(false),
            ..Update::default()
        })
    );
}

fn a_lost_create_is_not_sent_again_while_the_listing_may_lag(fake: Fake) {
    let (env, key) = lose_a_create(fake);
    env.pass_call(&strs(&fake.create_call()));
    // The host lists nothing yet, as it may right after the create landed.
    fake.serve_since(&env, "2026-10-17T11%3A55%3A00Z", &[]);
    let report = env.issue(&["push"]);
    assert!(writes(&report, "sent").is_empty(), "{report}");
    assert_eq!(writes(&report, "deferred")[0]["what"], "create");
    assert_eq!(
        fake.count(&env, &fake.create_call()),
        1,
        "no second issue yet"
    );
    // Later the listing is trusted: still nothing, so the create is sent again.
    let spec = created();
    env.respond(
        &strs(&fake.create_call()),
        &fake.issue_json(&spec).to_string(),
    );
    let later = crate::support::issue::NOW + 300;
    let report = env.issue_at(later, &["push"]);
    assert_eq!(kinds(&writes(&report, "sent")), ["create"], "{report}");
    assert_eq!(fake.count(&env, &fake.create_call()), 2);
    assert!(doc(&env, fake, "5")["number"] == 5, "{key}");
}

/// A pushed create whose send is cut short by `respond`ing with `answer`, so the host may hold
/// the issue while the doc is left `posting`.
fn push_a_create_with_answer(fake: Fake, answer: &str) -> (Env, String) {
    let env = Env::new();
    env.write_config(&fake.config(true));
    let made = env.issue(&["create", "--title", "Brand new", "--body", "Text"]);
    let key = made["key"].as_str().unwrap().to_owned();
    env.respond(
        &strs(&fake.user_call()),
        if fake.name() == "github" {
            r#"{"login":"me"}"#
        } else {
            r#"{"username":"me"}"#
        },
    );
    env.respond(&strs(&fake.create_call()), answer);
    env.issue_exec(&["push"], "", &[]);
    assert_eq!(fake.count(&env, &fake.create_call()), 1);
    (env, key)
}

fn the_second_push_waits_then_sends(fake: Fake, env: &Env, key: &str) {
    // The host lists nothing yet.
    fake.serve_since(env, "2026-10-17T11%3A55%3A00Z", &[]);
    env.respond(
        &strs(&fake.create_call()),
        &fake.issue_json(&created()).to_string(),
    );
    let report = env.issue(&["push"]);
    assert!(writes(&report, "sent").is_empty(), "{report}");
    assert_eq!(writes(&report, "deferred")[0]["what"], "create");
    assert_eq!(fake.count(env, &fake.create_call()), 1, "no second issue");
    let later = crate::support::issue::NOW + 300;
    let report = env.issue_at(later, &["push"]);
    assert_eq!(kinds(&writes(&report, "sent")), ["create"], "{report}");
    assert_eq!(fake.count(env, &fake.create_call()), 2, "{key}");
}

fn a_create_whose_answer_cannot_be_read_is_not_sent_again_while_the_listing_may_lag(fake: Fake) {
    let (env, key) = push_a_create_with_answer(fake, "{}");
    let stored = doc(&env, fake, &key);
    assert_eq!(stored["create"]["post"], "posting");
    assert!(stored["create"]["began_at"].as_str().is_some(), "{stored}");
    the_second_push_waits_then_sends(fake, &env, &key);
}

fn a_killed_create_is_not_sent_again_despite_an_old_create_error(fake: Fake) {
    let (env, key) = push_a_create_with_answer(fake, "{}");
    // A process killed mid-call leaves the mark and no new error; an error from an earlier,
    // much older attempt must not shorten the wait.
    let path = env
        .issue_state()
        .join(fake.tracker())
        .join(format!("{key}.json"));
    let mut stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    stored["errors"] = json!([{
        "what": "create", "message": "old", "refused": false, "at": "2026-10-17T10:00:00Z"
    }]);
    std::fs::write(&path, stored.to_string()).unwrap();
    the_second_push_waits_then_sends(fake, &env, &key);
}

fn a_close_queued_without_a_reason_is_recorded_as_sent_and_reopens_without_conflict(fake: Fake) {
    let env = pushable(fake, &[issue()]);
    env.issue(&["edit", "1", "--state", "closed"]);
    // A document queued before closes carried a reason: drop it.
    let path = env.issue_state().join(fake.tracker()).join("1.json");
    let mut stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    stored["fields"]["reason"] = Value::Null;
    std::fs::write(&path, stored.to_string()).unwrap();
    fake.accept_update(&env, 1);
    env.issue(&["push"]);
    let after = doc(&env, fake, "1");
    let sent = &fake.sent(&env, &fake.update_call(1))[0];
    let reason = if fake.name() == "github" {
        assert_eq!(sent["state_reason"], "completed");
        Value::from("completed")
    } else {
        Value::Null
    };
    assert_eq!(after["base"]["reason"], reason, "{after}");
    assert_eq!(after["base"], after["fields"], "nothing left to send");
    // The host shows the close as sent; reopening is no conflict.
    let mut remote = issue().closed().updated("2026-10-18T10:00:00Z");
    remote.reason = Some("completed");
    fake.serve_issue(&env, &remote);
    env.issue(&["edit", "1", "--state", "open"]);
    let report = env.issue(&["push", "--dry-run"]);
    assert!(writes(&report, "conflicts").is_empty(), "{report}");
    assert_eq!(kinds(&writes(&report, "writes")), ["update"]);
}

#[test]
fn automatic_create_attaches_one_issue_and_returns_its_canonical_key() {
    let fake: Fake = &crate::support::issue::Github;
    let env = Env::new();
    env.git(&["remote", "add", "origin", "https://github.com/o/r.git"]);
    let unrelated = env.issue(&["create", "--title", "Keep local"]);
    let other = unrelated["key"].as_str().unwrap();
    env.issue(&["comment", other, "Unrelated pending comment"]);
    env.write_config("[sync]\nauto_push = true\n");
    env.respond(&strs(&fake.user_call()), login_body(fake));
    env.respond(
        &strs(&fake.create_call()),
        &fake.issue_json(&created()).to_string(),
    );
    let output = env.issue_exec(
        &[
            "create",
            "--title",
            "Brand new",
            "--body",
            "Text",
            "--label",
            "bug",
        ],
        "",
        &[],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let made: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        made["key"],
        "o/r#5",
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        env.issue_doc("local", other)["comments"][0]["post"],
        "pending"
    );
    assert_eq!(env.forge_writes(fake), [fake.create_call().join(" ")]);
    let calls = env.forge_calls().len();
    env.issue(&["comment", "5", "Private", "--private"]);
    env.issue(&["link", "5", "--branch", "feat/private"]);
    env.issue(&["link", "5", "--pr", "7"]);
    assert_eq!(env.forge_calls().len(), calls);
    assert_eq!(env.issue_doc(fake.tracker(), "5")["comments"], json!([]));
}

#[test]
fn automatic_failure_preserves_local_save_and_explicit_false_denies_network() {
    let fake: Fake = &crate::support::issue::Github;
    let env = Env::new();
    env.write_config(&format!(
        "{}\n[sync]\nauto_push = true\n",
        fake.config(false)
    ));
    env.git(&["remote", "add", "origin", "https://github.com/o/r.git"]);
    let made = env.issue(&["create", "--title", "Keep me"]);
    let key = made["key"].as_str().unwrap();
    assert_eq!(env.issue_doc("local", key)["fields"]["title"], "Keep me");
    assert!(env.forge_calls().is_empty());
    env.write_config("[sync]\nauto_push = true\n");
    env.respond(&strs(&fake.user_call()), login_body(fake));
    env.fail_call(&strs(&fake.create_call()));
    let edited = env.issue(&["edit", key, "--title", "Saved despite failure"]);
    assert_eq!(edited["title"], "Saved despite failure");
    let saved = env.issue_doc(fake.tracker(), key);
    assert_eq!(saved["fields"]["title"], "Saved despite failure");
    assert_eq!(saved["create"]["post"], "posting");
    env.issue(&["edit", key, "--title", "Retry stays safe"]);
    assert_eq!(fake.count(&env, &fake.create_call()), 1);
}

#[test]
fn manual_push_allows_omitted_permission_and_local_dry_run_writes_no_files() {
    let fake: Fake = &crate::support::issue::Github;
    let env = Env::new();
    env.git(&["remote", "add", "origin", "https://github.com/o/r.git"]);
    let made = env.issue(&[
        "create",
        "--title",
        "Brand new",
        "--body",
        "Text",
        "--label",
        "bug",
    ]);
    let key = made["key"].as_str().unwrap();
    env.respond(&strs(&fake.user_call()), login_body(fake));
    let before = env.issue_files();
    let dry = env.issue(&["push", "--dry-run"]);
    assert_eq!(env.issue_files(), before);
    assert_eq!(dry["trackers"][1]["writes"][0]["kind"], "create");
    assert!(env.forge_writes(fake).is_empty());
    env.respond(
        &strs(&fake.create_call()),
        &fake.issue_json(&created()).to_string(),
    );
    env.issue(&["push"]);
    assert_eq!(env.issue_doc(fake.tracker(), "5")["key"], "o/r#5");
    assert!(
        !env.issue_state()
            .join("local")
            .join(format!("{key}.json"))
            .exists()
    );
}

#[test]
fn an_automatic_create_returns_its_identity_when_a_followup_write_fails() {
    let fake: Fake = &crate::support::issue::Github;
    let env = Env::new();
    env.git(&["remote", "add", "origin", "https://github.com/o/r.git"]);
    env.write_config("[sync]\nauto_push = true\n");
    env.respond(&strs(&fake.user_call()), login_body(fake));
    env.respond(
        &strs(&fake.create_call()),
        &fake.issue_json(&created()).to_string(),
    );
    env.fail_call(&strs(&fake.update_call(5)));
    let made = env.issue(&["create", "--title", "Brand new", "--body", "Text"]);
    assert_eq!(made["key"], "o/r#5");
    let saved = env.issue_doc(fake.tracker(), "5");
    assert_eq!(saved["create"]["post"], "posted");
    assert_eq!(saved["errors"][0]["what"], "update");
}

#[test]
fn empty_local_tracker_does_not_require_an_attachment_destination() {
    let github: Fake = &crate::support::issue::Github;
    let gitlab: Fake = &crate::support::issue::Gitlab;
    let env = Env::new();
    let made = env.issue(&[
        "create",
        "--title",
        "Brand new",
        "--body",
        "Text",
        "--label",
        "bug",
    ]);
    let key = made["key"].as_str().unwrap();
    env.write_config(&github.config(true));
    env.respond(&strs(&github.user_call()), login_body(github));
    env.respond(
        &strs(&github.create_call()),
        &github.issue_json(&created()).to_string(),
    );
    env.issue(&["push"]);
    assert!(
        !env.issue_state()
            .join("local")
            .join(format!("{key}.json"))
            .exists()
    );
    env.write_config(&format!("{}\n{}", github.config(true), gitlab.config(true)));
    github.serve(&env, &[]);
    gitlab.serve(&env, &[]);
    let github_writes_before = env.forge_writes(github);
    let gitlab_writes_before = env.forge_writes(gitlab);
    let report = env.issue(&["push"]);
    assert_eq!(report["trackers"].as_array().unwrap().len(), 2);
    assert!(env.issue_state().join("local/tracker.json").exists());
    assert_eq!(env.forge_writes(github), github_writes_before);
    assert_eq!(env.forge_writes(gitlab), gitlab_writes_before);
    env.issue(&["create", "--title", "Still pending"]);
    assert!(
        env.issue_fail(&["push"])
            .contains("several issue destinations")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn an_edit_waiting_for_attachment_does_not_recreate_the_source() {
    waiting_issue_mutation_does_not_recreate_the_source(false);
}

#[cfg(target_os = "linux")]
#[test]
fn an_attachment_waiting_for_another_attachment_does_not_recreate_the_source() {
    waiting_issue_mutation_does_not_recreate_the_source(true);
}

#[cfg(target_os = "linux")]
fn waiting_issue_mutation_does_not_recreate_the_source(attach: bool) {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let env = Env::new();
    let made = env.issue(&["create", "--title", "Move me"]);
    let key = made["key"].as_str().unwrap();
    let source = env.issue_state().join("local");
    let target = env.issue_state().join("gh-github.com-o-r");
    env.write_config(&crate::support::issue::Github.config(true));
    std::fs::create_dir_all(&target).unwrap();
    let guard = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(source.join(format!(".{key}.lock")))
        .unwrap();
    guard.lock().unwrap();
    let destination_guard = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(target.join(format!(".{key}.lock")))
        .unwrap();
    destination_guard.lock().unwrap();
    let args = if attach {
        vec!["issue", "push"]
    } else {
        vec!["issue", "edit", key, "--title", "Edited"]
    };
    let mut child = env
        .bin(&env.repo)
        .args(args)
        .env("AGT_OFFLINE", if attach { "0" } else { "1" })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let locks = std::fs::read_to_string("/proc/locks").unwrap();
        if locks.lines().any(|line| {
            line.contains("->")
                && line
                    .split_whitespace()
                    .any(|word| word == child.id().to_string())
        }) {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("mutation did not wait for the source lock");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let path = source.join(format!("{key}.json"));
    let destination = target.join(format!("{key}.json"));
    std::fs::rename(&path, &destination).unwrap();
    drop(destination_guard);
    drop(guard);
    let output = child.wait_with_output().unwrap();
    assert!(!path.exists(), "waiting edit recreated the attached issue");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("retry"));
    let moved: Value = serde_json::from_slice(&std::fs::read(destination).unwrap()).unwrap();
    assert_eq!(moved["fields"]["title"], "Move me");
}

#[test]
fn automatic_push_rejects_colliding_policy_and_attachment_destinations() {
    for local in [false, true] {
        let fake: Fake = &crate::support::issue::Github;
        let env = Env::new();
        let config = fake.config(false).replace("o/r", "a-b/c");
        env.write_config(&config);
        let made = env.issue(&["create", "--tracker", "a-b/c", "--title", "Stored"]);
        let stored_key = made["key"].as_str().unwrap();
        let made_local = env.issue(&["create", "--title", "Local"]);
        let key = if local {
            made_local["key"].as_str().unwrap()
        } else {
            stored_key
        };
        let before = env.issue_doc("gh-github.com-a-b-c", "tracker");
        env.write_config(&format!(
            "{}\n[sync]\nauto_push = true\n",
            config
                .replace("a-b/c", "a/b-c")
                .replace("push = false", "push = true")
        ));
        env.git(&["remote", "add", "origin", "https://github.com/a/b-c.git"]);
        let output = env.issue_exec(&["edit", key, "--title", "Saved locally"], "", &[]);
        assert!(output.status.success());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("tracker identity collision"), "{error}");
        assert_eq!(env.issue_doc("gh-github.com-a-b-c", "tracker"), before);
        assert!(env.forge_calls().is_empty());
        let tracker = if local {
            "local"
        } else {
            "gh-github.com-a-b-c"
        };
        assert_eq!(
            env.issue_doc(tracker, key)["fields"]["title"],
            "Saved locally"
        );
    }
}
