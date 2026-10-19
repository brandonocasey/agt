//! `agt branch pushed` and `landable` against a local bare repository as origin (reached through
//! `insteadOf`, so `origin` reads as a GitHub URL) and the fake `gh`.

use std::process::Output;

use serde_json::{Value, json};

use crate::support::Env;

const BIN: &str = env!("CARGO_BIN_EXE_agt");

struct Site {
    env: Env,
}

impl Site {
    /// A repository with `main` pushed to a bare origin, and the branch `topic` on it.
    fn new() -> Self {
        let env = Env::new();
        let bare = env.path("origin.git");
        let url = "git@github.com:acme/app.git";
        env.git(&["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()]);
        env.write("base.txt", "base\n");
        env.commit_all("base");
        env.git(&["remote", "add", "origin", url]);
        env.git(&["config", &format!("url.{}.insteadOf", bare.display()), url]);
        env.git(&["push", "-q", "origin", "main"]);
        env.git(&["remote", "set-head", "origin", "main"]);
        env.git(&["checkout", "-q", "-b", "topic"]);
        Self { env }
    }

    fn agt(&self, args: &[&str]) -> Output {
        self.env
            .command(BIN, &self.env.repo)
            .arg("branch")
            .args(args)
            .output()
            .unwrap()
    }

    /// The JSON report and exit code of `agt branch <args>`.
    fn report(&self, args: &[&str]) -> (Value, i32) {
        let output = self.agt(args);
        let code = output.status.code().unwrap();
        assert!(
            code == 0 || code > 3,
            "agt branch {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        (serde_json::from_slice(&output.stdout).unwrap(), code)
    }

    fn commit(&self, file: &str) -> String {
        self.env.write(file, file);
        self.env.commit_all(file)
    }

    /// Answers the fake `gh` for the PRs of `topic`.
    fn pulls(&self, nodes: &[Value]) {
        let body = json!({"data": {"repository": {"pullRequests": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": nodes,
        }}}});
        self.env.respond(
            &[
                "api",
                "graphql",
                "-f",
                "owner=acme",
                "-f",
                "name=app",
                "-f",
                "branch=topic",
                "-f",
                "query=LandedPulls",
            ],
            &body.to_string(),
        );
    }

    /// Answers the fake `gh` for the checks of PR 7; `rollup` entries make a check run exist.
    fn checks(&self, head: &str, rollup: usize) {
        let nodes: Vec<Value> = (0..rollup)
            .map(|_| json!({"__typename": "CheckRun", "name": "build", "status": "COMPLETED", "conclusion": "SUCCESS", "isRequired": true}))
            .collect();
        let body = json!({"data": {"repository": {"pullRequest": {
            "number": 7, "url": "u", "state": "OPEN", "mergedAt": null, "closed": false,
            "headRefOid": head,
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {
                "pageInfo": {"hasNextPage": false, "endCursor": null},
                "nodes": nodes,
            }}}}]},
        }}}});
        self.env.respond(
            &[
                "api",
                "graphql",
                "-f",
                "owner=acme",
                "-f",
                "name=app",
                "-F",
                "number=7",
                "-f",
                "query=PullChecks",
            ],
            &body.to_string(),
        );
    }
}

fn pull(head: &str) -> Value {
    json!({
        "number": 7, "url": "https://github.com/acme/app/pull/7", "title": "t", "state": "OPEN",
        "mergedAt": null, "closedAt": null, "baseRefName": "main", "headRefName": "topic",
        "headRefOid": head, "isCrossRepository": false,
    })
}

#[test]
fn pushed_equal_without_a_pull_request_exits_zero() {
    let site = Site::new();
    site.pulls(&[]);
    let tip = site.commit("a");
    site.env.git(&["push", "-q", "origin", "topic"]);
    site.env.write("dirty.txt", "x");
    let (report, code) = site.report(&["pushed"]);
    assert_eq!(code, 0);
    assert_eq!(report["state"], "equal");
    assert_eq!(report["remote_commit"], tip);
    assert_eq!(report["uncommitted"], 1);
    assert_eq!(report["pr"], Value::Null);
}

#[test]
fn pushed_skips_a_worktree_whose_directory_is_gone() {
    let site = Site::new();
    site.pulls(&[]);
    site.commit("a");
    site.env.git(&["push", "-q", "origin", "topic"]);
    site.env.git(&["checkout", "-q", "main"]);
    let tree = site.env.path("gone-tree");
    site.env
        .git(&["worktree", "add", "-q", tree.to_str().unwrap(), "topic"]);
    std::fs::remove_dir_all(&tree).unwrap();
    let (report, code) = site.report(&["pushed", "topic"]);
    assert_eq!(code, 0);
    assert_eq!(report["state"], "equal");
    assert_eq!(report["uncommitted"], Value::Null);
}

#[test]
fn pushed_counts_ahead_behind_diverged_and_missing() {
    let site = Site::new();
    site.pulls(&[]);
    let (report, code) = site.report(&["pushed"]);
    assert_eq!((report["state"].as_str(), code), (Some("missing"), 4));
    assert_eq!(report["remote_commit"], Value::Null);

    site.commit("a");
    site.env.git(&["push", "-q", "origin", "topic"]);
    site.commit("b");
    let (report, code) = site.report(&["pushed", "topic"]);
    assert_eq!((report["state"].as_str(), code), (Some("ahead"), 4));
    assert_eq!(
        (report["ahead"].clone(), report["behind"].clone()),
        (json!(1), json!(0))
    );

    site.env.git(&["reset", "-q", "--hard", "HEAD~2"]);
    let (report, _) = site.report(&["pushed"]);
    assert_eq!(report["state"], "behind");
    assert_eq!(report["behind"], 1);

    site.commit("c");
    let (report, code) = site.report(&["pushed"]);
    assert_eq!((report["state"].as_str(), code), (Some("diverged"), 4));
    assert_eq!(
        (report["ahead"].clone(), report["behind"].clone()),
        (json!(1), json!(1))
    );
}

#[test]
fn pushed_reports_a_pull_request_head_and_its_checks() {
    let site = Site::new();
    let tip = site.commit("a");
    site.env.git(&["push", "-q", "origin", "topic"]);
    site.pulls(&[pull(&tip)]);
    site.checks(&tip, 1);
    let (report, code) = site.report(&["pushed"]);
    assert_eq!(code, 0);
    assert_eq!(report["pr"]["head_matches"], true);
    assert_eq!(
        report["pr"]["pipeline"],
        json!({"sha": tip, "exists": true})
    );

    let stale = "b".repeat(40);
    site.pulls(&[pull(&stale)]);
    site.checks(&stale, 0);
    let (report, code) = site.report(&["pushed"]);
    assert_eq!(code, 4);
    assert_eq!(report["state"], "equal");
    assert_eq!(report["pr"]["head_matches"], false);
    assert_eq!(report["pr"]["pipeline"]["exists"], false);
}

#[test]
fn landable_reports_counts_and_commits_already_in_the_target() {
    let site = Site::new();
    let picked = site.commit("a");
    site.commit("b");
    site.env.git(&["checkout", "-q", "main"]);
    site.commit("m");
    site.env.git(&["cherry-pick", &picked]);
    let main = site.commit("n");
    site.env.git(&["push", "-q", "origin", "main"]);
    site.env.git(&["checkout", "-q", "topic"]);
    let (report, code) = site.report(&["landable"]);
    assert_eq!(code, 0);
    assert_eq!(report["target"], "origin/main");
    assert_eq!(report["target_commit"], main);
    assert_eq!(
        (report["ahead"].clone(), report["behind"].clone()),
        (json!(2), json!(3))
    );
    assert_eq!(report["conflicts"], json!([]));
    assert_eq!(report["already_in_target"], json!([picked]));
}

#[test]
fn landable_lists_conflicting_paths_against_an_explicit_target() {
    let site = Site::new();
    site.env.write("base.txt", "topic\n");
    site.env.commit_all("topic");
    site.env.git(&["checkout", "-q", "main"]);
    site.env.write("base.txt", "main\n");
    site.env.commit_all("main");
    site.env.git(&["checkout", "-q", "topic"]);
    let (report, code) = site.report(&["landable", "topic", "--target", "main"]);
    assert_eq!(code, 5);
    assert_eq!(report["target"], "main");
    assert_eq!(report["conflicts"], json!(["base.txt"]));
    assert_eq!(report["already_in_target"], json!([]));
}

#[test]
fn landable_without_a_default_branch_names_its_own_flag() {
    let env = Env::new();
    env.git(&["symbolic-ref", "HEAD", "refs/heads/trunk"]);
    env.write("a.txt", "a");
    env.commit_all("a");
    let output = env
        .command(BIN, &env.repo)
        .args(["branch", "landable"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("no default branch found; pass --target"),
        "{stderr}"
    );
    assert!(!stderr.contains("--base"), "{stderr}");
}
