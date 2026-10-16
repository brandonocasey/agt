use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;

use crate::support::{BIN, Env, numbered, snapshot};

const FINDINGS: &str = include_str!("../fixtures/findings.json");
const REVIEWER: &str = r#"[{"model":"gpt-6","effort":"high","harness":"codex","harness_version":"0.162.1","starting_context":"fresh"}]"#;

#[test]
fn run_and_pass_lifecycle() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let head = env.commit_all("change");

    let started = env.run(&["run", "start"]);
    let run = started["run"].as_str().unwrap().to_owned();
    assert!(run.starts_with("rv-") && run.len() == 7, "{run}");
    let key = started["target"].as_str().unwrap();
    assert!(key.starts_with("branch-feat-"), "{key}");

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
        REVIEWER,
    )["pass"]
        .clone();
    assert_eq!(pass["id"], "c1p0");
    assert_eq!(pass["head"], head.as_str());
    assert_eq!(pass["dirty"], false);
    assert_eq!(pass["reviewers"][0]["id"], "r1");
    assert_eq!(pass["reviewers"][0]["harness_version"], "0.162.1");
    assert_eq!(pass["commits"].as_array().unwrap().len(), 1);
    assert!(pass["range_patch_id"].is_string());

    let open = env.fail(
        &["pass", "start", "--run", &run, "--kind", "fix-check"],
        None,
    );
    assert!(open.contains("c1p0 is still open"), "{open}");
    let ended = env.run(&["pass", "end", "--run", &run, "--pass", "c1p0", "--lock"]);
    assert_eq!(ended["pass"]["locked"], true);
    let locked = env.fail(
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
        Some(FINDINGS),
    );
    assert!(locked.contains("is locked"), "{locked}");

    let fix = env.run_stdin(
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
        r#"[{"id":"r1","starting_context":"continued c1p0 + 1 finding, fix diff"}]"#,
    );
    assert_eq!(fix["pass"]["id"], "c1p1");
    assert_eq!(fix["pass"]["base"], head.as_str());
    assert_eq!(fix["pass"]["reviewers"][0]["model"], "gpt-6");
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p1"]);

    let second = env.run_stdin(
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
        r#"[{"model":"m2","harness":"claude-code"}]"#,
    );
    assert_eq!(second["pass"]["id"], "c2p0");
    assert_eq!(second["pass"]["reviewers"][0]["id"], "r2");
    let still_open = env.fail(&["run", "end", "--run", &run], None);
    assert!(still_open.contains("still open"), "{still_open}");
    env.run(&["pass", "end", "--run", &run, "--pass", "c2p0"]);

    let end = env.run(&["run", "end", "--run", &run]);
    assert!(end["ended_at"].is_string());
    let again = env.fail(&["run", "end", "--run", &run], None);
    assert!(again.contains("already ended"), "{again}");

    let shown = env.run(&["show", &run]);
    assert_eq!(shown["kind"], "run");
    assert_eq!(shown["run"]["passes"].as_array().unwrap().len(), 3);
    let shown = env.run(&["show", &format!("{run}.c1p1")]);
    assert_eq!(shown["kind"], "pass");
    assert_eq!(shown["pass"]["kind"], "fix-check");
}

#[test]
fn finding_add_records_verified_and_killed() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    env.run_stdin(
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
        REVIEWER,
    );
    let added = env.run_stdin(
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
        FINDINGS,
    );
    let prefix = format!("{run}.c1p0.r1");
    assert_eq!(
        added["added"],
        serde_json::json!([format!("{prefix}.f1"), format!("{prefix}.f2")])
    );
    assert_eq!(added["killed"], serde_json::json!([format!("{prefix}.f3")]));

    let thread = &env.run(&["show", &format!("{prefix}.f1")])["thread"];
    assert_eq!(thread["type"], "bug");
    assert_eq!(thread["severity"], "high");
    assert_eq!(thread["anchor"]["line"], 3);
    assert_eq!(
        thread["finding"]["evidence"],
        "a.txt:3 returns before the check."
    );
    assert_eq!(thread["comments"][0]["author"]["kind"], "agent");
    assert_eq!(thread["comments"][0]["author"]["model"], "gpt-6");
    assert_eq!(thread["comments"][0]["author"]["reviewer"], "r1");
    assert_eq!(thread["current"]["outdated"], false);

    let killed = env.run(&["show", &format!("{prefix}.f3")]);
    assert_eq!(killed["kind"], "killed");
    assert_eq!(
        killed["finding"]["reason"],
        "The guard moved to the caller."
    );
    assert_eq!(killed["finding"]["anchor"]["side"], "old");

    let ended = env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    assert_eq!(ended["pass"]["major_found"], true);

    let more = env.run_stdin(
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
        r#"{"type":"nit","path":"a.txt","line":1,"body":"Rename it."}"#,
    );
    assert_eq!(more["added"][0], format!("{prefix}.f4"));

    for (input, message) in [
        (r#"{"type":"bug","scenario":"x"}"#, "needs a severity"),
        (
            r#"{"type":"nit","scenario":"x","killed":true}"#,
            "needs a reason",
        ),
        (
            r#"{"type":"nit","scenario":"x","reason":"y"}"#,
            "only for killed",
        ),
        (
            r#"{"type":"nit","scenario":"x","colour":1}"#,
            "unknown field",
        ),
        (r#"{"type":"nit","scenario":"x","line":2}"#, "need a path"),
        (
            r#"{"type":"nit","scenario":"x","path":"../a","line":2}"#,
            "relative",
        ),
        ("[]", "no findings"),
    ] {
        let error = env.fail(
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
            Some(input),
        );
        assert!(error.starts_with("agent-review: "), "{error}");
        assert!(error.contains(message), "{input}: {error}");
    }
    let unknown = env.fail(
        &[
            "finding",
            "add",
            "--run",
            &run,
            "--pass",
            "c1p0",
            "--reviewer",
            "r9",
        ],
        Some(FINDINGS),
    );
    assert!(unknown.contains("reviewer r9 is not in pass"), "{unknown}");
}

#[test]
fn comment_and_reply() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let head = env.commit_all("change");

    let thread = &env.run(&[
        "comment",
        "--body",
        "Why three?",
        "--path",
        "a.txt",
        "--line",
        "3",
    ])["thread"];
    assert_eq!(thread["id"], "h1");
    assert_eq!(thread["type"], "comment");
    assert_eq!(thread["anchor"]["commit"], head.as_str());
    assert_eq!(thread["comments"][0]["author"]["login"], "Tester");

    let general = env.run_stdin(
        &[
            "comment", "--body", "-", "--type", "question", "--author", "alice",
        ],
        "Is this needed?\n",
    );
    assert_eq!(general["thread"]["id"], "h2");
    assert!(general["thread"]["anchor"].is_null());
    assert_eq!(general["thread"]["comments"][0]["author"]["login"], "alice");

    let missing = env.fail(
        &[
            "comment", "--body", "x", "--path", "nope.txt", "--line", "1",
        ],
        None,
    );
    assert!(missing.contains("does not exist"), "{missing}");

    let replied = env.run(&[
        "reply",
        "h1",
        "--body",
        "Spec says so.",
        "--model",
        "gpt-6",
        "--harness",
        "codex",
        "--effort",
        "high",
    ]);
    let author = &replied["thread"]["comments"][1]["author"];
    assert_eq!(author["kind"], "agent");
    assert_eq!(author["model"], "gpt-6");
    let bad = env.fail(
        &[
            "reply", "h1", "--body", "x", "--author", "bob", "--model", "m",
        ],
        None,
    );
    assert!(bad.contains("drop the agent flags"), "{bad}");
    let half = env.fail(&["reply", "h1", "--body", "x", "--model", "m"], None);
    assert!(half.contains("--harness"), "{half}");

    env.git(&["config", "user.name", ""]);
    let anonymous = env.fail(&["comment", "--body", "x"], None);
    assert!(anonymous.contains("no author"), "{anonymous}");
}

#[test]
fn resolve_triage_and_reopen() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let head = env.commit_all("change");
    env.run(&[
        "comment",
        "--body",
        "Why three?",
        "--path",
        "a.txt",
        "--line",
        "3",
    ]);
    env.run(&["comment", "--body", "Is this needed?", "--type", "question"]);

    let held = env.run(&["resolve", "h2", "--triage", "question"]);
    assert_eq!(held["thread"]["resolved"], false);
    assert_eq!(held["thread"]["held"], true);
    let answered = env.run(&["resolve", "h2", "--resolution", "answered"]);
    assert_eq!(answered["thread"]["resolved"], true);
    assert_eq!(answered["thread"]["held"], false);

    let fixed_needs_sha = env.fail(&["resolve", "h1", "--triage", "fixed"], None);
    assert!(fixed_needs_sha.contains("fixed <sha>"), "{fixed_needs_sha}");
    let fixed = env.run(&[
        "resolve",
        "h1",
        "--triage",
        "fixed",
        "--resolution",
        "fixed HEAD",
    ]);
    assert_eq!(fixed["thread"]["resolution"], format!("fixed {head}"));
    assert_eq!(fixed["thread"]["triage"], "fixed");
    let reopened = env.run(&[
        "resolve",
        "h1",
        "--reopen",
        "--resolution",
        "not fixed: still wrong",
    ]);
    assert_eq!(reopened["thread"]["resolved"], false);
    assert_eq!(reopened["thread"]["resolution"], "not fixed: still wrong");
    let stale = env.run(&["resolve", "h1", "--triage", "stale"]);
    assert_eq!(stale["thread"]["resolution"], "stale");
    let invalid = env.fail(&["resolve", "h1", "--resolution", "done"], None);
    assert!(invalid.contains("resolution must be"), "{invalid}");
    let unknown = env.fail(&["reply", "h9", "--body", "x"], None);
    assert!(unknown.contains("thread h9 not found"), "{unknown}");
}

#[test]
fn list_filters_and_prompt_format() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    assert_eq!(env.run(&["list"])["threads"], serde_json::json!([]));

    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    env.run_stdin(
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
        REVIEWER,
    );
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
        FINDINGS,
    );
    env.run(&[
        "comment",
        "--body",
        "Human note",
        "--path",
        "a.txt",
        "--line",
        "1",
    ]);
    env.run(&[
        "resolve",
        &format!("{run}.c1p0.r1.f2"),
        "--resolution",
        "answered",
    ]);

    assert_eq!(env.run(&["list"])["threads"].as_array().unwrap().len(), 3);
    let open = env.run(&["list", "--open"]);
    let ids: Vec<&str> = open["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [format!("{run}.c1p0.r1.f1").as_str(), "h1"]);
    let by_run = env.run(&["list", "--run", &run]);
    assert_eq!(by_run["threads"].as_array().unwrap().len(), 2);
    assert_eq!(by_run["killed"].as_array().unwrap().len(), 1);
    assert_eq!(
        env.run(&["list", "--run", "rv-zzzz"])["killed"],
        serde_json::json!([])
    );

    let prompt = env.text(&["list", "--run", &run, "--format", "prompt"]);
    assert!(
        prompt.contains(&format!("- {run}.c1p0.r1.f1 [bug/high] a.txt:3 (open)")),
        "{prompt}"
    );
    assert!(
        prompt.contains("Evidence: a.txt:3 returns before the check."),
        "{prompt}"
    );
    assert!(prompt.contains("(resolved: answered)"), "{prompt}");
    assert!(
        prompt.contains("Killed: The guard moved to the caller."),
        "{prompt}"
    );
    assert!(!prompt.contains("Human note"), "{prompt}");
    assert!(!prompt.trim_start().starts_with('{'));
}

#[test]
fn show_reports_unknown_ids() {
    let env = Env::with_branch();
    let missing = env.fail(&["show", "h1"], None);
    assert!(missing.contains("h1 not found"), "{missing}");
}

#[test]
fn range_and_commit_targets() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let first = env.commit_all("one");
    env.write("b.txt", "b\n");
    let second = env.commit_all("two");

    let range = env.run(&["--range", "main..HEAD", "run", "start"]);
    let base = env.git(&["rev-parse", "main"]);
    assert_eq!(range["target"], format!("range-{base}-{second}"));
    let run = range["run"].as_str().unwrap().to_owned();
    let pass = env.run(&[
        "--range",
        "main..HEAD",
        "pass",
        "start",
        "--run",
        &run,
        "--kind",
        "full",
    ]);
    assert_eq!(pass["pass"]["commits"].as_array().unwrap().len(), 2);

    let commit = env.run(&["--range", &first, "run", "start"]);
    assert_eq!(commit["target"], format!("commit-{first}"));
    let key = commit["target"].as_str().unwrap().to_owned();
    let run = commit["run"].as_str().unwrap().to_owned();
    env.run(&[
        "--target", &key, "pass", "start", "--run", &run, "--kind", "full",
    ]);
    env.run(&[
        "--target", &key, "pass", "end", "--run", &run, "--pass", "c1p0",
    ]);
    let status = env.run(&["--target", &key, "status"]);
    assert_eq!(status["head"], first.as_str());
    assert_eq!(status["reviewed"], true);
    assert!(status["worktree"].is_null());

    let symmetric = env.fail(&["--range", "main...HEAD", "status"], None);
    assert!(symmetric.contains("symmetric"), "{symmetric}");
    env.git(&["switch", "-q", "--detach", "HEAD"]);
    let detached = env.fail(&["status"], None);
    assert!(detached.contains("HEAD is detached"), "{detached}");
}

#[test]
fn errors_outside_a_repository() {
    let env = Env::new();
    let output = env
        .command(BIN, &env.path("home"))
        .arg("status")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "agent-review: not inside a git work tree\n"
    );
}

#[test]
fn store_layout_and_permissions() {
    let env = Env::with_branch();
    env.git(&["remote", "add", "origin", "git@github.com:Owner/Repo.git"]);
    let key = env.run(&["comment", "--body", "x"])["target"]
        .as_str()
        .unwrap()
        .to_owned();
    let dir = env.state().join("github.com").join("Owner").join("Repo");
    let file = dir.join(format!("{key}.json"));
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let stored: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(stored["version"], 1);
    assert_eq!(stored["revision"], 1);
    assert_eq!(stored["repo"], "github.com/Owner/Repo");
    env.run(&["reply", "h1", "--body", "y"]);
    let stored: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(stored["revision"], 2);

    fs::remove_file(&file).unwrap();
    std::os::unix::fs::symlink(env.path("gitconfig"), &file).unwrap();
    let symlink = env.fail(&["list"], None);
    assert!(symlink.contains("not a symbolic link"), "{symlink}");
}

#[test]
fn local_repo_key_without_remote() {
    let env = Env::with_branch();
    env.run(&["comment", "--body", "x"]);
    let local = fs::read_dir(env.state().join("local"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let name = local.file_name().into_string().unwrap();
    let (prefix, hash) = name.rsplit_once('-').unwrap();
    assert_eq!(prefix, "repo");
    assert_eq!(hash.len(), 12);
    let common = env.repo.join(".git");
    let expected = env.git_stdin(&["hash-object", "--stdin"], common.to_str().unwrap());
    assert_eq!(&expected[..12], hash);
}

#[test]
fn concurrent_writers_do_not_lose_updates() {
    let env = Env::with_branch();
    env.run(&["comment", "--body", "first"]);
    let writers = 12;
    let children: Vec<_> = (0..writers)
        .map(|index| {
            env.command(BIN, &env.repo)
                .args(["comment", "--body", &format!("note {index}")])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let threads = env.run(&["list"])["threads"].as_array().unwrap().clone();
    assert_eq!(threads.len(), writers + 1);
    let mut ids: Vec<&str> = threads.iter().map(|t| t["id"].as_str().unwrap()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), writers + 1);
    let stored = fs::read_dir(env.state().join("local"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let file = fs::read_dir(stored)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .unwrap();
    let target: serde_json::Value = serde_json::from_slice(&fs::read(file).unwrap()).unwrap();
    assert_eq!(target["revision"], writers + 1);
}

#[test]
fn status_writes_nothing() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    env.review("full");
    env.run(&["comment", "--body", "x", "--path", "a.txt", "--line", "3"]);
    env.write("a.txt", &numbered(10, &[(3, "dirty")]));
    env.write("new.txt", "untracked\n");

    let store_before = snapshot(&env.state());
    let git_before = snapshot(&env.repo.join(".git"));
    let status = env.run(&["status"]);
    assert_eq!(status["worktree"]["dirty"], true);
    env.run(&["status", "--head", "HEAD~1"]);
    env.run(&["list"]);
    env.run(&["show", "h1"]);
    assert_eq!(snapshot(&env.state()), store_before);
    assert_eq!(snapshot(&env.repo.join(".git")), git_before);

    let fresh = Env::with_branch();
    fresh.run(&["status"]);
    assert!(!fresh.state().exists());
}

#[test]
fn prune_by_age() {
    let env = Env::with_branch();
    env.run(&["comment", "--body", "old"]);
    let day = 24 * 60 * 60;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let soon = (now + 89 * day).to_string();
    let later = (now + 91 * day).to_string();

    let kept = env.run_with(&["prune"], None, &[("AGENT_REVIEW_NOW", &soon)]);
    assert_eq!(kept["pruned"], serde_json::json!([]));
    let dry = env.run_with(
        &["prune", "--dry-run"],
        None,
        &[("AGENT_REVIEW_NOW", &later)],
    );
    assert_eq!(dry["pruned"].as_array().unwrap().len(), 1);
    assert_eq!(env.run(&["list"])["threads"].as_array().unwrap().len(), 1);

    let pruned = env.run_with(&["prune"], None, &[("AGENT_REVIEW_NOW", &later)]);
    assert_eq!(pruned["pruned"].as_array().unwrap().len(), 1);
    assert_eq!(env.run(&["list"])["threads"], serde_json::json!([]));

    env.run(&["comment", "--body", "again"]);
    let output = env
        .command(BIN, &env.path("home"))
        .args(["prune", "--all"])
        .env("AGENT_REVIEW_NOW", &later)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let all: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(all["pruned"].as_array().unwrap().len(), 1);
    assert_eq!(env.run(&["list"])["threads"], serde_json::json!([]));

    let old_key = env.run(&["comment", "--body", "old branch"])["target"].clone();
    env.git(&["switch", "-q", "-c", "other"]);
    env.write("b.txt", "b\n");
    env.commit_all("other");
    let later_env = [("AGENT_REVIEW_NOW", later.as_str())];
    let run = env.run_with(&["run", "start"], None, &later_env)["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let end = env.run_with(&["run", "end", "--run", &run], None, &later_env);
    assert_eq!(end["pruned"][0]["target"], old_key);
    assert_eq!(end["pruned"].as_array().unwrap().len(), 1);
}

fn add_findings(env: &Env, run: &str, pass: &str, findings: &str) {
    let reviewer = if pass.starts_with("c1") { "r1" } else { "r2" };
    env.run_stdin(
        &[
            "finding",
            "add",
            "--run",
            run,
            "--pass",
            pass,
            "--reviewer",
            reviewer,
        ],
        findings,
    );
}

fn start_pass(env: &Env, run: &str) -> String {
    let pass = env.run_stdin(
        &[
            "pass",
            "start",
            "--run",
            run,
            "--kind",
            "full",
            "--reviewers",
            "-",
        ],
        REVIEWER,
    );
    pass["pass"]["id"].as_str().unwrap().to_owned()
}

#[test]
fn finding_kill_works_on_a_locked_pass() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    start_pass(&env, &run);
    add_findings(
        &env,
        &run,
        "c1p0",
        r#"{"type":"bug","severity":"high","path":"a.txt","line":3,"scenario":"Breaks."}"#,
    );
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p0", "--lock"]);
    let id = format!("{run}.c1p0.r1.f1");
    let killed = env.run(&["finding", "kill", &id, "--reason", "duplicate of a test"]);
    assert_eq!(killed["killed"]["reason"], "duplicate of a test");
    let shown = env.run(&["show", &id]);
    assert_eq!(shown["kind"], "killed");
    let pass = env.run(&["show", &format!("{run}.c1p0")]);
    assert_eq!(pass["pass"]["major_found"], false);
    assert_eq!(pass["pass"]["locked"], true);
    let listed = env.run(&["list", "--run", &run]);
    assert_eq!(listed["threads"], serde_json::json!([]));
    assert_eq!(listed["killed"][0]["id"], id);
    let again = env.fail(&["finding", "kill", &id, "--reason", "x"], None);
    assert!(again.contains("not found"), "{again}");
}

#[test]
fn run_end_counts_fix_commits_as_repaired() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let reviewed = env.commit_all("change");
    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    env.reviewed_pass(&run, "full");
    assert_eq!(env.run(&["status"])["reviews_at_head"], 1);
    env.write("a.txt", &numbered(10, &[(3, "three"), (4, "four")]));
    let fix = env.commit_all("fix");
    let ended = env.run(&["run", "end", "--run", &run]);
    assert_eq!(ended["repaired_head"], fix);
    let status = env.run(&["status"]);
    let matched: Vec<(&str, &str)> = status["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|commit| {
            (
                commit["sha"].as_str().unwrap(),
                commit["match"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        matched,
        [(reviewed.as_str(), "pass"), (fix.as_str(), "repair")]
    );
    assert_eq!(status["unreviewed_commits"], serde_json::json!([]));
    assert_eq!(status["reviews_at_head"], 0, "a repair never adds a review");
    assert_eq!(status["runs"][0]["repaired_head"], fix);
}

#[test]
fn list_cycle_selects_one_cycle() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let finding =
        |text: &str| format!(r#"{{"type":"nit","path":"a.txt","line":3,"scenario":"{text}"}}"#);
    start_pass(&env, &run);
    add_findings(&env, &run, "c1p0", &finding("Cycle one."));
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    start_pass(&env, &run);
    add_findings(&env, &run, "c2p0", &finding("Cycle two."));
    add_findings(
        &env,
        &run,
        "c2p0",
        r#"{"type":"nit","scenario":"Killed in two.","killed":true,"reason":"wrong"}"#,
    );
    env.run(&["pass", "end", "--run", &run, "--pass", "c2p0"]);

    let listed = env.run(&["list", "--run", &run, "--cycle", "2"]);
    let ids: Vec<&str> = listed["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [format!("{run}.c2p0.r2.f1").as_str()]);
    assert_eq!(listed["killed"][0]["id"], format!("{run}.c2p0.r2.f2"));
    let prompt = env.text(&["list", "--run", &run, "--cycle", "2", "--format", "prompt"]);
    assert!(prompt.contains("Cycle two."), "{prompt}");
    assert!(prompt.contains("general: Killed in two."), "{prompt}");
    assert!(!prompt.contains("Cycle one."), "{prompt}");
    let none = env.run(&["list", "--run", &run, "--cycle", "3"]);
    assert_eq!(none["threads"], serde_json::json!([]));
}
