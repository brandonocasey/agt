use serde_json::json;
use std::fs;

use crate::support::Env;
use crate::support::issue::{Github, IssueFake, NOW, Spec};

fn seeded_cache() -> Env {
    let env = Env::new();
    let fake: &dyn IssueFake = &Github;
    env.git(&["remote", "add", "origin", "https://github.com/o/r.git"]);
    env.write_config(&fake.config(false));
    fake.serve(&env, &[Spec::new(1, "Seed")]);
    env.issue(&["sync"]);
    let template = env.issue_doc(fake.tracker(), "1");
    for number in 1..=100 {
        let mut doc = template.clone();
        doc["number"] = json!(number);
        doc["key"] = json!(format!("o/r#{number}"));
        fs::write(
            env.issue_state()
                .join(fake.tracker())
                .join(format!("{number}.json")),
            doc.to_string(),
        )
        .unwrap();
        fake.serve_issue(&env, &Spec::new(number, "Seed").children(&[200]));
    }
    fake.serve_since(&env, "2026-10-10T09%3A55%3A00Z", &[]);
    env.write_config(&format!(
        "{}\n[sync]\npull = \"auto\"\n",
        fake.config(false)
    ));
    env
}

#[test]
fn automatic_hierarchy_refresh_rotates_bounded_batches_and_explicit_sync_finishes() {
    let env = seeded_cache();
    let fake: &dyn IssueFake = &Github;
    let before = env.forge_calls().len();
    env.issue_at(NOW + 600, &["list"]);
    let calls = env.forge_calls().len() - before;
    assert!(calls <= 32, "automatic refresh made {calls} requests");
    let tracker = env.issue_doc(fake.tracker(), "tracker");
    assert_eq!(tracker["synced_at"], "2026-10-17T12:10:00Z");
    assert_eq!(tracker["watermark"], "2026-10-10T10:00:00Z");
    assert_eq!(
        env.issue_doc(fake.tracker(), "tracker")["hierarchy_cursor"],
        10
    );
    assert_eq!(
        env.issue_doc(fake.tracker(), "10")["sub_issues"],
        json!(["o/r#200"])
    );
    assert_eq!(env.issue_doc(fake.tracker(), "11")["sub_issues"], json!([]));
    for batch in 2..=10 {
        env.issue_at(NOW + batch * 600, &["list"]);
    }
    assert_eq!(
        env.issue_doc(fake.tracker(), "100")["sub_issues"],
        json!(["o/r#200"])
    );
    assert!(env.issue_doc(fake.tracker(), "tracker")["hierarchy_cursor"].is_null());
    env.issue_at(NOW + 6600, &["list"]);
    let report = env.issue(&["sync"]);
    assert_eq!(report["trackers"][0]["hierarchy_refreshed"], 100);
    assert!(env.issue_doc(fake.tracker(), "tracker")["hierarchy_cursor"].is_null());
}

#[test]
fn automatic_hierarchy_progress_survives_failure_and_skips_unavailable_records() {
    use crate::support::forge::argv::{gh_get, gh_paginate};
    use crate::support::strs;

    let env = seeded_cache();
    let fake: &dyn IssueFake = &Github;
    let missing = gh_paginate("repos/o/r/issues/3/sub_issues?per_page=100");
    let failed = gh_paginate("repos/o/r/issues/5/sub_issues?per_page=100");
    env.fail_call_with(&strs(&missing), "gh: Not Found (HTTP 404)");
    env.fail_call_with(&strs(&gh_get("repos/o/r/issues/3")), "gh: Gone (HTTP 410)");
    env.fail_call_with(&strs(&failed), "gh: unavailable (HTTP 503)");
    env.issue_at(NOW + 600, &["list"]);
    let tracker = env.issue_doc(fake.tracker(), "tracker");
    assert_eq!(tracker["hierarchy_cursor"], 4);
    assert_eq!(tracker["synced_at"], "2026-10-17T12:00:00Z");
    let first = gh_paginate("repos/o/r/issues/1/sub_issues?per_page=100");
    let first_count = fake.count(&env, &first);
    env.pass_call(&strs(&failed));
    env.issue_at(NOW + 1200, &["list"]);
    assert_eq!(
        env.issue_doc(fake.tracker(), "tracker")["hierarchy_cursor"],
        14
    );
    assert_eq!(fake.count(&env, &first), first_count);
    assert_eq!(fake.count(&env, &missing), 1);
    assert_eq!(
        env.issue_doc(fake.tracker(), "14")["sub_issues"],
        json!(["o/r#200"])
    );
}

#[test]
fn automatic_budget_rotates_unchanged_listed_and_unlisted_issues() {
    let env = seeded_cache();
    let fake: &dyn IssueFake = &Github;
    let listed: Vec<_> = (1..=12)
        .map(|number| Spec::new(number, "Seed").children(&[300]))
        .collect();
    fake.serve_since(&env, "2026-10-10T09%3A55%3A00Z", &listed);
    env.issue_at(NOW + 600, &["list"]);
    assert_eq!(
        env.issue_doc(fake.tracker(), "10")["sub_issues"],
        json!(["o/r#300"])
    );
    assert_eq!(env.issue_doc(fake.tracker(), "11")["sub_issues"], json!([]));
    assert_eq!(env.issue_doc(fake.tracker(), "23")["sub_issues"], json!([]));
    assert_eq!(
        env.issue_doc(fake.tracker(), "tracker")["hierarchy_cursor"],
        10
    );
    env.issue_at(NOW + 1200, &["list"]);
    assert_eq!(
        env.issue_doc(fake.tracker(), "tracker")["hierarchy_cursor"],
        20
    );
    assert_eq!(
        env.issue_doc(fake.tracker(), "12")["sub_issues"],
        json!(["o/r#300"])
    );
    assert_eq!(
        env.issue_doc(fake.tracker(), "20")["sub_issues"],
        json!(["o/r#200"])
    );
    assert_eq!(env.issue_doc(fake.tracker(), "21")["sub_issues"], json!([]));
}

#[test]
fn automatic_same_timestamp_overlap_stays_bounded_and_rotates() {
    let env = seeded_cache();
    let fake: &dyn IssueFake = &Github;
    let listed: Vec<_> = (1..=100)
        .map(|number| Spec::new(number, "Seed").children(&[300]))
        .collect();
    fake.serve_since(&env, "2026-10-10T09%3A55%3A00Z", &listed);
    for batch in 1..=10 {
        let before = env.forge_calls().len();
        env.issue_at(NOW + batch * 600, &["list"]);
        let calls = env.forge_calls().len() - before;
        assert!(calls <= 32, "batch {batch} made {calls} requests");
        let end = batch * 10;
        assert_eq!(
            env.issue_doc(fake.tracker(), &end.to_string())["sub_issues"],
            json!(["o/r#300"])
        );
        let cursor = env.issue_doc(fake.tracker(), "tracker")["hierarchy_cursor"].clone();
        assert_eq!(cursor, if batch == 10 { json!(null) } else { json!(end) });
        if batch < 10 {
            assert_eq!(
                env.issue_doc(fake.tracker(), &(end + 1).to_string())["sub_issues"],
                json!([])
            );
        }
    }
    let report = env.issue(&["sync"]);
    assert_eq!(report["trackers"][0]["unchanged"], 100);
    assert_eq!(report["trackers"][0]["hierarchy_refreshed"], 100);
}

#[test]
fn automatic_changed_and_new_listed_issues_merge_beyond_cached_budget() {
    let env = seeded_cache();
    let fake: &dyn IssueFake = &Github;
    let listed = [
        Spec::new(100, "Updated")
            .updated("2026-10-10T10:01:00Z")
            .children(&[300]),
        Spec::new(101, "New").children(&[300]),
    ];
    fake.serve_since(&env, "2026-10-10T09%3A55%3A00Z", &listed);
    let before = env.forge_calls().len();
    env.issue_at(NOW + 600, &["list"]);
    assert!(env.forge_calls().len() - before <= 46);
    assert_eq!(
        env.issue_doc(fake.tracker(), "tracker")["hierarchy_cursor"],
        10
    );
    for (number, title) in [(100, "Updated"), (101, "New")] {
        let issue = env.issue_doc(fake.tracker(), &number.to_string());
        assert_eq!(issue["fields"]["title"], title);
        assert_eq!(issue["fields"]["body"], format!("Body of {title}"));
        assert_eq!(issue["sub_issues"], json!(["o/r#300"]));
        assert_eq!(
            fake.count(
                &env,
                &crate::support::forge::argv::gh_paginate(&format!(
                    "repos/o/r/issues/{number}/sub_issues?per_page=100"
                ))
            ),
            1
        );
    }
}
