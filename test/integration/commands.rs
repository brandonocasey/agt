use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;

use crate::support::{Env, numbered, snapshot};

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
    assert_eq!(pass["commit_count"], 1);
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
        assert!(error.starts_with("agt: "), "{error}");
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
    assert_eq!(pass["pass"]["commit_count"], 2);

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
    let detached = env.fail(&["status", "--full"], None);
    assert!(detached.contains("HEAD is detached"), "{detached}");
}

#[test]
fn errors_outside_a_repository() {
    let env = Env::new();
    let output = env.agt(&env.path("home")).arg("status").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "agt: error: not inside a git work tree\n"
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
fn foreign_documents_in_the_store_are_ignored() {
    let env = Env::with_branch();
    env.git(&["remote", "add", "origin", "git@github.com:Owner/Repo.git"]);
    let key = env.run(&["comment", "--body", "x"])["target"]
        .as_str()
        .unwrap()
        .to_owned();
    let dir = env.state().join("github.com").join("Owner").join("Repo");
    fs::write(
        dir.join("issue-gh-7.json"),
        r#"{"version":1,"kind":"issue","key":"issue-gh-7","threads":[]}"#,
    )
    .unwrap();
    fs::create_dir(dir.join("issues.json")).unwrap();
    fs::create_dir(dir.join("issues")).unwrap();
    fs::write(
        dir.join("issues").join("issue-gh-8.json"),
        r#"{"version":1,"kind":"issue"}"#,
    )
    .unwrap();
    assert_eq!(env.run(&["status"])["target"], key);
    assert_eq!(env.run(&["list"])["threads"].as_array().unwrap().len(), 1);
    env.run(&["prune", "--all"]);
    assert!(dir.join("issue-gh-7.json").exists());
    assert!(dir.join("issues").join("issue-gh-8.json").exists());
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
            env.agt(&env.repo)
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
        .find(|path| {
            path.extension().is_some_and(|ext| ext == "json")
                && !path.file_name().unwrap().to_string_lossy().starts_with('.')
        })
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

    let kept = env.run_with(&["prune"], None, &[("AGT_NOW", &soon)]);
    assert_eq!(kept["pruned"], serde_json::json!([]));
    let dry = env.run_with(&["prune", "--dry-run"], None, &[("AGT_NOW", &later)]);
    assert_eq!(dry["pruned"].as_array().unwrap().len(), 1);
    assert_eq!(env.run(&["list"])["threads"].as_array().unwrap().len(), 1);

    let pruned = env.run_with(&["prune"], None, &[("AGT_NOW", &later)]);
    assert_eq!(pruned["pruned"].as_array().unwrap().len(), 1);
    assert_eq!(env.run(&["list"])["threads"], serde_json::json!([]));

    env.run(&["comment", "--body", "again"]);
    let output = env
        .agt(&env.path("home"))
        .args(["prune", "--all"])
        .env("AGT_NOW", &later)
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
    let later_env = [("AGT_NOW", later.as_str())];
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
    assert_eq!(env.run(&["status", "--full"])["reviews_at_head"], 1);
    env.write("a.txt", &numbered(10, &[(3, "three"), (4, "four")]));
    let fix = env.commit_all("fix");
    let ended = env.run(&["run", "end", "--run", &run]);
    assert_eq!(ended["repaired_head"], fix);
    let status = env.run(&["status", "--full"]);
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

#[test]
fn long_branch_names_get_short_keys() {
    let env = Env::with_branch();
    let branch = format!("feature/{}", "very-long-name-".repeat(12));
    env.git(&["switch", "-q", "-c", &branch]);
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let first = env.run(&["run", "start"]);
    let key = first["target"].as_str().unwrap().to_owned();
    assert!(key.len() <= 200, "{key}");
    assert_eq!(
        env.run(&["run", "start"])["target"],
        key,
        "the key is stable"
    );
}

#[test]
fn manual_and_fix_reviewer_ids() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let reviewers = r#"[{"id":"manual","model":"m1","effort":"high","harness":"claude-code"},
      {"id":"fix","model":"m2","effort":"low","harness":"codex","harness_version":"0.162.1"}]"#;
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
        reviewers,
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
            "manual",
        ],
        r#"{"type":"nit","scenario":"Manual pass finding."}"#,
    );
    let id = format!("{run}.c1p0.manual.f1");
    let replied = env.run(&[
        "reply",
        &id,
        "--body",
        "Fixed in abc.",
        "--run",
        &run,
        "--pass",
        "c1p0",
        "--reviewer",
        "fix",
    ]);
    let author = &replied["thread"]["comments"][1]["author"];
    assert_eq!(
        (&author["model"], &author["harness"]),
        (&serde_json::json!("m2"), &serde_json::json!("codex"))
    );
}

#[test]
fn target_branch_missing_without_pr_errors() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let key = env.run(&["run", "start"])["target"]
        .as_str()
        .unwrap()
        .to_owned();
    env.git(&["switch", "-q", "--detach", "main"]);
    env.git(&["branch", "-q", "-D", "feat"]);
    let error = env.fail(&["--target", &key, "run", "start"], None);
    assert!(error.contains("branch feat"), "{error}");
}

/// A branch target whose branch was deleted, after one reviewed pass with an anchored finding.
fn orphaned_target(with_pass: bool) -> (Env, String, String) {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let head = env.commit_all("change");
    env.run(&[
        "comment",
        "--body",
        "Human note",
        "--path",
        "a.txt",
        "--line",
        "3",
    ]);
    if with_pass {
        let run = env.run(&["run", "start"])["run"]
            .as_str()
            .unwrap()
            .to_owned();
        start_pass(&env, &run);
        add_findings(
            &env,
            &run,
            "c1p0",
            r#"{"type":"nit","path":"a.txt","line":3,"scenario":"Anchored."}"#,
        );
        env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    }
    let key = env.run(&["status"])["target"].as_str().unwrap().to_owned();
    env.git(&["switch", "-q", "--detach", "main"]);
    env.git(&["branch", "-q", "-D", "feat"]);
    (env, key, head)
}

#[test]
fn read_only_commands_use_the_last_pass_head() {
    let (env, key, head) = orphaned_target(true);
    let status = env.run(&["--target", &key, "status"]);
    assert_eq!(status["head"], head);
    let list = env.run(&["--target", &key, "list"]);
    assert_eq!(list["threads"][0]["current"]["line"], 3);
    env.run(&["--target", &key, "show", "h1"]);
    let error = env.fail(&["--target", &key, "comment", "--body", "x"], None);
    assert!(error.contains("branch feat"), "{error}");
}

#[test]
fn read_only_commands_without_any_head() {
    let (env, key, _) = orphaned_target(false);
    let status = env.run(&["--target", &key, "status"]);
    assert_eq!(status["head"], serde_json::Value::Null);
    assert_eq!(
        status["open_threads"][0]["current"],
        serde_json::Value::Null
    );
    let list = env.run(&["--target", &key, "list"]);
    assert_eq!(list["threads"][0]["current"], serde_json::Value::Null);
    let shown = env.run(&["--target", &key, "show", "h1"]);
    assert_eq!(shown["thread"]["current"], serde_json::Value::Null);
}

#[test]
fn target_on_detached_review_worktree_uses_head() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let key = env.run(&["run", "start"])["target"]
        .as_str()
        .unwrap()
        .to_owned();
    env.git(&["switch", "-q", "--detach"]);
    env.write("a.txt", &numbered(10, &[(3, "three"), (5, "five")]));
    let fix = env.commit_all("fix on detached head");
    assert_eq!(env.run(&["--target", &key, "status"])["head"], fix);
    env.git(&["switch", "-q", "--detach", "main"]);
    assert_ne!(
        env.run(&["--target", &key, "status"])["head"],
        env.git(&["rev-parse", "main"]),
        "an unrelated detached HEAD is not the target's"
    );
}

#[test]
fn target_elsewhere_uses_the_head_tree() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let head = env.commit_all("change");
    let started = env.run(&["run", "start"]);
    let key = started["target"].as_str().unwrap().to_owned();
    let run = started["run"].as_str().unwrap().to_owned();
    env.git(&["switch", "-q", "main"]);
    env.write("a.txt", "uncommitted on main\n");
    let pass = env.run(&[
        "--target", &key, "pass", "start", "--run", &run, "--kind", "full",
    ]);
    assert_eq!(pass["pass"]["dirty"], false);
    assert_eq!(pass["pass"]["head"], head);
    assert_eq!(
        pass["pass"]["tree"],
        env.git(&["rev-parse", &format!("{head}^{{tree}}")])
    );
    let comment = env.run(&[
        "--target", &key, "comment", "--body", "Note", "--path", "a.txt", "--line", "3",
    ]);
    assert_eq!(comment["thread"]["anchor"]["commit"], head);
}

#[test]
fn range_fix_check_and_repair_use_a_descendant_head() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let end = env.commit_all("reviewed range end");
    let range = format!("main..{end}");
    let run = env.run(&["--range", &range, "run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    let full = [
        "--range", &range, "pass", "start", "--run", &run, "--kind", "full",
    ];
    env.run(&full);
    env.run(&[
        "--range", &range, "pass", "end", "--run", &run, "--pass", "c1p0",
    ]);
    env.write("a.txt", &numbered(10, &[(3, "THREE")]));
    let fix = env.commit_all("fix");

    let unrelated = env.fail(
        &[
            "--range",
            &range,
            "pass",
            "start",
            "--run",
            &run,
            "--kind",
            "fix-check",
            "--head",
            "main",
        ],
        None,
    );
    assert!(unrelated.contains("does not descend from"), "{unrelated}");
    let check = env.run(&[
        "--range",
        &range,
        "pass",
        "start",
        "--run",
        &run,
        "--kind",
        "fix-check",
        "--head",
        &fix,
    ]);
    assert_eq!(check["pass"]["head"], fix, "{check}");
    assert_eq!(check["pass"]["base"], end);
    env.run(&[
        "--range", &range, "pass", "end", "--run", &run, "--pass", "c1p1",
    ]);
    env.write("a.txt", &numbered(10, &[(3, "THREE"), (9, "nine")]));
    let repair = env.commit_all("repair");
    let ended = env.run(&[
        "--range", &range, "run", "end", "--run", &run, "--head", &repair,
    ]);
    assert_eq!(ended["repaired_head"], repair, "{ended}");
}

#[test]
fn list_head_maps_anchors_to_a_later_head() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let end = env.commit_all("range end");
    let range = format!("main..{end}");
    env.run(&[
        "--range",
        &range,
        "comment",
        "--body",
        "Why three?",
        "--path",
        "a.txt",
        "--line",
        "3",
        "--commit",
        &end,
    ]);
    env.write("a.txt", &numbered(10, &[(1, "one"), (3, "THREE")]));
    let fix = env.commit_all("fix");
    let at_end = env.run(&["--range", &range, "list"]);
    assert_eq!(
        at_end["threads"][0]["current"]["outdated"], false,
        "{at_end}"
    );
    let at_fix = env.run(&["--range", &range, "list", "--head", &fix]);
    assert_eq!(
        at_fix["threads"][0]["current"]["outdated"], true,
        "{at_fix}"
    );
}

#[test]
fn batched_review_records_in_few_calls() {
    let env = Env::with_branch();
    let main = env.git(&["rev-parse", "main"]);
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let reviewers = r#"[{"model":"a","harness":"codex"},{"model":"b","harness":"claude-code"}]"#;
    let started = env.run_stdin(
        &[
            "run",
            "start",
            "--kind",
            "full",
            "--base",
            &main,
            "--reviewers",
            "-",
        ],
        reviewers,
    );
    let run = started["run"].as_str().unwrap().to_owned();
    assert_eq!(started["pass"]["id"], "c1p0", "{started}");
    assert_eq!(started["pass"]["commit_count"], 1);
    assert!(started["pass"].get("commits").is_none(), "{started}");

    let findings = r#"[
        {"reviewer":"r2","type":"bug","severity":"high","path":"a.txt","line":3,"scenario":"broken"},
        {"type":"nit","path":"a.txt","line":3,"scenario":"style","killed":true,"reason":"taste"},
        {"reviewer":"r2","type":"bug","severity":"low","path":"a.txt","line":3,"scenario":"also"}
    ]"#;
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
        findings,
    );
    let f1 = format!("{run}.c1p0.r2.f1");
    let f2 = format!("{run}.c1p0.r2.f2");
    assert_eq!(ended["added"], serde_json::json!([f1, f2]), "{ended}");
    assert_eq!(
        ended["killed"],
        serde_json::json!([format!("{run}.c1p0.r1.f1")])
    );
    assert_eq!(ended["pass"]["major_found"], true);
    assert!(ended["pass"]["ended_at"].is_string());

    // Fix both in one call, signed by the fixer.
    env.write("a.txt", &numbered(10, &[(3, "fixed")]));
    let fix = env.commit_all("fix");
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
        r#"[{"id":"r2"},{"id":"fix","model":"m","harness":"claude-code"}]"#,
    );
    let fixed = env.run(&[
        "resolve",
        &f1,
        &f2,
        "--fixed",
        &fix,
        "--run",
        &run,
        "--pass",
        "c1p1",
        "--reviewer",
        "fix",
    ]);
    assert_eq!(fixed["threads"][0]["resolved"], true, "{fixed}");
    assert_eq!(fixed["threads"][1]["resolution"], format!("fixed {fix}"));
    let shown = env.run(&["show", &f1]);
    let reply = &shown["thread"]["comments"][1];
    assert_eq!(
        reply["body"],
        format!("✅ Fixed in {} fix.", &fix[..7]),
        "{shown}"
    );
    assert_eq!(reply["author"]["reviewer"], "fix");

    // The fix check finds one still broken.
    let reopened = env.run(&[
        "resolve",
        &f2,
        "--not-fixed",
        "still off by one",
        "--run",
        &run,
        "--pass",
        "c1p1",
        "--reviewer",
        "r2",
    ]);
    assert_eq!(reopened["thread"]["resolved"], false, "{reopened}");
    assert_eq!(
        reopened["thread"]["resolution"],
        "not fixed: still off by one"
    );
    assert_eq!(
        reopened["thread"]["comments"][2]["body"],
        "❌ Not fixed: still off by one."
    );
    env.run_stdin(
        &[
            "pass",
            "end",
            "--run",
            &run,
            "--pass",
            "c1p1",
            "--findings",
            "-",
        ],
        "[]",
    );

    let ended = env.run(&["run", "end", "--run", &run]);
    assert_eq!(ended["status"]["reviewed"], true, "{ended}");
    assert_eq!(ended["status"]["open_threads"][0]["id"], f2.as_str());
    assert!(ended["status"].get("commits").is_none());
}

#[test]
fn wrong_ids_name_the_valid_ones() {
    let env = Env::with_branch();
    let run = env.review("full");
    let error = env.fail(&["pass", "end", "--run", &run, "--pass", "1"], None);
    assert!(error.contains("its passes: c1p0"), "{error}");
    let error = env.fail(&["pass", "end", "--run", "rv-zzzz", "--pass", "c1p0"], None);
    assert!(error.contains("open runs: none"), "{error}");
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
    let error = env.fail(
        &[
            "finding",
            "add",
            "--run",
            &run,
            "--pass",
            "c1p0",
            "--reviewer",
            "reviewer-1",
        ],
        Some(FINDINGS),
    );
    assert!(error.contains("its reviewers: r1"), "{error}");
}

#[test]
fn status_is_brief_and_compact_unless_asked() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    env.review("full");
    let brief = env.run(&["status"]);
    assert_eq!(brief["reviewed"], true);
    for field in ["commits", "runs", "head_tree"] {
        assert!(brief.get(field).is_none(), "{field} in {brief}");
    }
    let full = env.run(&["status", "--full"]);
    assert_eq!(full["commits"].as_array().unwrap().len(), 1);
    assert_eq!(full["runs"].as_array().unwrap().len(), 1);
    // Agents read a pipe, so the JSON is one line.
    assert_eq!(env.text(&["status"]).lines().count(), 1);
}

#[test]
fn run_id_finds_its_range_target() {
    let env = Env::with_branch();
    let main = env.git(&["rev-parse", "main"]);
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let a = env.commit_all("A");
    let range = format!("{main}..{a}");
    let started = env.run_stdin(
        &[
            "--range",
            &range,
            "run",
            "start",
            "--kind",
            "full",
            "--reviewers",
            "-",
        ],
        REVIEWER,
    );
    let run = started["run"].as_str().unwrap().to_owned();
    let key = started["target"].clone();
    // No --range from here on: the run id names the target.
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
        r#"[{"type":"bug","severity":"low","path":"a.txt","line":3,"scenario":"x"}]"#,
    );
    assert_eq!(ended["target"], key, "{ended}");
    let id = ended["added"][0].as_str().unwrap().to_owned();
    assert_eq!(env.run(&["show", &id])["target"], key);
    let resolved = env.run(&["resolve", &id, "--triage", "stale"]);
    assert_eq!(resolved["target"], key);
    let ended = env.run(&["run", "end", "--run", &run]);
    assert_eq!(ended["target"], key, "{ended}");
    let error = env.fail(&["pass", "end", "--run", "rv-zzzz", "--pass", "c1p0"], None);
    assert!(error.starts_with("agt: error: "), "{error}");
}

#[test]
fn run_lookup_never_picks_another_branch_target() {
    let env = Env::with_branch();
    env.git(&["switch", "-q", "main"]);
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("on main");
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
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    // The fix goes on a new branch; main's target would evaluate main's old tip.
    env.git(&["switch", "-q", "-c", "fix"]);
    env.write("a.txt", &numbered(10, &[(3, "fixed")]));
    env.commit_all("fix");
    let error = env.fail(
        &["pass", "start", "--run", &run, "--kind", "fix-check"],
        None,
    );
    assert!(error.contains(&format!("run {run} not found")), "{error}");
}

#[test]
fn failed_first_pass_leaves_no_run() {
    let env = Env::with_branch();
    env.fail(&["run", "start", "--kind", "fix-check"], None);
    env.fail(
        &["run", "start", "--kind", "full", "--reviewers", "-"],
        Some("not json"),
    );
    let error = env.fail(&["pass", "end", "--run", "rv-zzzz", "--pass", "c1p0"], None);
    assert!(error.contains("open runs: none"), "{error}");
}

#[test]
fn pass_end_lists_ids_in_input_order() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let started = env.run_stdin(
        &["run", "start", "--kind", "full", "--reviewers", "-"],
        r#"[{"model":"a","harness":"codex"},{"id":"manual","model":"m","harness":"claude-code"}]"#,
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
        r#"[
            {"type":"bug","severity":"low","scenario":"A"},
            {"reviewer":"manual","type":"bug","severity":"low","scenario":"B"},
            {"type":"bug","severity":"low","scenario":"C"}
        ]"#,
    );
    let p = format!("{run}.c1p0");
    assert_eq!(
        ended["added"],
        serde_json::json!([
            format!("{p}.r1.f1"),
            format!("{p}.manual.f1"),
            format!("{p}.r1.f2")
        ]),
        "{ended}"
    );
}

#[test]
fn usage_and_time_are_recorded() {
    let env = Env::with_branch();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    let started = env.run_stdin(
        &["run", "start", "--kind", "full", "--reviewers", "-"],
        r#"[{"model":"a","harness":"codex"}]"#,
    );
    let run = started["run"].as_str().unwrap().to_owned();
    env.run_stdin(
        &["pass", "end", "--run", &run, "--pass", "c1p0", "--findings", "-", "--reviewer", "r1"],
        r#"{"findings": [], "usage": [{"reviewer": "r1", "tokens": 52000, "duration_ms": 61000, "tool_uses": 9}]}"#,
    );
    let ended = env.run_stdin(
        &["run", "end", "--run", &run, "--usage", "-"],
        r#"[{"role": "fix", "tokens": 18000, "duration_ms": 40000}]"#,
    );
    let spent = &ended["spent"];
    assert_eq!(spent["reviewer_tokens"], 52000, "{ended}");
    assert_eq!(spent["main_tokens"], 18000);
    assert!(spent["total_secs"].is_i64());
    assert_eq!(spent["roles"][0]["role"], "fix");
    let pass = env.run(&["show", &format!("{run}.c1p0")]);
    assert_eq!(
        pass["pass"]["reviewers"][0]["usage"]["tool_uses"], 9,
        "{pass}"
    );
    let error = env.fail(
        &[
            "pass",
            "end",
            "--run",
            &run,
            "--pass",
            "c1p0",
            "--findings",
            "-",
        ],
        Some(r#"{"findings": [], "usage": [], "extra": 1}"#),
    );
    assert!(error.contains("unknown field extra"), "{error}");
}
