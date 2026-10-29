use serde_json::{Value, json};

use crate::support::Env;
use crate::support::issue::{Github, Gitlab, IssueFake, Spec};

fn key(issue: &Value) -> &str {
    issue["key"].as_str().unwrap()
}

#[test]
fn local_create_reparent_remove_and_filter() {
    let env = Env::new();
    let parent = env.issue(&["create", "--title", "Parent ü ;"]);
    let child = env.issue(&["create", "--title", "Child", "--parent", key(&parent)]);
    let grandchild = env.issue(&["create", "--title", "Grandchild", "--parent", key(&child)]);
    assert_eq!(child["parent"], parent["key"]);
    assert_eq!(
        env.issue(&["show", key(&parent)])["sub_issues"],
        json!([key(&child)])
    );
    let children = env.issue(&["list", "--parent", key(&parent)]);
    assert_eq!(children["count"], 1);
    assert_eq!(children["issues"][0]["key"], child["key"]);
    env.issue(&["edit", key(&child), "--state", "closed"]);
    assert_eq!(env.issue(&["list", "--parent", key(&parent)])["count"], 1);
    assert_eq!(
        env.issue(&["list", "--parent", key(&parent), "--state", "open"])["count"],
        0
    );
    assert!(
        env.issue_fail(&["edit", key(&parent), "--parent", key(&grandchild)])
            .contains("own ancestor")
    );
    assert!(
        env.issue_fail(&["edit", key(&child), "--parent", key(&child)])
            .contains("own ancestor")
    );
    assert!(
        env.issue_fail(&["create", "--title", "Missing", "--parent", "local-missing"])
            .contains("no issue")
    );
    assert_eq!(env.issue(&["list", "--state", "all"])["count"], 3);
    assert!(
        env.issue_fail(&["discard", key(&parent)])
            .contains("with children")
    );
    env.issue(&["edit", key(&grandchild), "--parent", key(&parent)]);
    env.issue(&["edit", key(&child), "--remove-parent"]);
    assert_eq!(env.issue(&["show", key(&child)])["parent"], Value::Null);
    assert_eq!(
        env.issue(&["show", key(&parent)])["sub_issues"],
        json!([key(&grandchild)])
    );
    env.issue(&["discard", key(&grandchild)]);
    env.issue(&["discard", key(&parent)]);
    assert_eq!(env.issue(&["list", "--state", "all"])["count"], 1);
    assert!(env.forge_calls().is_empty());
}

fn pull_hierarchy(fake: &dyn IssueFake) {
    let env = Env::new();
    env.write_config(&fake.config(false));
    let parent = Spec::new(1, "Parent").children(&[2, 9]);
    let child = Spec::new(2, "Child").parent(1).closed();
    fake.serve(&env, &[parent.clone(), child.clone()]);
    env.issue(&["sync"]);
    assert_eq!(
        env.issue(&["show", "1"])["sub_issues"],
        json!([format!("{}#2", fake.repo()), format!("{}#9", fake.repo())])
    );
    assert_eq!(
        env.issue(&["show", "2"])["parent"],
        format!("{}#1", fake.repo())
    );
    assert_eq!(env.issue(&["list", "--parent", "1"])["count"], 1);
    let mut detached = child.clone();
    detached.parent = None;
    fake.serve_since(&env, "2026-10-10T09%3A55%3A00Z", &[parent, detached]);
    env.issue(&["sync"]);
    assert_eq!(
        env.issue(&["show", "2"])["parent"],
        Value::Null,
        "relationship changes pull even with unchanged updated_at"
    );
    assert_eq!(
        env.issue(&["show", "1"])["sub_issues"],
        json!([format!("{}#9", fake.repo())])
    );
    assert!(env.forge_writes(fake).is_empty());
}

#[test]
fn github_pulls_hierarchy() {
    pull_hierarchy(&Github);
}
#[test]
fn gitlab_pulls_hierarchy() {
    pull_hierarchy(&Gitlab);
}

#[test]
fn gitlab_requires_explicit_conversion_and_rejects_nested_tasks() {
    let env = Env::new();
    env.write_config(&Gitlab.config(false));
    (&Gitlab as &dyn IssueFake).serve(
        &env,
        &[
            Spec::new(1, "Parent"),
            Spec::new(2, "Ordinary issue"),
            Spec::new(3, "Task").parent(1),
        ],
    );
    env.issue(&["sync"]);
    let before = env.forge_calls().len();
    assert!(
        env.issue_fail(&["edit", "2", "--parent", "1"])
            .contains("convert the Issue to a Task on GitLab")
    );
    assert!(
        env.issue_fail(&["create", "--title", "Nested", "--parent", "3"])
            .contains("nested Tasks")
    );
    assert_eq!(env.forge_calls().len(), before);
    let child = env.issue(&["create", "--title", "Native Task", "--parent", "1"]);
    assert_eq!(child["tracker"], Gitlab.tracker());
    assert_eq!(child["parent"], "g/p#1");
    env.issue(&["edit", "3", "--remove-parent"]);
    assert_eq!(
        env.issue(&["show", "3"])["queued"]["parent"],
        json!({"key":null})
    );
    env.issue(&["discard", "3"]);
    assert_eq!(env.issue(&["show", "3"])["parent"], "g/p#1");
}

fn parent_call(
    fake: &dyn IssueFake,
    child: u64,
    parent: Option<u64>,
    previous: u64,
) -> Vec<String> {
    use crate::support::args;
    use crate::support::forge::argv::gh_write;
    if fake.name() == "github" {
        gh_write(
            if parent.is_some() { "POST" } else { "DELETE" },
            &format!(
                "repos/o/r/issues/{}/{}",
                parent.unwrap_or(previous),
                if parent.is_some() {
                    "sub_issues"
                } else {
                    "sub_issue"
                }
            ),
        )
    } else {
        args(&[
            "api",
            "graphql",
            "-f",
            &format!("child=gid://gitlab/WorkItem/{}", 1000 + child),
            if parent.is_some() { "-f" } else { "-F" },
            &parent.map_or_else(
                || "parent=null".to_owned(),
                |parent| format!("parent=gid://gitlab/WorkItem/{}", 1000 + parent),
            ),
            "-f",
            "query=IssueParent",
        ])
    }
}

fn accept_parent(env: &Env, fake: &dyn IssueFake, child: u64, parent: Option<u64>, previous: u64) {
    use crate::support::strs;
    env.respond(&strs(&parent_call(fake,child,parent,previous)), if fake.name() == "github" {"{}"} else {r#"{"data":{"workItemUpdate":{"errors":[],"workItem":{"id":"gid://gitlab/WorkItem/1002","iid":"2"}}}}"#});
}

fn reparent_and_remove(fake: &dyn IssueFake) {
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(
        &env,
        &[
            Spec::new(1, "Parent"),
            Spec::new(2, "Child").parent(1),
            Spec::new(3, "Other"),
        ],
    );
    env.issue(&["sync"]);
    let calls = env.forge_calls().len();
    env.issue(&["edit", "2", "--parent", "3"]);
    assert_eq!(env.forge_calls().len(), calls);
    assert_eq!(env.issue(&["list", "--pending"])["count"], 1);
    env.write_config(&fake.config(true));
    accept_parent(&env, fake, 2, Some(3), 1);
    let dry = env.issue(&["push", "--dry-run"]);
    assert_eq!(dry["trackers"][0]["writes"][0]["kind"], "parent");
    assert!(env.forge_writes(fake).is_empty());
    assert_eq!(
        env.issue(&["show", "2"])["queued"]["parent"],
        json!({"key":format!("{}#3",fake.repo())})
    );
    let report = env.issue(&["push"]);
    assert_eq!(report["trackers"][0]["failed"], json!([]), "{report}");
    assert_eq!(report["trackers"][0]["sent"][0]["kind"], "parent");
    assert_eq!(env.issue(&["show", "2"])["queued"]["parent"], Value::Null);
    fake.serve_issue(&env, &Spec::new(2, "Child").parent(3));
    let writes = env.forge_writes(fake).len();
    assert_eq!(env.issue(&["push"])["trackers"][0]["sent"], json!([]));
    assert_eq!(env.forge_writes(fake).len(), writes);
    env.issue(&["edit", "2", "--remove-parent"]);
    accept_parent(&env, fake, 2, None, 3);
    let report = env.issue(&["push"]);
    assert_eq!(report["trackers"][0]["failed"], json!([]), "{report}");
    assert_eq!(
        report["trackers"][0]["sent"][0]["kind"], "parent",
        "{report}"
    );
    assert_eq!(
        report["trackers"][0]["sent"][0]["parent"],
        Value::Null,
        "{report}"
    );
    assert_eq!(env.issue(&["show", "2"])["parent"], Value::Null);
    assert_eq!(env.issue(&["list", "--pending"])["count"], 0);
}

#[test]
fn github_reparents_and_removes() {
    reparent_and_remove(&Github);
}
#[test]
fn gitlab_reparents_and_removes() {
    reparent_and_remove(&Gitlab);
}

fn publish_local_tree(fake: &dyn IssueFake) {
    use crate::support::strs;
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(&env, &[]);
    let parent = env.issue(&["create", "--title", "Parent"]);
    let child = env.issue(&["create", "--title", "Child", "--parent", key(&parent)]);
    let unrelated = env.issue(&["create", "--title", "Unrelated"]);
    env.issue(&["discard", key(&unrelated)]);
    let mut parent_remote = Spec::new(1, "Parent");
    parent_remote.body = fake.created_body_placeholder().into();
    let mut child_remote = Spec::new(2, "Child");
    child_remote.task = fake.name() == "gitlab";
    child_remote.body = fake.created_body_placeholder().into();
    env.respond_nth(
        &strs(&fake.create_call()),
        1,
        &fake.issue_json(&parent_remote).to_string(),
    );
    env.respond_nth(
        &strs(&fake.create_call()),
        2,
        &fake.issue_json(&child_remote).to_string(),
    );
    fake.serve_issue(&env, &Spec::new(1, "Parent"));
    let mut task = Spec::new(2, "Child");
    task.task = fake.name() == "gitlab";
    fake.serve_issue(&env, &task);
    accept_parent(&env, fake, 2, Some(1), 1);
    env.write_config(&fake.config(true));
    let report = env.issue(&["push"]);
    let tracker = &report["trackers"][0];
    assert_eq!(tracker["failed"], json!([]), "{report}");
    assert_eq!(tracker["deferred"], json!([]), "{report}");
    let sent = tracker["sent"].as_array().unwrap();
    assert_eq!(sent.len(), 3, "{report}");
    assert_eq!(sent[0]["issue"], format!("{}#1", fake.repo()));
    assert_eq!(sent[1]["issue"], format!("{}#2", fake.repo()));
    assert_eq!(sent[2]["kind"], "parent");
    assert_eq!(
        env.issue(&["show", key(&child)])["parent"],
        format!("{}#1", fake.repo())
    );
    assert_eq!(
        env.issue(&["show", key(&parent)])["sub_issues"],
        json!([format!("{}#2", fake.repo())])
    );
    assert_eq!(env.issue(&["list", "--pending"])["count"], 0);
    if fake.name() == "gitlab" {
        let bodies = fake.sent(&env, &fake.create_call());
        assert_eq!(bodies[1]["issue_type"], "task");
        assert!(bodies[0].get("issue_type").is_none());
    }
}

#[test]
fn github_publishes_ancestors_before_children() {
    publish_local_tree(&Github);
}
#[test]
fn gitlab_creates_native_tasks_after_parent() {
    publish_local_tree(&Gitlab);
}

fn parent_conflict_and_force(fake: &dyn IssueFake) {
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(
        &env,
        &[
            Spec::new(1, "Parent"),
            Spec::new(2, "Child").parent(1),
            Spec::new(3, "Other"),
        ],
    );
    env.issue(&["sync"]);
    env.issue(&["edit", "2", "--remove-parent"]);
    fake.serve_since(
        &env,
        "2026-10-10T09%3A55%3A00Z",
        &[
            Spec::new(1, "Parent"),
            Spec::new(2, "Child").parent(3),
            Spec::new(3, "Other"),
        ],
    );
    env.issue(&["sync"]);
    let shown = env.issue(&["show", "2"]);
    assert_eq!(shown["parent"], format!("{}#3", fake.repo()));
    assert_eq!(shown["conflicts"][0]["field"], "parent");
    env.write_config(&fake.config(true));
    accept_parent(&env, fake, 2, None, 3);
    let report = env.issue(&["push", "--force"]);
    assert_eq!(report["trackers"][0]["failed"], json!([]), "{report}");
    assert_eq!(env.issue(&["show", "2"])["parent"], Value::Null);
    assert_eq!(env.issue(&["show", "2"])["conflicts"], json!([]));
}
#[test]
fn github_parent_conflict_requires_force() {
    parent_conflict_and_force(&Github);
}
#[test]
fn gitlab_parent_conflict_requires_force() {
    parent_conflict_and_force(&Gitlab);
}

fn failed_parent_update_is_reconciled(fake: &dyn IssueFake) {
    use crate::support::strs;
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(
        &env,
        &[
            Spec::new(1, "Parent"),
            Spec::new(2, "Child").parent(1),
            Spec::new(3, "Other"),
        ],
    );
    env.issue(&["sync"]);
    env.issue(&["edit", "2", "--parent", "3"]);
    env.write_config(&fake.config(true));
    let call = parent_call(fake, 2, Some(3), 1);
    env.fail_call(&strs(&call));
    let report = env.issue(&["push"]);
    assert_eq!(
        report["trackers"][0]["failed"][0]["what"], "parent",
        "{report}"
    );
    assert_eq!(
        env.issue(&["show", "2"])["queued"]["parent"],
        json!({"key":format!("{}#3",fake.repo())})
    );
    assert_eq!(fake.count(&env, &call), 1);
    fake.serve_issue(&env, &Spec::new(2, "Child").parent(3));
    let report = env.issue(&["push"]);
    assert_eq!(report["trackers"][0]["failed"], json!([]), "{report}");
    assert_eq!(
        fake.count(&env, &call),
        1,
        "lost response must not duplicate a landed update"
    );
    assert_eq!(env.issue(&["list", "--pending"])["count"], 0);
    assert_eq!(env.issue(&["show", "2"])["errors"], json!([]));
}
#[test]
fn github_reconciles_a_lost_parent_response() {
    failed_parent_update_is_reconciled(&Github);
}
#[test]
fn gitlab_reconciles_a_lost_parent_response() {
    failed_parent_update_is_reconciled(&Gitlab);
}

#[test]
fn parent_failure_keeps_children_queued() {
    use crate::support::strs;
    let fake: &dyn IssueFake = &Github;
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(&env, &[]);
    let parent = env.issue(&["create", "--title", "Parent"]);
    let child = env.issue(&["create", "--title", "Child", "--parent", key(&parent)]);
    env.fail_call_with(&strs(&fake.create_call()), "gh: Forbidden (HTTP 403)");
    env.write_config(&fake.config(true));
    let report = env.issue(&["push"]);
    assert_eq!(report["trackers"][0]["failed"].as_array().unwrap().len(), 1);
    assert_eq!(report["trackers"][0]["deferred"][0]["issue"], child["key"]);
    assert_eq!(fake.count(&env, &fake.create_call()), 1);
    assert_eq!(env.issue(&["list", "--pending"])["count"], 2);
}

#[test]
fn gitlab_mutation_errors_do_not_mark_the_parent_sent() {
    use crate::support::strs;
    let fake: &dyn IssueFake = &Gitlab;
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(
        &env,
        &[Spec::new(1, "Parent"), Spec::new(2, "Task").parent(1)],
    );
    env.issue(&["sync"]);
    env.issue(&["edit", "2", "--remove-parent"]);
    env.write_config(&fake.config(true));
    let call = parent_call(fake, 2, None, 1);
    env.respond(
        &strs(&call),
        r#"{"data":{"workItemUpdate":{"errors":["Permission denied"],"workItem":null}}}"#,
    );
    let report = env.issue(&["push"]);
    assert_eq!(report["trackers"][0]["sent"], json!([]));
    assert!(
        report["trackers"][0]["failed"][0]["message"]
            .as_str()
            .unwrap()
            .contains("Permission denied")
    );
    assert_eq!(
        env.issue(&["show", "2"])["queued"]["parent"],
        json!({"key":null})
    );
}

#[test]
fn automatic_push_attaches_only_the_changed_child_and_its_ancestors() {
    use crate::support::strs;
    let fake: &dyn IssueFake = &Github;
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(&env, &[]);
    let parent = env.issue(&["create", "--title", "Parent"]);
    let child = env.issue(&["create", "--title", "Child", "--parent", key(&parent)]);
    let sibling = env.issue(&["create", "--title", "Sibling", "--parent", key(&parent)]);
    let unrelated = env.issue(&["create", "--title", "Unrelated"]);
    for (n, title) in [(1, "Parent"), (2, "Child")] {
        let mut spec = Spec::new(n, title);
        spec.body = fake.created_body_placeholder().into();
        env.respond_nth(
            &strs(&fake.create_call()),
            n as usize,
            &fake.issue_json(&spec).to_string(),
        );
        fake.serve_issue(&env, &Spec::new(n, title));
    }
    accept_parent(&env, fake, 2, Some(1), 1);
    env.git(&["remote", "add", "origin", "https://github.com/o/r.git"]);
    env.write_config(&fake.config(true).replace("manual", "auto"));
    let changed = env.issue(&["edit", key(&child), "--body", "Updated"]);
    assert_eq!(
        changed["key"], "o/r#2",
        "automatic output must name the child"
    );
    assert_eq!(changed["parent"], "o/r#1");
    assert_eq!(fake.count(&env, &fake.create_call()), 2);
    assert_eq!(
        env.issue(&["show", key(&sibling)])["tracker"],
        fake.tracker()
    );
    assert!(env.issue(&["show", key(&sibling)])["number"].is_null());
    assert_eq!(env.issue(&["show", key(&sibling)])["parent"], "o/r#1");
    assert_eq!(
        env.issue(&["show", key(&parent)])["sub_issues"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    assert_eq!(env.issue(&["show", key(&unrelated)])["tracker"], "local");
    assert_eq!(env.issue(&["show", key(&parent)])["key"], "o/r#1");
}

fn refresh_outside_listing(fake: &dyn IssueFake) {
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(
        &env,
        &[Spec::new(1, "Parent"), Spec::new(2, "Child").parent(1)],
    );
    env.issue(&["sync"]);
    fake.serve_since(&env, "2026-10-10T09%3A55%3A00Z", &[]);
    let mut child = Spec::new(2, "Child");
    child.task = true;
    fake.serve_issue(&env, &child);
    let report = env.issue(&["sync"]);
    assert_eq!(report["trackers"][0]["listed"], 0);
    assert_eq!(report["trackers"][0]["hierarchy_refreshed"], 2);
    assert_eq!(env.issue(&["show", "2"])["parent"], Value::Null);
}
#[test]
fn github_refreshes_hierarchy_outside_the_changed_listing() {
    refresh_outside_listing(&Github);
}
#[test]
fn gitlab_refreshes_hierarchy_outside_the_changed_listing() {
    refresh_outside_listing(&Gitlab);
}

#[test]
fn legacy_documents_and_a_thousand_children() {
    use std::fs;
    let env = Env::new();
    let parent = env.issue(&["create", "--title", "Parent"]);
    let child = env.issue(&["create", "--title", "Template"]);
    let dir = env.issue_state().join("local");
    let mut doc = env.issue_doc("local", key(&child));
    doc.as_object_mut().unwrap().remove("local_key");
    doc.as_object_mut().unwrap().remove("task");
    doc.as_object_mut().unwrap().remove("sub_issues");
    doc["fields"].as_object_mut().unwrap().remove("parent");
    fs::write(dir.join(format!("{}.json", key(&child))), doc.to_string()).unwrap();
    env.issue(&["edit", key(&child), "--parent", key(&parent)]);
    assert_eq!(env.issue(&["show", key(&child)])["parent"], parent["key"]);
    for n in 1..1000 {
        let key = format!("local-scale-{n}");
        doc["key"] = json!(key);
        doc["fields"]["parent"] = parent["key"].clone();
        fs::write(dir.join(format!("{key}.json")), doc.to_string()).unwrap();
    }
    assert_eq!(
        env.issue(&["list", "--parent", key(&parent)])["count"],
        1000
    );
    assert_eq!(
        env.issue(&["show", key(&parent)])["sub_issues"]
            .as_array()
            .unwrap()
            .len(),
        1000
    );
}

#[test]
fn github_sub_issue_pages_and_permission_failures_preserve_local_state() {
    use crate::support::forge::argv::{gh_get, gh_paginate};
    use crate::support::strs;
    let fake: &dyn IssueFake = &Github;
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(&env, &[Spec::new(1, "Parent")]);
    let route = gh_paginate("repos/o/r/issues/1/sub_issues?per_page=100");
    env.respond(&strs(&route), "[{\"number\":2,\"html_url\":\"https://github.com/o/r/issues/2\"}]\n[{\"number\":3,\"html_url\":\"https://github.com/o/other/issues/3\"}]");
    env.issue(&["sync"]);
    assert_eq!(
        env.issue(&["show", "1"])["sub_issues"],
        json!(["o/other#3", "o/r#2"])
    );
    fake.serve_since(&env, "2026-10-10T09%3A55%3A00Z", &[Spec::new(1, "Parent")]);
    let before = env.issue_doc(fake.tracker(), "1");
    env.fail_call_with(
        &strs(&gh_get("repos/o/r/issues/1/parent")),
        "gh: Forbidden (HTTP 403)",
    );
    assert!(env.issue_fail(&["sync"]).contains("Forbidden"));
    assert_eq!(env.issue_doc(fake.tracker(), "1"), before);
}

#[test]
fn gitlab_paginates_children_and_rejects_repeated_cursors() {
    let fake: &dyn IssueFake = &Gitlab;
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(&env, &[Spec::new(1, "Parent")]);
    let page = |n: u64, next: bool| {
        json!({"data":{"project":{"workItems":{"nodes":[{
            "id":"gid://gitlab/WorkItem/1001","iid":"1","webUrl":"https://gitlab.com/g/p/-/issues/1",
            "workItemType":{"name":"Issue"},"widgets":[{"type":"HIERARCHY","parent":null,
            "children":{"nodes":[{"iid":n.to_string(),"project":{"fullPath":"g/p"},"webUrl":format!("https://gitlab.com/g/p/-/issues/{n}")}],
            "pageInfo":{"hasNextPage":next,"endCursor":"next"}}}]
        }]}}}})
    };
    let initial = &[
        "api",
        "graphql",
        "-f",
        "path=g/p",
        "-f",
        "iid=1",
        "-f",
        "query=IssueHierarchy",
    ];
    let next = &[
        "api",
        "graphql",
        "-f",
        "path=g/p",
        "-f",
        "iid=1",
        "-f",
        "after=next",
        "-f",
        "query=IssueHierarchy",
    ];
    env.respond(initial, &page(2, true).to_string());
    env.respond(next, &page(3, false).to_string());
    env.issue(&["sync"]);
    assert_eq!(
        env.issue(&["show", "1"])["sub_issues"],
        json!(["g/p#2", "g/p#3"])
    );
    fake.serve_since(&env, "2026-10-10T09%3A55%3A00Z", &[Spec::new(1, "Parent")]);
    env.respond(initial, &page(2, true).to_string());
    env.respond(next, &page(3, true).to_string());
    let before = env.issue_doc(fake.tracker(), "1");
    assert!(env.issue_fail(&["sync"]).contains("repeated a page cursor"));
    assert_eq!(env.issue_doc(fake.tracker(), "1"), before);
}

#[test]
fn gitlab_project_metadata_handles_work_item_urls_and_ignores_epic_parents() {
    let fake: &dyn IssueFake = &Gitlab;
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(&env, &[Spec::new(1, "Issue")]);
    let response = json!({"data":{"project":{"workItems":{"nodes":[{
        "id":"gid://gitlab/WorkItem/1001","iid":"1","webUrl":"https://gitlab.com/g/p/-/work_items/1",
        "workItemType":{"name":"Issue"},"widgets":[{"type":"HIERARCHY",
        "parent":{"iid":"4","project":null,"webUrl":"https://gitlab.com/groups/g/-/epics/4"},
        "children":{"nodes":[{"iid":"2","project":{"fullPath":"g/other"},"webUrl":"https://gitlab.com/g/other/-/work_items/2"}],
        "pageInfo":{"hasNextPage":false,"endCursor":null}}}]
    }]}}}});
    env.respond(
        &[
            "api",
            "graphql",
            "-f",
            "path=g/p",
            "-f",
            "iid=1",
            "-f",
            "query=IssueHierarchy",
        ],
        &response.to_string(),
    );
    env.issue(&["sync"]);
    let issue = env.issue(&["show", "1"]);
    assert_eq!(
        issue["parent"],
        Value::Null,
        "epics are outside the issue hierarchy"
    );
    assert_eq!(issue["sub_issues"], json!(["g/other#2"]));
}

#[test]
fn force_publishes_an_unpublished_conflicting_parent_first() {
    use crate::support::strs;
    let fake: &dyn IssueFake = &Github;
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(
        &env,
        &[
            Spec::new(1, "Parent"),
            Spec::new(2, "Child").parent(1),
            Spec::new(3, "Other"),
        ],
    );
    env.issue(&["sync"]);
    let parent = env.issue(&[
        "create",
        "--title",
        "New parent",
        "--tracker",
        fake.tracker(),
    ]);
    env.issue(&["edit", "2", "--parent", key(&parent)]);
    fake.serve_since(
        &env,
        "2026-10-10T09%3A55%3A00Z",
        &[
            Spec::new(1, "Parent"),
            Spec::new(2, "Child").parent(3),
            Spec::new(3, "Other"),
        ],
    );
    env.issue(&["sync"]);
    let mut new_parent = Spec::new(4, "New parent");
    new_parent.body = fake.created_body_placeholder().into();
    env.respond(
        &strs(&fake.create_call()),
        &fake.issue_json(&new_parent).to_string(),
    );
    fake.serve_issue(&env, &Spec::new(4, "New parent"));
    accept_parent(&env, fake, 2, Some(4), 3);
    env.write_config(&fake.config(true));
    let report = env.issue(&["push", "--force"]);
    assert_eq!(report["trackers"][0]["failed"], json!([]), "{report}");
    assert_eq!(
        report["trackers"][0]["sent"][0]["kind"], "create",
        "{report}"
    );
    assert_eq!(
        report["trackers"][0]["sent"][1]["kind"], "parent",
        "{report}"
    );
    assert_eq!(env.issue(&["show", "2"])["parent"], "o/r#4");
}

fn dry_run_refetch_preserves_queued_state(fake: &dyn IssueFake) {
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(&env, &[Spec::new(1, "Original")]);
    env.issue(&["sync"]);
    env.issue(&["edit", "1", "--title", "Local title"]);
    fake.serve_issue(
        &env,
        &Spec::new(1, "Remote title")
            .updated("2026-10-11T12:00:00Z")
            .comment(9, "bob", "New remote comment"),
    );
    env.write_config(&fake.config(true));
    let before = env.issue_doc(fake.tracker(), "1");
    let report = env.issue(&["push", "--dry-run"]);
    assert_eq!(
        report["trackers"][0]["conflicts"][0]["fields"],
        json!(["title"])
    );
    assert!(env.forge_writes(fake).is_empty());
    assert_eq!(
        env.issue_doc(fake.tracker(), "1"),
        before,
        "a dry-run refetch must not persist any merge"
    );
}
#[test]
fn github_dry_run_refetch_preserves_queued_state() {
    dry_run_refetch_preserves_queued_state(&Github);
}
#[test]
fn gitlab_dry_run_refetch_preserves_queued_state() {
    dry_run_refetch_preserves_queued_state(&Gitlab);
}

#[test]
fn interrupted_attachment_cannot_orphan_children_by_discarding_the_parent() {
    use std::fs;
    let fake: &dyn IssueFake = &Github;
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(&env, &[]);
    env.issue(&["sync"]);
    let parent = env.issue(&["create", "--title", "Parent"]);
    let child = env.issue(&["create", "--title", "Child", "--parent", key(&parent)]);
    let mut doc = env.issue_doc("local", key(&child));
    doc["tracker"] = json!(fake.tracker());
    let source = env
        .issue_state()
        .join("local")
        .join(format!("{}.json", key(&child)));
    let destination = env
        .issue_state()
        .join(fake.tracker())
        .join(format!("{}.json", key(&child)));
    fs::write(&source, doc.to_string()).unwrap();
    fs::rename(&source, &destination).unwrap();
    assert!(
        env.issue_fail(&["discard", key(&parent)])
            .contains("with children")
    );
    assert_eq!(env.issue(&["show", key(&parent)])["key"], parent["key"]);
    assert!(destination.exists());
}

fn an_explicit_parent_edit_replaces_a_saved_conflict(fake: &dyn IssueFake, remove: bool) {
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(
        &env,
        &[
            Spec::new(1, "Parent"),
            Spec::new(2, "Child").parent(1),
            Spec::new(3, "Remote parent"),
            Spec::new(4, "Chosen parent"),
        ],
    );
    env.issue(&["sync"]);
    env.issue(&["edit", "2", "--remove-parent"]);
    fake.serve_since(
        &env,
        "2026-10-10T09%3A55%3A00Z",
        &[
            Spec::new(1, "Parent"),
            Spec::new(2, "Child").parent(3),
            Spec::new(3, "Remote parent"),
            Spec::new(4, "Chosen parent"),
        ],
    );
    env.issue(&["sync"]);
    assert_eq!(env.issue(&["show", "2"])["conflicts"][0]["field"], "parent");
    env.issue(if remove {
        &["edit", "2", "--remove-parent"]
    } else {
        &["edit", "2", "--parent", "4"]
    });
    env.write_config(&fake.config(true));
    let chosen = if remove { None } else { Some(4) };
    accept_parent(&env, fake, 2, chosen, 3);
    let report = env.issue(&["push"]);
    assert_eq!(report["trackers"][0]["failed"], json!([]), "{report}");
    assert_eq!(
        report["trackers"][0]["sent"][0]["kind"], "parent",
        "{report}"
    );
    let shown = env.issue(&["show", "2"]);
    assert_eq!(
        shown["parent"],
        json!(chosen.map(|number| format!("{}#{number}", fake.repo())))
    );
    assert_eq!(shown["conflicts"], json!([]));
}
#[test]
fn github_explicit_reparent_replaces_a_conflict() {
    an_explicit_parent_edit_replaces_a_saved_conflict(&Github, false);
}
#[test]
fn gitlab_explicit_reparent_replaces_a_conflict() {
    an_explicit_parent_edit_replaces_a_saved_conflict(&Gitlab, false);
}
#[test]
fn github_explicit_detach_replaces_a_conflict() {
    an_explicit_parent_edit_replaces_a_saved_conflict(&Github, true);
}
#[test]
fn gitlab_explicit_detach_replaces_a_conflict() {
    an_explicit_parent_edit_replaces_a_saved_conflict(&Gitlab, true);
}

fn unavailable_stored_issue_does_not_block_sync(fake: &dyn IssueFake) {
    use crate::support::forge::argv::gh_get;
    use crate::support::strs;
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(&env, &[Spec::new(1, "Unavailable"), Spec::new(2, "Other")]);
    env.issue(&["sync"]);
    let before = env.issue_doc(fake.tracker(), "1");
    env.issue(&["edit", "2", "--title", "Queued title"]);
    fake.serve_since(
        &env,
        "2026-10-10T09%3A55%3A00Z",
        &[Spec::new(2, "Other").updated("2026-10-11T12:00:00Z")],
    );
    if fake.name() == "github" {
        use crate::support::forge::argv::gh_paginate;
        env.fail_call_with(
            &strs(&gh_paginate("repos/o/r/issues/1/sub_issues?per_page=100")),
            "gh: Not Found (HTTP 404)",
        );
        env.fail_call_with(
            &strs(&gh_get("repos/o/r/issues/1")),
            "gh: Not Found (HTTP 404)",
        );
    } else {
        env.respond(
            &[
                "api",
                "graphql",
                "-f",
                "path=g/p",
                "-f",
                "iid=1",
                "-f",
                "query=IssueHierarchy",
            ],
            r#"{"data":{"project":{"workItems":{"nodes":[]}}}}"#,
        );
    }
    fake.accept_update(&env, 2);
    env.write_config(&fake.config(true));
    let report = env.issue(&["sync"]);
    assert_eq!(
        report["trackers"][0]["unavailable"],
        json!([format!("{}#1", fake.repo())]),
        "{report}"
    );
    assert_eq!(
        report["push"]["trackers"][0]["sent"][0]["kind"], "update",
        "{report}"
    );
    assert_eq!(env.issue_doc(fake.tracker(), "1"), before);
    fake.serve_since(&env, "2026-10-11T11%3A55%3A00Z", &[]);
    let report = env.issue(&["sync"]);
    assert_eq!(
        report["trackers"][0]["unavailable"],
        json!([format!("{}#1", fake.repo())])
    );
    assert_eq!(fake.count(&env, &fake.update_call(2)), 1);
}
#[test]
fn github_unavailable_issue_does_not_block_sync() {
    unavailable_stored_issue_does_not_block_sync(&Github);
}
#[test]
fn gitlab_unavailable_issue_does_not_block_sync() {
    unavailable_stored_issue_does_not_block_sync(&Gitlab);
}

fn broken_hierarchy_routes_are_not_treated_as_missing_issues(fake: &dyn IssueFake) {
    use crate::support::forge::argv::gh_paginate;
    use crate::support::strs;
    let env = Env::new();
    env.write_config(&fake.config(false));
    fake.serve(&env, &[Spec::new(1, "Visible issue")]);
    env.issue(&["sync"]);
    fake.serve_since(&env, "2026-10-10T09%3A55%3A00Z", &[]);
    if fake.name() == "github" {
        env.fail_call_with(
            &strs(&gh_paginate("repos/o/r/issues/1/sub_issues?per_page=100")),
            "gh: Not Found (HTTP 404)",
        );
    } else {
        env.respond(
            &[
                "api",
                "graphql",
                "-f",
                "path=g/p",
                "-f",
                "iid=1",
                "-f",
                "query=IssueHierarchy",
            ],
            r#"{"data":{"project":{"workItems":{"nodes":null}}}}"#,
        );
    }
    let before = env.issue_doc(fake.tracker(), "1");
    env.issue_fail(&["sync"]);
    assert_eq!(env.issue_doc(fake.tracker(), "1"), before);
}
#[test]
fn github_unsupported_hierarchy_endpoint_is_not_a_missing_issue() {
    broken_hierarchy_routes_are_not_treated_as_missing_issues(&Github);
}
#[test]
fn gitlab_malformed_hierarchy_is_not_a_missing_issue() {
    broken_hierarchy_routes_are_not_treated_as_missing_issues(&Gitlab);
}
