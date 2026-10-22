//! `agt check run`: a pass is cached per git tree, name and command; failures and dirty trees
//! always run.

use serde_json::Value;

use crate::support::Env;

/// A repository with one commit and a counter script `bump`, which appends a line to `ran`.
fn setup() -> Env {
    let env = Env::new();
    env.write("a.txt", "a\n");
    env.commit_all("init");
    env
}

fn check(env: &Env, name: &str, extra: &[&str], command: &[&str]) -> (Option<i32>, Value, String) {
    let output = env
        .bin(&env.repo)
        .args(["check", "run", "--name", name])
        .args(extra)
        .arg("--")
        .args(command)
        .output()
        .expect("agt runs");
    let json = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is JSON: {error}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    (
        output.status.code(),
        json,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn runs(env: &Env) -> usize {
    std::fs::read_to_string(env.path("ran")).map_or(0, |text| text.lines().count())
}

fn bump(env: &Env) -> Vec<String> {
    let script = format!("echo x >> {}", env.path("ran").display());
    vec!["sh".into(), "-c".into(), script]
}

fn run_bump(env: &Env, name: &str, extra: &[&str]) -> (Option<i32>, Value, String) {
    let command = bump(env);
    let refs: Vec<&str> = command.iter().map(String::as_str).collect();
    check(env, name, extra, &refs)
}

#[test]
fn the_second_run_on_the_same_tree_hits_without_running() {
    let env = setup();
    let (code, first, _) = run_bump(&env, "test", &[]);
    assert_eq!((code, runs(&env)), (Some(0), 1));
    assert_eq!(first["cached"], false);
    assert_eq!(first["recorded"], true);
    let log = first["log"].as_str().unwrap();
    assert!(std::path::Path::new(log).exists());

    let (code, second, _) = run_bump(&env, "test", &[]);
    assert_eq!((code, runs(&env)), (Some(0), 1));
    assert_eq!(second["cached"], true);
    assert_eq!(second["key"], first["key"]);
    assert_eq!(second["ran_at"], first["ran_at"]);
    assert_eq!(second["log"], first["log"]);
}

#[test]
fn output_goes_to_stderr_and_the_log() {
    let env = setup();
    let (_, summary, stderr) = check(&env, "echo", &[], &["sh", "-c", "echo out; echo err >&2"]);
    assert!(stderr.contains("out") && stderr.contains("err"), "{stderr}");
    let log = std::fs::read_to_string(summary["log"].as_str().unwrap()).unwrap();
    assert!(log.contains("out") && log.contains("err"), "{log}");
}

#[test]
fn a_failure_exits_with_its_code_and_is_not_cached() {
    let env = setup();
    let script = format!("echo x >> {}; exit 7", env.path("ran").display());
    for _ in 0..2 {
        let (code, summary, _) = check(&env, "fail", &[], &["sh", "-c", &script]);
        assert_eq!(code, Some(7));
        assert_eq!(summary["exit"], 7);
        assert_eq!(summary["cached"], false);
        assert_eq!(summary["recorded"], false);
    }
    assert_eq!(runs(&env), 2);
}

#[test]
fn a_dirty_tree_always_runs_and_records_nothing() {
    let env = setup();
    env.write("a.txt", "changed\n");
    for expected in 1..=2 {
        let (code, summary, _) = run_bump(&env, "test", &[]);
        assert_eq!((code, runs(&env)), (Some(0), expected));
        assert_eq!(summary["dirty"], true);
        assert_eq!(summary["recorded"], false);
    }
    env.git(&["checkout", "--", "a.txt"]);
    let (_, summary, _) = run_bump(&env, "test", &[]);
    assert_eq!(
        summary["cached"], false,
        "no entry was recorded while dirty"
    );
    assert_eq!(runs(&env), 3);
}

#[test]
fn untracked_files_are_dirty_unless_ignored() {
    let env = setup();
    env.write("new.txt", "n\n");
    let (_, summary, _) = run_bump(&env, "test", &[]);
    assert_eq!(summary["dirty"], true);
    std::fs::remove_file(env.repo.join("new.txt")).unwrap();
    env.write(".gitignore", "ignored.txt\n");
    env.commit_all("ignore");
    env.write("ignored.txt", "i\n");
    let (_, first, _) = run_bump(&env, "test", &[]);
    assert_eq!(
        (first["dirty"].clone(), first["recorded"].clone()),
        (false.into(), true.into())
    );
    let (_, second, _) = run_bump(&env, "test", &[]);
    assert_eq!(second["cached"], true);
}

#[test]
fn force_reruns_on_a_hit_and_records_a_new_pass() {
    let env = setup();
    let (_, first, _) = run_bump(&env, "test", &[]);
    let (_, forced, _) = run_bump(&env, "test", &["--force"]);
    assert_eq!(
        (forced["cached"].clone(), forced["recorded"].clone()),
        (false.into(), true.into())
    );
    assert_eq!(runs(&env), 2);
    assert_ne!(forced["log"], first["log"]);
    let (_, hit, _) = run_bump(&env, "test", &[]);
    assert_eq!(hit["cached"], true);
    assert_eq!(hit["log"], forced["log"]);
    assert_eq!(runs(&env), 2);
}

#[test]
fn a_different_name_or_command_misses() {
    let env = setup();
    run_bump(&env, "test", &[]);
    let (_, other_name, _) = run_bump(&env, "lint", &[]);
    assert_eq!(other_name["cached"], false);
    let script = format!("echo x >> {}; true", env.path("ran").display());
    let (_, other_command, _) = check(&env, "test", &[], &["sh", "-c", &script]);
    assert_eq!(other_command["cached"], false);
    assert_eq!(runs(&env), 3);
}

#[test]
fn a_new_commit_with_the_same_tree_hits_and_a_new_tree_misses() {
    let env = setup();
    run_bump(&env, "test", &[]);
    env.git(&["commit", "-q", "--allow-empty", "-m", "empty"]);
    let (_, same, _) = run_bump(&env, "test", &[]);
    assert_eq!(same["cached"], true);
    env.write("a.txt", "b\n");
    env.commit_all("change");
    let (_, changed, _) = run_bump(&env, "test", &[]);
    assert_eq!(changed["cached"], false);
    assert_eq!(runs(&env), 2);
}

#[test]
fn a_linked_worktree_shares_the_cache() {
    let env = setup();
    run_bump(&env, "test", &[]);
    let linked = env.path("linked");
    env.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "other",
        linked.to_str().unwrap(),
    ]);
    let command = bump(&env);
    let output = env
        .bin(&linked)
        .args(["check", "run", "--name", "test", "--"])
        .args(&command)
        .output()
        .unwrap();
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["cached"], true);
    assert_eq!(runs(&env), 1);
}
