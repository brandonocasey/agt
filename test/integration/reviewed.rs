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

    let pass = env.run(&["pass", "start", "--kind", "full"])["pass"].clone();
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
    let status = env.run(&["status", "--full"]);
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

    let status = env.run(&["status", "--full"]);
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
    let started = env.run(&["pass", "start", "--kind", "full"]);
    let run = started["run"].as_str().unwrap().to_owned();
    let pass = started["pass"]["id"].as_str().unwrap().to_owned();
    env.run(&["pass", "end", "--run", &run, "--pass", &pass]);
    let shown = env.run(&["show", &format!("{run}.{pass}")]);
    assert_eq!(shown["pass"]["dirty"], true);

    let before = env.run(&["status", "--full"]);
    assert_eq!(before["worktree"]["dirty"], true);
    assert_eq!(before["worktree"]["reviewed"], true);
    assert_eq!(before["reviewed"], false);

    env.commit_all("reviewed work");
    let after = env.run(&["status", "--full"]);
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
    let rebased = env.run(&["status", "--full"]);
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
    let new = env.run(&["pass", "start", "--kind", "full"])["target"].clone();
    assert_ne!(new, old);
}

#[test]
fn count_full_reviews_and_clean_fix_checks() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("one");
    let pass = env.run_stdin(
        &["pass", "start", "--kind", "full", "--reviewers", "-"],
        r#"[{"model":"a","harness":"codex"},{"model":"b","harness":"claude-code"}]"#,
    );
    assert_eq!(pass["pass"]["reviewers"][1]["id"], "r2");
    let run = pass["run"].as_str().unwrap().to_owned();
    env.run_stdin(
        &[
            "pass",
            "end",
            "--run",
            &run,
            "--pass",
            "c1p0",
            "--findings",
            "-",
            "--reviewer",
            "r1",
        ],
        r#"{"type":"bug","severity":"medium","path":"a.txt","line":3,"scenario":"broken"}"#,
    );
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
            "pass",
            "end",
            "--run",
            &run,
            "--pass",
            "c1p2",
            "--findings",
            "-",
            "--reviewer",
            "r1",
        ],
        r#"{"type":"bug","severity":"high","scenario":"still broken"}"#,
    );
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
    let new = env.run(&["pass", "start", "--kind", "full"])["target"].clone();
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
    let run = env.run(&["pass", "start", "--kind", "full", "--base", "HEAD"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    env.commit_all("B reviewed");
    env.run(&["run", "end", "--run", &run]);
    let status = env.run(&["status", "--full"]);
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
    let run = env.run(&["--range", &range, "pass", "start", "--kind", "full"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
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
    let run = env.run(&["--range", &range, "pass", "start", "--kind", "full"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
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
fn range_review_of_the_branch_head_counts_and_names_its_run() {
    let env = Env::with_branch();
    let main = env.git(&["rev-parse", "main"]);
    env.write("a.txt", &numbered(10, &[(2, "A")]));
    let a = env.commit_all("A");
    let range = format!("{main}..{a}");
    let run = env.run_stdin(
        &[
            "--range",
            &range,
            "pass",
            "start",
            "--kind",
            "full",
            "--reviewers",
            "-",
        ],
        r#"[{"model":"a","harness":"codex"}]"#,
    )["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let key = env.run(&["--range", &range, "status"])["target"].clone();
    let added = env.run_stdin(
        &[
            "--range",
            &range,
            "pass",
            "end",
            "--run",
            &run,
            "--pass",
            "c1p0",
            "--findings",
            "-",
            "--reviewer",
            "r1",
        ],
        r#"{"type":"bug","severity":"high","path":"a.txt","line":2,"scenario":"broken"}"#,
    );
    env.run(&["--range", &range, "run", "end", "--run", &run]);
    let status = env.run(&["status"]);
    assert_eq!(status["reviewed"], true, "{status}");
    assert_eq!(status["reviews_at_head"], 1, "{status}");
    assert_eq!(status["covered_by"][0]["target"], key, "{status}");
    assert_eq!(status["covered_by"][0]["run"], run, "{status}");
    // The covering target's open finding shows too, so a count never hides it.
    assert_eq!(
        status["open_threads"][0]["id"], added["added"][0],
        "{status}"
    );
    assert_eq!(status["open_threads"][0]["target"], key, "{status}");
    assert_eq!(status["open_threads"][0]["held"], false, "{status}");

    // A dirty work tree with the same change, on a branch the range never reviewed.
    env.git(&["switch", "-q", "-c", "other", "main"]);
    env.write("a.txt", &numbered(10, &[(2, "A")]));
    let status = env.run(&["status"]);
    assert_eq!(status["worktree"]["reviews"], 1, "{status}");
    assert_eq!(status["open_threads"][0]["target"], key, "{status}");
}

#[test]
fn run_left_in_two_target_files_counts_once() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(2, "A")]));
    env.commit_all("A");
    env.review("full");
    let key = env.run(&["status"])["target"].as_str().unwrap().to_owned();
    // A rename to the PR/MR key that crashed before removing the old file.
    let old = walk(&env.state())
        .into_iter()
        .find(|path| path.file_name().unwrap() == format!("{key}.json").as_str())
        .unwrap();
    let mut target: Value = serde_json::from_slice(&std::fs::read(&old).unwrap()).unwrap();
    target["key"] = json!("gh-5");
    std::fs::write(
        old.with_file_name("gh-5.json"),
        serde_json::to_vec(&target).unwrap(),
    )
    .unwrap();
    let status = env.run(&["status"]);
    assert_eq!(status["reviews_at_head"], 1, "{status}");
    assert_eq!(
        status["covered_by"].as_array().unwrap().len(),
        1,
        "{status}"
    );
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .unwrap()
        .flat_map(|entry| {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path)
            } else {
                vec![path]
            }
        })
        .collect()
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
    let status = env.run(&["status", "--full"]);
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
    let started = env.run(&["pass", "start", "--kind", "full", "--base", &a]);
    let run = started["run"].as_str().unwrap().to_owned();
    assert_eq!(started["pass"]["partial"], false, "{started}");
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    env.run(&["run", "end", "--run", &run]);
    let status = env.run(&["status"]);
    assert_eq!(status["reviewed"], true, "{status}");
    assert_eq!(status["last_reviewed_head"], b);
    assert_eq!(status["review_from"], b);

    env.write("a.txt", &numbered(10, &[(2, "A"), (5, "B"), (8, "C")]));
    env.commit_all("C");
    let run = env.run(&["pass", "start", "--kind", "full", "--base", &b])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    env.write("a.txt", &numbered(10, &[(2, "A"), (5, "B"), (8, "C2")]));
    let fix = env.commit_all("fix C");
    env.run(&["run", "end", "--run", &run]);
    let status = env.run(&["status"]);
    assert_eq!(status["last_reviewed_head"], fix, "the repair counts");
    assert_eq!(status["review_from"], fix);
}

#[test]
fn range_repair_clears_fix_commits_on_the_branch() {
    let env = Env::with_branch();
    let main = env.git(&["rev-parse", "main"]);
    env.write("a.txt", &numbered(10, &[(2, "A")]));
    let a = env.commit_all("A");
    let range = format!("{main}..{a}");
    let run = env.run(&["--range", &range, "pass", "start", "--kind", "full"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    env.run(&[
        "--range", &range, "pass", "end", "--run", &run, "--pass", "c1p0",
    ]);
    env.write("a.txt", &numbered(10, &[(2, "A2")]));
    let fix = env.commit_all("fix A");
    env.run(&[
        "--range", &range, "run", "end", "--run", &run, "--head", &fix,
    ]);
    let status = env.run(&["status", "--full"]);
    assert_eq!(status["unreviewed_commits"], json!([]), "{status}");
    let fixed = status["commits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|commit| commit["sha"] == fix)
        .unwrap();
    assert_eq!(fixed["match"], "repair", "{status}");
}

#[test]
fn sha256_repository_reviews_and_tracks_commits() {
    let env = Env::new();
    std::fs::remove_dir_all(env.repo.join(".git")).unwrap();
    env.git(&["init", "-q", "--object-format=sha256", "-b", "main"]);
    env.write("a.txt", &numbered(10, &[]));
    let main = env.commit_all("base");
    assert_eq!(main.len(), 64, "a SHA-256 repository");
    env.git(&["switch", "-q", "-c", "feat"]);
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let started = env.run_stdin(
        &[
            "pass",
            "start",
            "--kind",
            "full",
            "--base",
            &main,
            "--reviewers",
            "-",
        ],
        r#"[{"model":"m","harness":"codex"}]"#,
    );
    let run = started["run"].as_str().unwrap().to_owned();
    let ended = env.run_stdin(
        &[
            "pass",
            "end",
            "--run",
            &run,
            "--pass",
            "c1p0",
            "--findings",
            "-",
            "--reviewer",
            "r1",
        ],
        r#"[{"type":"bug","severity":"high","path":"a.txt","line":3,"scenario":"broken"}]"#,
    );
    let id = ended["added"][0].as_str().unwrap().to_owned();
    env.write("a.txt", &numbered(10, &[(3, "fixed")]));
    let fix = env.commit_all("fix");
    let fixed = env.run(&["resolve", &id, "--fixed", &fix]);
    assert_eq!(fixed["thread"]["resolution"], format!("fixed {fix}"));
    let ended = env.run(&["run", "end", "--run", &run]);
    assert_eq!(ended["status"]["reviewed"], true, "{ended}");
    assert_eq!(ended["status"]["unreviewed_commits"], json!([]));

    // A reworded commit keeps the reviewed state through its patch-id.
    env.git(&["commit", "-q", "--amend", "-m", "fix, reworded"]);
    let status = env.run(&["status"]);
    assert_eq!(status["unreviewed_commits"], json!([]), "{status}");

    // A range target names 64-character ids.
    let head = env.git(&["rev-parse", "HEAD"]);
    let range = format!("{main}..{head}");
    let started = env.run(&["--range", &range, "pass", "start", "--kind", "full"]);
    assert!(
        started["target"].as_str().unwrap().contains(&head),
        "{started}"
    );
    let shown = env.run(&["show", &id]);
    assert_eq!(
        shown["thread"]["anchor"]["commit"].as_str().unwrap().len(),
        64
    );
}

/// Starts a full pass with `extra` flags, ends it and returns the run, left open.
fn pass_without_run_end(env: &Env, extra: &[&str]) -> String {
    let mut args = vec!["pass", "start", "--kind", "full"];
    args.extend(extra);
    let run = env.run(&args)["run"].as_str().unwrap().to_owned();
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    run
}

fn unreviewed(status: &Value) -> Vec<String> {
    status["unreviewed_commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|sha| sha.as_str().unwrap().to_owned())
        .collect()
}

/// The stored target file of the repository's only target.
fn stored_target(env: &Env) -> Value {
    let key = env.run(&["status"])["target"].as_str().unwrap().to_owned();
    let file = walk(&env.state())
        .into_iter()
        .find(|path| path.file_name().unwrap() == format!("{key}.json").as_str())
        .unwrap();
    serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap()
}

/// A conflicting merge of `main` into `feat`, resolved by hand; returns the merge commit.
fn conflict_merge(env: &Env) -> String {
    env.git(&["switch", "-q", "main"]);
    env.write("a.txt", &numbered(10, &[(4, "four")]));
    env.commit_all("main change");
    env.git(&["switch", "-q", "feat"]);
    assert!(
        !env.git_status(&["merge", "-q", "main"]),
        "the merge conflicts"
    );
    env.write("a.txt", &numbered(10, &[(3, "three"), (4, "four")]));
    env.commit_all("merge main")
}

#[test]
fn partial_pass_tree_does_not_cover_a_squash_or_reorder() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    let a = env.commit_all("A");
    env.write("y.txt", "B\n");
    env.commit_all("B");
    pass_without_run_end(&env, &["--base", &a]);
    let run = env.run(&["status", "--full"])["runs"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    env.run(&["run", "end", "--run", &run]);

    env.git(&["reset", "-q", "--soft", "main"]);
    env.commit_all("S");
    let status = env.run(&["status", "--full"]);
    assert!(!commit_reviewed(&status, "S"), "{status}");
    assert_eq!(status["reviewed"], false, "{status}");

    env.git(&["switch", "-q", "-C", "feat", "main"]);
    env.write("y.txt", "B\n");
    env.commit_all("B again");
    env.write("x.txt", "A\n");
    env.commit_all("A again");
    let status = env.run(&["status", "--full"]);
    assert!(commit_reviewed(&status, "B again"), "{status}");
    assert!(!commit_reviewed(&status, "A again"), "{status}");
    assert_eq!(status["reviewed"], false, "{status}");
}

#[test]
fn partial_dirty_pass_is_not_adopted_by_a_squash() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    env.commit_all("A");
    env.write("y.txt", "B\n");
    let run = pass_without_run_end(&env, &["--base", "HEAD"]);
    env.git(&["reset", "-q", "--soft", "main"]);
    env.commit_all("S");
    env.run(&["run", "end", "--run", &run]);
    let status = env.run(&["status", "--full"]);
    assert!(!commit_reviewed(&status, "S"), "{status}");
    assert_eq!(status["reviewed"], false, "{status}");
}

#[test]
fn repair_ignores_commits_merged_in_from_main() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    env.commit_all("A");
    let run = pass_without_run_end(&env, &[]);
    env.git(&["switch", "-q", "main"]);
    env.write("m.txt", "M\n");
    env.commit_all("main commit");
    env.git(&["switch", "-q", "feat"]);
    env.git(&["merge", "-q", "--no-edit", "main"]);
    env.write("fix.txt", "F\n");
    env.commit_all("fix");
    env.run(&["run", "end", "--run", &run]);
    let target = stored_target(&env);
    let repair = target["runs"][0]["repair"].as_array().unwrap();
    assert_eq!(repair.len(), 1, "only the fix, not main's commit: {target}");
}

#[test]
fn rebase_before_run_end_is_not_a_last_reviewed_head() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    env.commit_all("A");
    let run = pass_without_run_end(&env, &[]);
    env.git(&["switch", "-q", "main"]);
    env.write("m.txt", "M\n");
    env.commit_all("main commit");
    env.git(&["switch", "-q", "feat"]);
    env.git(&["rebase", "-q", "main"]);
    env.run(&["run", "end", "--run", &run]);
    let status = env.run(&["status"]);
    assert_eq!(status["last_reviewed_head"], Value::Null, "{status}");
}

#[test]
fn conflict_merge_after_the_last_pass_stays_unreviewed_after_run_end() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let run = pass_without_run_end(&env, &[]);
    let merge = conflict_merge(&env);
    env.run(&["run", "end", "--run", &run]);
    let status = env.run(&["status"]);
    assert_eq!(unreviewed(&status), [merge], "{status}");
    assert_eq!(status["reviewed"], false, "{status}");
}

#[test]
fn range_review_after_a_merge_does_not_mark_it_reviewed() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let first = env.commit_all("change");
    let merge = conflict_merge(&env);
    env.write("g.txt", "g\n");
    env.commit_all("after one");
    env.write("g.txt", "g\nh\n");
    let last = env.commit_all("after two");
    let review = |range: &str| {
        let run = env.run(&["--range", range, "pass", "start", "--kind", "full"])["run"]
            .as_str()
            .unwrap()
            .to_owned();
        env.run(&[
            "--range", range, "pass", "end", "--run", &run, "--pass", "c1p0",
        ]);
        env.run(&["--range", range, "run", "end", "--run", &run]);
    };
    review(&format!("{merge}..{last}"));
    let status = env.run(&["status", "--full"]);
    assert!(unreviewed(&status).contains(&merge), "{status}");
    assert!(!commit_reviewed(&status, "merge main"), "{status}");

    // A range that contains the merge reviews it.
    review(&format!("{first}..{last}"));
    let status = env.run(&["status", "--full"]);
    assert!(commit_reviewed(&status, "merge main"), "{status}");
    let merge_view = status["commits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|commit| commit["sha"] == merge.as_str())
        .unwrap();
    assert_eq!(merge_view["match"], "merge", "{status}");
    assert_eq!(merge_view["by"][0]["via"], "contained", "{status}");
}

#[test]
fn range_review_of_a_merge_marks_it_reviewed_in_its_own_target() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let merge = conflict_merge(&env);
    let run = env.run(&["--range", &merge, "pass", "start", "--kind", "full"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    env.run(&[
        "--range", &merge, "pass", "end", "--run", &run, "--pass", "c1p0",
    ]);
    env.run(&["--range", &merge, "run", "end", "--run", &run]);
    let status = env.run(&["--range", &merge, "status", "--full"]);
    assert!(commit_reviewed(&status, "merge main"), "{status}");
    assert_eq!(status["unreviewed_commits"], json!([]), "{status}");
}

fn already_reviewed(started: &Value) -> &Value {
    &started["already_reviewed"]
}

#[test]
fn pass_start_reports_partly_reviewed_commits() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    let a = env.commit_all("A");
    let run = env.review("full");
    env.write("y.txt", "B\n");
    let b = env.commit_all("B");
    let started = env.run(&["pass", "start", "--kind", "full"]);
    let report = already_reviewed(&started);
    assert_eq!(report["commits"][0]["sha"], a, "{started}");
    assert_eq!(report["commits"][0]["via"], "sha", "{started}");
    let by = &report["commits"][0]["by"][0];
    assert_eq!(by["run"], run.as_str(), "{started}");
    assert_eq!(by["pass"], "c1p0");
    assert_eq!(by["kind"], "full");
    assert_eq!(by["pass_commits"], 1);
    assert_eq!(report["unreviewed"], json!([b]), "{started}");
    assert_eq!(report["head"], json!([]), "{started}");
    assert_eq!(
        report["hint"],
        format!("1 of 2 already reviewed; --base {a} reviews only the other 1 (partial)"),
    );
    // The pass still covers the whole range.
    assert_eq!(started["pass"]["commit_count"], 2, "{started}");
    assert_eq!(started["pass"]["partial"], false);
}

#[test]
fn pass_start_reports_a_fully_reviewed_range() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    env.commit_all("A");
    env.review("full");
    let started = env.run(&["pass", "start", "--kind", "full"]);
    let report = already_reviewed(&started);
    assert_eq!(report["unreviewed"], json!([]), "{started}");
    assert_eq!(report["commits"].as_array().unwrap().len(), 1);
    assert_eq!(report["head"].as_array().unwrap().len(), 1, "{started}");
    assert_eq!(report["hint"], "all 1 commits already reviewed");
}

#[test]
fn pass_start_reports_nothing_for_unreviewed_commits() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    let a = env.commit_all("A");
    let started = env.run(&["pass", "start", "--kind", "full"]);
    let report = already_reviewed(&started);
    assert_eq!(report["commits"], json!([]), "{started}");
    assert_eq!(report["unreviewed"], json!([a]));
    assert_eq!(report["head"], json!([]));
    assert_eq!(report["hint"], Value::Null);
}

#[test]
fn fix_check_report_leaves_out_its_own_run() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    let a = env.commit_all("A");
    let run = pass_without_run_end(&env, &[]);
    // A fix check over the whole branch is not told that its own run reviewed it.
    let started = env.run(&[
        "pass",
        "start",
        "--run",
        &run,
        "--kind",
        "fix-check",
        "--base",
        "main",
    ]);
    let report = already_reviewed(&started);
    assert_eq!(report["commits"], json!([]), "{started}");
    assert_eq!(report["head"], json!([]), "{started}");
    assert_eq!(report["unreviewed"], json!([a]), "{started}");
}

#[test]
fn dry_run_reports_and_writes_nothing() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    let a = env.commit_all("A");
    env.review("full");
    env.write("y.txt", "B\n");
    let b = env.commit_all("B");
    let store = env.state().parent().unwrap().to_path_buf();
    let before = crate::support::snapshot(&store);
    let report = env.run(&["pass", "start", "--kind", "full", "--dry-run"]);
    assert_eq!(report["dry_run"], true, "{report}");
    assert_eq!(report["already_reviewed"]["unreviewed"], json!([b]));
    assert_eq!(report["already_reviewed"]["commits"][0]["sha"], a);
    assert!(report["pass"].is_null(), "{report}");
    assert!(
        before == crate::support::snapshot(&store),
        "a dry run wrote to the store"
    );
    let runs = env.run(&["status", "--full"])["runs"].clone();
    assert_eq!(runs.as_array().unwrap().len(), 1);
}

#[test]
fn pass_abort_deletes_an_open_pass_and_an_emptied_run() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    env.commit_all("A");
    let run = env.run(&["pass", "start", "--kind", "full"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let blocked = env.fail(&["run", "end", "--run", &run], None);
    assert!(blocked.contains("is still open"), "{blocked}");
    let aborted = env.run(&["pass", "abort", "--run", &run]);
    assert_eq!(aborted["aborted"], "c1p0", "{aborted}");
    assert_eq!(aborted["run_removed"], true, "{aborted}");
    let status = env.run(&["status", "--full"]);
    assert_eq!(status["runs"], json!([]), "{status}");
    assert_eq!(status["reviewed"], false);
}

#[test]
fn pass_abort_keeps_the_run_with_ended_passes_and_refuses_ended_ones() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    env.commit_all("A");
    let run = pass_without_run_end(&env, &[]);
    let error = env.fail(&["pass", "abort", "--run", &run], None);
    assert!(error.contains("no open pass"), "{error}");
    let started = env.run(&["pass", "start", "--run", &run, "--kind", "fix-check"]);
    assert_eq!(started["pass"]["id"], "c1p1");
    let error = env.fail(&["pass", "abort", "--run", &run, "--pass", "c1p0"], None);
    assert!(error.contains("already ended"), "{error}");
    let aborted = env.run(&["pass", "abort", "--run", &run, "--pass", "c1p1"]);
    assert_eq!(aborted["run_removed"], false, "{aborted}");
    let target = stored_target(&env);
    assert_eq!(target["runs"][0]["passes"].as_array().unwrap().len(), 1);
    env.run(&["run", "end", "--run", &run]);
}

#[test]
fn status_names_what_marked_each_commit() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    let a = env.commit_all("A");
    let run = pass_without_run_end(&env, &[]);
    env.write("fix.txt", "F\n");
    env.commit_all("fix");
    env.run(&["run", "end", "--run", &run]);
    let status = env.run(&["status", "--full"]);
    let target = status["target"].clone();
    let commits = status["commits"].as_array().unwrap();
    let a_view = commits
        .iter()
        .find(|commit| commit["sha"] == a.as_str())
        .unwrap();
    assert_eq!(a_view["by"][0]["target"], target, "{status}");
    assert_eq!(a_view["by"][0]["run"], run.as_str());
    assert_eq!(a_view["by"][0]["pass"], "c1p0");
    assert_eq!(a_view["by"][0]["kind"], "full");
    assert_eq!(a_view["by"][0]["via"], "sha");
    let fix = commits
        .iter()
        .find(|commit| commit["subject"] == "fix")
        .unwrap();
    assert_eq!(fix["match"], "repair", "{status}");
    assert_eq!(fix["by"][0]["via"], "repair");
    assert!(fix["by"][0]["pass"].is_null());
    assert_eq!(status["already_reviewed"], 2);
    let brief = env.run(&["status"]);
    assert_eq!(brief["already_reviewed"], 2, "{brief}");
}

#[test]
fn changed_commit_shows_its_earlier_reviewed_version_without_counting_it() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    env.commit_all("Add parser");
    env.review("full");
    env.write("x.txt", "A changed\n");
    env.git(&["add", "-A"]);
    env.git(&["commit", "-q", "--amend", "--no-edit"]);
    let status = env.run(&["status", "--full"]);
    assert!(!commit_reviewed(&status, "Add parser"), "{status}");
    assert_eq!(status["reviewed"], false);
    let view = &status["commits"][0];
    assert_eq!(view["earlier_version"]["pass"], "c1p0", "{status}");
    // The brief status stays cheap and carries no hint.
    assert!(env.run(&["status"])["commits"].is_null());
}

/// Reviews `rev` as a commit target of its own and ends the run.
fn review_commit(env: &Env, rev: &str) {
    let run = env.run(&["--range", rev, "pass", "start", "--kind", "full"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    env.run(&[
        "--range", rev, "pass", "end", "--run", &run, "--pass", "c1p0",
    ]);
    env.run(&["--range", rev, "run", "end", "--run", &run]);
}

#[test]
fn hint_suggests_no_base_when_the_first_commit_is_the_unreviewed_one() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    env.commit_all("A");
    env.write("y.txt", "B\n");
    let b = env.commit_all("B");
    review_commit(&env, &b);
    let report = env.run(&["pass", "start", "--kind", "full", "--dry-run"]);
    assert_eq!(
        report["already_reviewed"]["hint"], "1 of 2 already reviewed",
        "{report}"
    );
}

#[test]
fn fix_check_report_does_not_cover_the_head_by_a_lone_fix_commit() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    env.commit_all("A");
    env.write("y.txt", "B\n");
    env.commit_all("B");
    let run = pass_without_run_end(&env, &[]);
    env.write("fix.txt", "F\n");
    let fix = env.commit_all("fix");
    review_commit(&env, &fix);
    let report = env.run(&[
        "pass",
        "start",
        "--run",
        &run,
        "--kind",
        "fix-check",
        "--dry-run",
    ]);
    assert_eq!(report["already_reviewed"]["head"], json!([]), "{report}");
    assert_eq!(env.run(&["status"])["covered_by"], json!([]));
}

#[test]
fn pass_abort_refuses_a_pass_whose_reviewer_replied() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let reviewer = r#"[{"model":"m","effort":"high","harness":"h","starting_context":"fresh"}]"#;
    let run = env.run_stdin(
        &["pass", "start", "--kind", "full", "--reviewers", "-"],
        reviewer,
    )["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let ended = env.run_stdin(
        &[
            "pass",
            "end",
            "--run",
            &run,
            "--pass",
            "c1p0",
            "--findings",
            "-",
            "--reviewer",
            "r1",
        ],
        r#"[{"type":"bug","severity":"high","path":"a.txt","line":3,"scenario":"broken"}]"#,
    );
    let id = ended["added"][0].as_str().unwrap().to_owned();
    let started = env.run_stdin(
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
        reviewer,
    );
    let checker = started["pass"]["reviewers"][0]["id"].as_str().unwrap();
    env.run(&[
        "resolve",
        &id,
        "--not-fixed",
        "still broken",
        "--reviewer",
        &format!("{run}.c1p1.{checker}"),
    ]);
    let error = env.fail(&["pass", "abort", "--run", &run], None);
    assert!(error.contains("has findings or replies"), "{error}");
}

#[test]
fn earlier_version_hint_ignores_other_targets() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    env.commit_all("fix lint");
    let first = env.git(&["rev-parse", "HEAD"]);
    review_commit(&env, &first);
    env.write("x.txt", "A changed\n");
    env.git(&["add", "-A"]);
    env.git(&["commit", "-q", "--amend", "--no-edit"]);
    let status = env.run(&["status", "--full"]);
    assert!(!commit_reviewed(&status, "fix lint"), "{status}");
    assert!(
        status["commits"][0]["earlier_version"].is_null(),
        "{status}"
    );
}

#[test]
fn pass_start_report_counts_an_unreviewed_conflict_merge() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    env.review("full");
    let merge = conflict_merge(&env);
    // The merge is unreviewed, so the range is not fully reviewed, with or without commits after it.
    let report = env.run(&["pass", "start", "--kind", "full", "--dry-run"]);
    let report = &report["already_reviewed"];
    assert!(
        report["unreviewed"]
            .as_array()
            .unwrap()
            .contains(&json!(merge)),
        "{report}"
    );
    assert_ne!(report["hint"], "all 1 commits already reviewed", "{report}");
    env.write("g.txt", "g\n");
    env.commit_all("after");
    let report = env.run(&["pass", "start", "--kind", "full", "--dry-run"]);
    let report = &report["already_reviewed"];
    assert!(
        report["unreviewed"]
            .as_array()
            .unwrap()
            .contains(&json!(merge)),
        "{report}"
    );
    let hint = report["hint"].as_str().unwrap();
    assert!(!hint.contains(&format!("--base {merge}")), "{hint}");
}

#[test]
fn earlier_version_hint_skips_commits_still_in_the_history() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    env.commit_all("fix lint");
    env.review("full");
    env.write("y.txt", "B\n");
    env.commit_all("fix lint");
    let status = env.run(&["status", "--full"]);
    let unreviewed = status["commits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|view| view["reviewed"] == false)
        .unwrap();
    assert!(unreviewed["earlier_version"].is_null(), "{status}");
}

#[test]
fn default_branch_report_lists_commits_unreviewed_after_a_base_pass() {
    let env = Env::with_branch();
    env.git(&["switch", "-q", "main"]);
    env.write("a.txt", "A\n");
    let a = env.commit_all("A");
    env.write("b.txt", "B\n");
    env.commit_all("B");
    pass_without_run_end(&env, &["--base", &a]);
    env.write("c.txt", "C\n");
    let c = env.commit_all("C");
    let report = env.run(&["pass", "start", "--kind", "full", "--base", &a, "--dry-run"]);
    let report = &report["already_reviewed"];
    assert_eq!(report["unreviewed"], json!([c]), "{report}");
    assert_ne!(report["hint"], "all 2 commits already reviewed", "{report}");
}

#[test]
fn continuing_fix_check_hint_suggests_no_base_at_the_previous_head() {
    let env = Env::with_branch();
    env.write("x.txt", "A\n");
    env.commit_all("A");
    let run = pass_without_run_end(&env, &[]);
    env.write("f1.txt", "1\n");
    env.commit_all("F1");
    env.write("f2.txt", "2\n");
    let f2 = env.commit_all("F2");
    review_commit(&env, &f2);
    let report = env.run(&[
        "pass",
        "start",
        "--run",
        &run,
        "--kind",
        "fix-check",
        "--dry-run",
    ]);
    assert_eq!(
        report["already_reviewed"]["hint"], "1 of 2 already reviewed",
        "{report}"
    );
}

#[test]
fn default_branch_report_counts_an_unreviewed_conflict_merge() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let merge = conflict_merge(&env);
    env.git(&["switch", "-q", "main"]);
    let before = env.git(&["rev-parse", "main"]);
    env.git(&["merge", "-q", "--ff-only", "feat"]);
    // On the default branch the range start is the head, so the merge is only in the pass's base range.
    let report = env.run(&[
        "pass",
        "start",
        "--kind",
        "full",
        "--base",
        &before,
        "--dry-run",
    ]);
    let report = &report["already_reviewed"];
    assert!(
        report["unreviewed"]
            .as_array()
            .unwrap()
            .contains(&json!(merge)),
        "{report}"
    );
}
