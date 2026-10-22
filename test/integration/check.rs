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

#[test]
fn a_command_that_edits_a_tracked_file_is_not_recorded() {
    let env = setup();
    let edit = format!("echo edit >> {}/a.txt", env.repo.display());
    let (code, first, _) = check(&env, "test", &[], &["sh", "-c", &edit]);
    assert_eq!(code, Some(0));
    assert_eq!(first["recorded"], false);
    assert_eq!(
        first["reason"],
        "the work tree or HEAD changed during the run"
    );
    env.git(&["checkout", "--", "a.txt"]);
    let (_, second, _) = run_bump(&env, "test", &[]);
    assert_eq!(second["cached"], false, "no entry was recorded");
}

#[test]
fn a_command_that_commits_is_not_recorded() {
    let env = setup();
    let git = format!(
        "git -C {} commit -q --allow-empty -m moved && echo y > {}/b.txt && git -C {} add b.txt && git -C {} commit -q -m b",
        env.repo.display(),
        env.repo.display(),
        env.repo.display(),
        env.repo.display()
    );
    let (code, summary, _) = check(&env, "test", &[], &["sh", "-c", &git]);
    assert_eq!(code, Some(0));
    assert_eq!(summary["recorded"], false);
    assert!(summary["reason"].is_string());
}

#[test]
fn a_background_process_holding_the_pipes_does_not_hang_the_run() {
    let env = setup();
    let started = std::time::Instant::now();
    let (code, summary, _) = check(&env, "test", &[], &["sh", "-c", "sleep 30 & exit 0"]);
    assert_eq!(code, Some(0));
    assert_eq!(summary["recorded"], true);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn untracked_files_are_dirty_even_when_git_config_hides_them() {
    let env = setup();
    env.git(&["config", "status.showUntrackedFiles", "no"]);
    env.write("new.txt", "n\n");
    let (_, summary, _) = run_bump(&env, "test", &[]);
    assert_eq!(summary["dirty"], true);
    assert_eq!(summary["recorded"], false);
}

#[test]
fn a_store_that_cannot_open_runs_uncached_with_a_warning() {
    let env = setup();
    let bare = env.path("bare.git");
    env.git(&["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
    let linked = env.path("bare-linked");
    let added = env
        .command("git", &bare)
        .args([
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            linked.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(added.success());
    let command = bump(&env);
    let output = env
        .bin(&linked)
        .args(["check", "run", "--name", "test", "--"])
        .args(&command)
        .output()
        .unwrap();
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{json}");
    assert_eq!(
        (json["cached"].clone(), json["recorded"].clone()),
        (false.into(), false.into())
    );
    assert!(
        json["warnings"][0]
            .as_str()
            .unwrap()
            .contains("running uncached"),
        "{json}"
    );
    assert_eq!(runs(&env), 1);
}

#[test]
fn cleanup_drops_entries_older_than_the_output_logs_and_their_locks() {
    let env = setup();
    let (_, first, _) = run_bump(&env, "test", &[]);
    let key = first["key"].as_str().unwrap();
    let dir = env.repo.join(".agt").join("checks");
    assert!(dir.join(format!("{key}.json")).exists());
    let cleanup = |now: u64, args: &[&str]| {
        let output = env
            .bin(&env.repo)
            .env("AGT_NOW", now.to_string())
            .arg("cleanup")
            .args(["--only", "checks"])
            .args(args)
            .output()
            .unwrap();
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let day = 86_400;
    let young = cleanup(crate::support::unix_now() + 6 * day, &["--yes"]);
    assert_eq!(young["totals"]["items"], 0, "{young}");
    assert!(dir.join(format!("{key}.json")).exists());
    let corrupt = dir.join(format!("{}.json", "0".repeat(64)));
    std::fs::write(&corrupt, "{ nope").unwrap();
    let old = cleanup(crate::support::unix_now() + 8 * day, &["--yes"]);
    assert_eq!(old["totals"]["items"], 1, "{old}");
    assert!(!dir.join(format!("{key}.json")).exists());
    assert!(!dir.join(format!(".{key}.lock")).exists());
    assert!(corrupt.exists());
    assert!(
        old["warnings"][0].as_str().unwrap().ends_with("; kept"),
        "{old}"
    );
}

fn key_file(env: &Env, key: &str) -> std::path::PathBuf {
    env.repo
        .join(".agt")
        .join("checks")
        .join(format!("{key}.json"))
}

#[test]
fn a_corrupt_entry_is_removed_and_replaced_by_the_new_pass() {
    let env = setup();
    let (_, first, _) = run_bump(&env, "test", &[]);
    let file = key_file(&env, first["key"].as_str().unwrap());
    for extra in [&[][..], &["--force"][..]] {
        std::fs::write(&file, "{ nope").unwrap();
        let before = runs(&env);
        let (code, summary, _) = run_bump(&env, "test", extra);
        assert_eq!((code, runs(&env)), (Some(0), before + 1), "{summary}");
        assert_eq!(summary["cached"], false);
        assert_eq!(summary["recorded"], true, "{summary}");
        assert!(
            summary["warnings"][0]
                .as_str()
                .unwrap()
                .starts_with("removed the unreadable cached pass"),
            "{summary}"
        );
        let (_, hit, _) = run_bump(&env, "test", &[]);
        assert_eq!(
            (hit["cached"].clone(), runs(&env)),
            (true.into(), before + 1)
        );
    }
}

#[test]
fn an_entry_from_a_newer_agt_is_kept_and_runs_uncached() {
    let env = setup();
    let (_, first, _) = run_bump(&env, "test", &[]);
    let file = key_file(&env, first["key"].as_str().unwrap());
    std::fs::write(&file, r#"{"version":2}"#).unwrap();
    for extra in [&[][..], &["--force"][..]] {
        let (code, summary, _) = run_bump(&env, "test", extra);
        assert_eq!(code, Some(0), "{summary}");
        assert_eq!(summary["recorded"], false, "{summary}");
        assert!(
            summary["warnings"][0]
                .as_str()
                .unwrap()
                .starts_with("ignoring the cached pass"),
            "{summary}"
        );
    }
    assert_eq!(runs(&env), 3);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), r#"{"version":2}"#);
}

#[test]
fn an_entry_that_cannot_be_removed_or_written_is_not_an_error() {
    let env = setup();
    let (_, first, _) = run_bump(&env, "test", &[]);
    let file = key_file(&env, first["key"].as_str().unwrap());
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    let (code, summary, _) = run_bump(&env, "test", &[]);
    assert_eq!((code, runs(&env)), (Some(0), 2), "{summary}");
    assert_eq!(summary["recorded"], false, "{summary}");
    let warnings = summary["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 2, "{summary}");
    assert!(
        warnings[0].as_str().unwrap().contains("removing it failed"),
        "{summary}"
    );
}

#[test]
fn a_hit_whose_log_is_gone_reruns() {
    let env = setup();
    let (_, first, _) = run_bump(&env, "test", &[]);
    std::fs::remove_file(first["log"].as_str().unwrap()).unwrap();
    let (code, second, _) = run_bump(&env, "test", &[]);
    assert_eq!((code, runs(&env)), (Some(0), 2));
    assert_eq!(second["cached"], false);
    assert_eq!(second["recorded"], true);
    assert!(std::path::Path::new(second["log"].as_str().unwrap()).exists());
}

#[test]
fn a_modified_submodule_is_dirty_even_when_git_config_ignores_submodules() {
    let env = setup();
    let source = env.path("subsrc");
    std::fs::create_dir_all(&source).unwrap();
    let sub = |dir: &std::path::Path, args: &[&str]| {
        let status = env
            .command("git", dir)
            .args(["-c", "protocol.file.allow=always"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    sub(&source, &["init", "-q", "-b", "main"]);
    std::fs::write(source.join("lib.txt"), "lib\n").unwrap();
    sub(&source, &["add", "-A"]);
    sub(&source, &["commit", "-q", "-m", "lib"]);
    sub(
        &env.repo,
        &["submodule", "add", "-q", &source.to_string_lossy(), "lib"],
    );
    env.git(&["commit", "-q", "-m", "add submodule"]);
    env.git(&["config", "diff.ignoreSubmodules", "all"]);
    std::fs::write(env.repo.join("lib/lib.txt"), "changed\n").unwrap();
    let (_, summary, _) = run_bump(&env, "test", &[]);
    assert_eq!(summary["dirty"], true, "{summary}");
    assert_eq!(summary["recorded"], false);
}
