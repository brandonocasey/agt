use serde_json::{Value, json};

use crate::support::{Env, numbered};

fn matches(status: &Value) -> Vec<String> {
    status["covered_by"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|pass| pass["match"].as_array().unwrap().clone())
        .map(|value| value.as_str().unwrap().to_owned())
        .collect()
}

fn commit_reviewed(status: &Value, subject: &str) -> bool {
    status["commits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|commit| commit["subject"] == subject)
        .unwrap_or_else(|| panic!("no commit {subject} in {status}"))["reviewed"]
        .as_bool()
        .unwrap()
}

#[test]
fn tracked_ignored_file_stays_in_tree_id() {
    let env = Env::with_branch();
    env.write(".gitignore", "*.log\n");
    env.write("keep.log", "v1\n");
    env.git(&["add", ".gitignore"]);
    env.git(&["add", "-f", "keep.log"]);
    env.git(&["commit", "-q", "-m", "tracked log"]);
    env.write("keep.log", "v2\n");
    let staged_before = env.git(&["ls-files", "-s", "keep.log"]);

    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let pass = env.run(&["pass", "start", "--run", &run, "--kind", "full"])["pass"].clone();
    assert_eq!(pass["dirty"], true);
    let tree = pass["tree"].as_str().unwrap();
    let blob = env.git(&["rev-parse", &format!("{tree}:keep.log")]);
    assert_eq!(blob, env.git_stdin(&["hash-object", "--stdin"], "v2\n"));
    assert_eq!(env.git(&["ls-files", "-s", "keep.log"]), staged_before);
    assert!(env.git_status(&["diff", "--cached", "--quiet"]));
}

#[test]
fn rename_across_anchor() {
    let env = Env::with_branch();
    env.write("b.txt", &numbered(10, &[]));
    env.commit_all("add b");
    env.run(&[
        "comment", "--body", "keep", "--path", "b.txt", "--line", "6",
    ]);
    env.run(&[
        "comment", "--body", "changes", "--path", "b.txt", "--line", "3",
    ]);
    env.run(&[
        "comment",
        "--body",
        "old side",
        "--path",
        "b.txt",
        "--old-line",
        "2",
        "--side",
        "old",
    ]);

    env.git(&["mv", "b.txt", "c.txt"]);
    env.write("c.txt", &format!("0\n{}", numbered(10, &[(3, "three")])));
    env.commit_all("rename and edit");

    let moved = &env.run(&["show", "h1"])["thread"]["current"];
    assert_eq!(
        *moved,
        json!({ "path": "c.txt", "line": 7, "outdated": false })
    );
    let changed = &env.run(&["show", "h2"])["thread"]["current"];
    assert_eq!(changed["path"], "c.txt");
    assert_eq!(changed["outdated"], true);
    let status = env.run(&["status"]);
    assert_eq!(status["outdated_threads"], json!(["h2", "h3"]));
}

#[test]
fn squash_then_status_still_reviewed() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("one");
    env.write("b.txt", "b\n");
    env.commit_all("two");
    env.review("full");

    env.git(&["reset", "-q", "--soft", "main"]);
    env.git(&["commit", "-q", "-m", "squashed"]);
    let status = env.run(&["status"]);
    assert!(status["target"].is_string());
    assert_eq!(status["reviewed"], true);
    assert_eq!(status["reviews_at_head"], 1);
    assert_eq!(status["unreviewed_commits"], json!([]));
    assert!(commit_reviewed(&status, "squashed"));
    let matched = matches(&status);
    assert!(
        matched.contains(&"range-patch-id".to_owned()),
        "{matched:?}"
    );
    assert!(matched.contains(&"head-patch-id".to_owned()), "{matched:?}");
}

#[test]
fn plain_rebase_still_reviewed_and_changed_line_outdated() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("one");
    env.write("b.txt", "b\n");
    env.commit_all("two");
    env.review("full");
    let key = env.run(&["status"])["target"].clone();
    env.run(&[
        "comment", "--body", "fine", "--path", "a.txt", "--line", "3",
    ]);
    env.run(&[
        "comment",
        "--body",
        "base edits this",
        "--path",
        "a.txt",
        "--line",
        "9",
    ]);

    env.git(&["switch", "-q", "main"]);
    env.write("a.txt", &numbered(10, &[(9, "nine")]));
    env.commit_all("main moves");
    env.git(&["switch", "-q", "feat"]);
    env.git(&["rebase", "-q", "main"]);

    let status = env.run(&["status"]);
    assert_eq!(status["target"], key);
    assert_eq!(status["reviewed"], true);
    assert_eq!(status["reviews_at_head"], 1);
    assert_eq!(status["unreviewed_commits"], json!([]));
    assert!(!matches(&status).contains(&"tree".to_owned()));
    assert_eq!(status["outdated_threads"], json!(["h2"]));
}

#[test]
fn conflict_resolved_commit_shows_unreviewed() {
    let env = Env::with_branch();
    env.write("b.txt", "b\n");
    env.commit_all("clean");
    env.write("a.txt", &numbered(10, &[(3, "feat")]));
    env.commit_all("conflicting");
    env.review("full");

    env.git(&["switch", "-q", "main"]);
    env.write("a.txt", &numbered(10, &[(3, "main")]));
    env.commit_all("main edits line 3");
    env.git(&["switch", "-q", "feat"]);
    assert!(!env.git_status(&["rebase", "-q", "main"]));
    env.write("a.txt", &numbered(10, &[(3, "merged")]));
    env.git(&["add", "a.txt"]);
    env.git(&["rebase", "--continue"]);

    let status = env.run(&["status"]);
    assert!(status["target"].is_string());
    assert_eq!(status["reviewed"], false);
    assert_eq!(status["reviews_at_head"], 0);
    assert!(commit_reviewed(&status, "clean"));
    assert!(!commit_reviewed(&status, "conflicting"));
    assert_eq!(status["unreviewed_commits"].as_array().unwrap().len(), 1);
}

#[test]
fn uncommitted_review_then_commit_stays_reviewed() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let pass = env.reviewed_pass(&run, "full");
    let shown = env.run(&["show", &format!("{run}.{pass}")]);
    assert_eq!(shown["pass"]["dirty"], true);

    let before = env.run(&["status"]);
    assert_eq!(before["worktree"]["dirty"], true);
    assert_eq!(before["worktree"]["reviewed"], true);
    assert_eq!(before["reviewed"], false);

    env.commit_all("reviewed work");
    let after = env.run(&["status"]);
    assert_eq!(after["reviewed"], true);
    assert_eq!(after["reviews_at_head"], 1);
    assert!(commit_reviewed(&after, "reviewed work"));
    assert_eq!(after["worktree"]["dirty"], false);

    let ended = env.run(&["run", "end", "--run", &run]);
    assert_eq!(ended["recorded"], json!([format!("{run}.{pass}")]));
    let recorded = env.run(&["show", &format!("{run}.{pass}")]);
    assert_eq!(recorded["pass"]["commits"].as_array().unwrap().len(), 1);
    assert!(recorded["pass"]["head_patch_id"].is_string());

    env.git(&["switch", "-q", "main"]);
    env.write("c.txt", "c\n");
    env.commit_all("main moves");
    env.git(&["switch", "-q", "feat"]);
    env.git(&["rebase", "-q", "main"]);
    let rebased = env.run(&["status"]);
    assert_eq!(rebased["target"], after["target"]);
    assert_eq!(rebased["reviewed"], true);
    assert!(commit_reviewed(&rebased, "reviewed work"));
}

#[test]
fn reused_branch_name_starts_new_target() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("first feature");
    env.review("full");
    let old = env.run(&["status"])["target"].clone();
    assert!(old.is_string());

    env.git(&["switch", "-q", "main"]);
    env.git(&["merge", "-q", "--no-ff", "-m", "merge feat", "feat"]);
    env.git(&["branch", "-q", "-D", "feat"]);
    env.git(&["switch", "-q", "-c", "feat"]);
    env.write("a.txt", &numbered(10, &[(3, "three"), (5, "five")]));
    env.commit_all("second feature");

    let status = env.run(&["status"]);
    assert!(status["target"].is_null(), "{status}");
    assert_eq!(status["reviewed"], false);
    let new = env.run(&["run", "start"])["target"].clone();
    assert_ne!(new, old);
}

#[test]
fn count_full_reviews_and_clean_fix_checks() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("one");
    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let pass = env.run_stdin(
        &[
            "pass",
            "start",
            "--run",
            &run,
            "--kind",
            "full",
            "--reviewers",
            "-",
        ],
        r#"[{"model":"a","harness":"codex"},{"model":"b","harness":"claude-code"}]"#,
    );
    assert_eq!(pass["pass"]["reviewers"][1]["id"], "r2");
    env.run_stdin(
        &[
            "finding",
            "add",
            "--run",
            &run,
            "--pass",
            "c1p0",
            "--reviewer",
            "r1",
        ],
        r#"{"type":"bug","severity":"medium","path":"a.txt","line":3,"scenario":"broken"}"#,
    );
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    let at_review = env.run(&["status"]);
    assert_eq!(at_review["reviews_at_head"], 1);

    env.write("a.txt", &numbered(10, &[(3, "fixed")]));
    env.commit_all("fix");
    assert_eq!(env.run(&["status"])["reviews_at_head"], 0);
    let clean = env.reviewed_pass(&run, "fix-check");
    assert_eq!(clean, "c1p1");
    assert_eq!(env.run(&["status"])["reviews_at_head"], 1);

    env.run_stdin(
        &[
            "pass",
            "start",
            "--run",
            &run,
            "--kind",
            "fix-check",
            "--reviewers",
            "-",
        ],
        r#"[{"id":"r1"}]"#,
    );
    env.run_stdin(
        &[
            "finding",
            "add",
            "--run",
            &run,
            "--pass",
            "c1p2",
            "--reviewer",
            "r1",
        ],
        r#"{"type":"bug","severity":"high","scenario":"still broken"}"#,
    );
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p2"]);
    let status = env.run(&["status"]);
    assert_eq!(status["covered_by"].as_array().unwrap().len(), 2);
    assert_eq!(status["reviews_at_head"], 1);
}

#[test]
fn rebase_where_every_commit_conflicts_keeps_target() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "feat")]));
    env.commit_all("only commit");
    env.review("full");
    env.run(&[
        "comment", "--body", "keep me", "--path", "a.txt", "--line", "8",
    ]);
    let key = env.run(&["status"])["target"].clone();

    env.git(&["switch", "-q", "main"]);
    env.write("a.txt", &numbered(10, &[(3, "main")]));
    env.commit_all("main edits line 3");
    env.git(&["switch", "-q", "feat"]);
    assert!(!env.git_status(&["rebase", "-q", "main"]));
    env.write("a.txt", &numbered(10, &[(3, "merged")]));
    env.git(&["add", "a.txt"]);
    env.git(&["rebase", "--continue"]);

    let status = env.run(&["status"]);
    assert_eq!(status["target"], key);
    assert_eq!(status["reviewed"], false);
    assert_eq!(status["open_threads"][0]["id"], "h1");
    assert_eq!(status["open_threads"][0]["current"]["outdated"], false);
}

#[test]
fn recreated_branch_from_same_base_starts_new_target() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("first try");
    env.review("full");
    let old = env.run(&["status"])["target"].clone();

    env.git(&["switch", "-q", "main"]);
    env.git(&["branch", "-q", "-D", "feat"]);
    // A later reflog time stands in for recreating the branch in a later second.
    env.git_env(
        &["switch", "-q", "-c", "feat"],
        &[("GIT_COMMITTER_DATE", "2030-01-01T00:00:00Z")],
    );
    env.write("a.txt", &numbered(10, &[(4, "four")]));
    env.commit_all("second try");

    let status = env.run(&["status"]);
    assert!(status["target"].is_null(), "{status}");
    let new = env.run(&["run", "start"])["target"].clone();
    assert_ne!(new, old);
}

#[test]
fn linked_worktrees_resolve_same_target() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let key = env.run(&[
        "comment", "--body", "shared", "--path", "a.txt", "--line", "3",
    ])["target"]
        .clone();
    let linked = env.path("linked");
    env.git(&[
        "worktree",
        "add",
        "-q",
        "-f",
        linked.to_str().unwrap(),
        "feat",
    ]);

    let status = env.run_in(&linked, &["status"]);
    assert_eq!(status["target"], key);
    assert_eq!(status["open_threads"][0]["id"], "h1");
    let reply = env.run_in(
        &linked,
        &["reply", "h1", "--body", "from the linked worktree"],
    );
    assert_eq!(reply["target"], key);
    assert_eq!(
        env.run(&["show", "h1"])["thread"]["comments"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn partial_pass_does_not_cover_earlier_commits() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(2, "two")]));
    env.commit_all("A unreviewed");
    env.write("a.txt", &numbered(10, &[(2, "two"), (8, "eight")]));
    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    env.run(&[
        "pass", "start", "--run", &run, "--kind", "full", "--base", "HEAD",
    ]);
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    env.commit_all("B reviewed");
    env.run(&["run", "end", "--run", &run]);
    let status = env.run(&["status"]);
    assert!(!commit_reviewed(&status, "A unreviewed"));
    assert!(commit_reviewed(&status, "B reviewed"));
    assert_eq!(status["reviewed"], false, "{status}");
    assert_eq!(status["reviews_at_head"], 0);
    assert_eq!(
        status["last_reviewed_head"],
        Value::Null,
        "a partial run's repaired head hides nothing"
    );
}

#[test]
fn status_reports_last_reviewed_head() {
    let env = Env::with_branch();
    assert_eq!(env.run(&["status"])["last_reviewed_head"], Value::Null);
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let reviewed = env.commit_all("change");
    env.review("full");
    env.write("a.txt", &numbered(10, &[(3, "three"), (4, "four")]));
    env.commit_all("later");
    assert_eq!(env.run(&["status"])["last_reviewed_head"], reviewed);
    env.git(&["switch", "-q", "main"]);
    env.git(&["merge", "-q", "--ff-only", "feat"]);
    env.write("b.txt", "b\n");
    env.commit_all("on main");
    let status = env.run(&["status"]);
    assert_eq!(
        status["last_reviewed_head"], reviewed,
        "found across targets"
    );
    assert_eq!(
        env.run(&["status", "--head", "main~3"])["last_reviewed_head"],
        Value::Null,
        "only ancestors of the inspected head"
    );
}

#[test]
fn review_from_starts_at_oldest_unreviewed_commit() {
    let env = Env::with_branch();
    let main = env.git(&["rev-parse", "main"]);
    let mut commits = Vec::new();
    for (line, text) in [(2, "A"), (5, "B"), (8, "C")] {
        let mut changes: Vec<(usize, &str)> = vec![(2, "A"), (5, "B"), (8, "C")];
        changes.retain(|(at, _)| *at <= line);
        env.write("a.txt", &numbered(10, &changes));
        commits.push(env.commit_all(text));
    }
    let range = format!("{}..{}", commits[1], commits[2]);
    let run = env.run(&["--range", &range, "run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    env.run(&[
        "--range", &range, "pass", "start", "--run", &run, "--kind", "full",
    ]);
    env.run(&[
        "--range", &range, "pass", "end", "--run", &run, "--pass", "c1p0",
    ]);
    env.run(&["--range", &range, "run", "end", "--run", &run]);
    let status = env.run(&["status"]);
    assert_eq!(status["last_reviewed_head"], commits[2]);
    assert_eq!(status["review_from"], main, "A and B are unreviewed");
}

#[test]
fn conflict_resolved_merge_is_unreviewed() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    env.review("full");
    env.git(&["switch", "-q", "main"]);
    env.write("a.txt", &numbered(10, &[(4, "four")]));
    env.commit_all("main change");
    env.git(&["switch", "-q", "feat"]);
    assert!(
        !env.git_status(&["merge", "-q", "main"]),
        "the merge conflicts"
    );
    env.write("a.txt", &numbered(10, &[(3, "three"), (4, "four")]));
    let merge = env.commit_all("merge main");
    let status = env.run(&["status"]);
    let unreviewed: Vec<&str> = status["unreviewed_commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|sha| sha.as_str().unwrap())
        .collect();
    assert_eq!(unreviewed, [merge.as_str()], "{status}");
    assert_eq!(status["reviewed"], false, "{status}");
}

#[test]
fn range_review_clears_branch_commits() {
    let env = Env::with_branch();
    let main = env.git(&["rev-parse", "main"]);
    env.write("a.txt", &numbered(10, &[(2, "A")]));
    env.commit_all("A");
    env.write("a.txt", &numbered(10, &[(2, "A"), (5, "B")]));
    let b = env.commit_all("B");
    let range = format!("{main}..{b}");
    let run = env.run(&["--range", &range, "run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    env.run(&[
        "--range", &range, "pass", "start", "--run", &run, "--kind", "full",
    ]);
    env.run(&[
        "--range", &range, "pass", "end", "--run", &run, "--pass", "c1p0",
    ]);
    env.run(&["--range", &range, "run", "end", "--run", &run]);
    env.write("a.txt", &numbered(10, &[(2, "A"), (5, "B"), (8, "C")]));
    let c = env.commit_all("C");
    let status = env.run(&["status"]);
    assert_eq!(status["unreviewed_commits"], json!([c]), "{status}");
    assert_eq!(status["review_from"], b);
}

#[test]
fn reviewed_merge_counts_after_a_new_pass() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    env.review("full");
    env.git(&["switch", "-q", "main"]);
    env.write("a.txt", &numbered(10, &[(4, "four")]));
    env.commit_all("main change");
    env.git(&["switch", "-q", "feat"]);
    assert!(!env.git_status(&["merge", "-q", "main"]));
    env.write("a.txt", &numbered(10, &[(3, "three"), (4, "four")]));
    env.commit_all("merge main");
    assert_eq!(env.run(&["status"])["reviewed"], false);
    env.review("full");
    let status = env.run(&["status"]);
    assert_eq!(status["reviewed"], true, "{status}");
    assert_eq!(status["unreviewed_commits"], json!([]));
}

#[test]
fn rewritten_earlier_commit_is_not_covered_by_head_patch_id() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(2, "X")]));
    env.commit_all("X");
    env.write("a.txt", &numbered(10, &[(2, "X"), (8, "Y")]));
    let y = env.commit_all("Y");
    env.review("full");
    env.git(&["reset", "-q", "--hard", "main"]);
    env.write("a.txt", &numbered(10, &[(2, "X changed")]));
    env.commit_all("X changed");
    env.git(&["cherry-pick", &y]);
    let status = env.run(&["status"]);
    assert_eq!(status["reviewed"], false, "{status}");
    assert!(!commit_reviewed(&status, "X changed"));
}

#[test]
fn default_branch_pass_with_base_reviews_the_branch() {
    let env = Env::with_branch();
    env.git(&["switch", "-q", "main"]);
    env.write("a.txt", &numbered(10, &[(2, "A")]));
    let a = env.commit_all("A");
    env.write("a.txt", &numbered(10, &[(2, "A"), (5, "B")]));
    let b = env.commit_all("B");
    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let started = env.run(&[
        "pass", "start", "--run", &run, "--kind", "full", "--base", &a,
    ]);
    assert_eq!(started["pass"]["partial"], false, "{started}");
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    env.run(&["run", "end", "--run", &run]);
    let status = env.run(&["status"]);
    assert_eq!(status["reviewed"], true, "{status}");
    assert_eq!(status["last_reviewed_head"], b);
    assert_eq!(status["review_from"], b);

    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    env.write("a.txt", &numbered(10, &[(2, "A"), (5, "B"), (8, "C")]));
    env.commit_all("C");
    env.run(&[
        "pass", "start", "--run", &run, "--kind", "full", "--base", &b,
    ]);
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    env.write("a.txt", &numbered(10, &[(2, "A"), (5, "B"), (8, "C2")]));
    let fix = env.commit_all("fix C");
    env.run(&["run", "end", "--run", &run]);
    let status = env.run(&["status"]);
    assert_eq!(status["last_reviewed_head"], fix, "the repair counts");
    assert_eq!(status["review_from"], fix);
}
