//! The global `-C, --directory <DIR>` flag: run as if started in DIR.

use std::fs;
use std::process::Output;

use crate::support::Env;

/// A directory outside any repository, where only `-C` can find the repository.
fn elsewhere(env: &Env) -> std::path::PathBuf {
    let dir = env.path("elsewhere");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn text(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn repo_with_commit() -> Env {
    let env = Env::new();
    env.write("a.txt", "a\n");
    env.commit_all("base");
    env.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "feat",
        env.path("trees/feat").to_str().unwrap(),
    ]);
    env
}

#[test]
fn the_flag_comes_before_or_after_the_subcommand() {
    let env = repo_with_commit();
    let repo = env.repo.to_str().unwrap();
    let inside = text(
        &env.bin(&env.repo)
            .args(["worktree", "list"])
            .output()
            .unwrap(),
    );
    assert!(inside.contains("feat"), "{inside}");
    let outside = elsewhere(&env);
    for args in [
        vec!["-C", repo, "worktree", "list"],
        vec!["worktree", "list", "-C", repo],
        vec!["--directory", repo, "worktree", "list"],
        vec!["worktree", "-C", repo, "list"],
    ] {
        let output = env.bin(&outside).args(&args).output().unwrap();
        assert_eq!(text(&output), inside, "{args:?}");
    }
    let without = env
        .bin(&outside)
        .args(["worktree", "list"])
        .output()
        .unwrap();
    assert!(!without.status.success());
}

#[test]
fn a_missing_directory_is_an_error() {
    let env = repo_with_commit();
    let missing = env.path("nope");
    let output = env
        .bin(&env.repo)
        .args(["-C", missing.to_str().unwrap(), "worktree", "list"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("cannot change to"), "{error}");
    assert!(error.contains("nope"), "{error}");
    assert_eq!(output.stdout, b"");
}

#[test]
fn relative_paths_resolve_against_the_directory() {
    let env = repo_with_commit();
    let token = format!("{}{}", "ghp_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
    fs::write(env.repo.join("leak.txt"), format!("token: {token}\n")).unwrap();
    fs::write(env.repo.join("clean.txt"), "clean\n").unwrap();
    let outside = elsewhere(&env);
    let scan = |file: &str| {
        env.bin(&outside)
            .args(["secrets", "-C", env.repo.to_str().unwrap(), file])
            .output()
            .unwrap()
    };
    assert_eq!(scan("leak.txt").status.code(), Some(1));
    assert_eq!(scan("clean.txt").status.code(), Some(0));
}

#[test]
fn a_relative_directory_resolves_against_the_working_directory() {
    let env = repo_with_commit();
    let outside = elsewhere(&env);
    let link = env.path("elsewhere/inner");
    std::os::unix::fs::symlink(&env.repo, &link).unwrap();
    let output = env
        .bin(&outside)
        .args(["-C", "inner", "worktree", "list"])
        .output()
        .unwrap();
    assert!(text(&output).contains("feat"));
}

#[test]
fn wait_runs_its_check_in_the_directory_and_leaves_arguments_after_the_separator_alone() {
    let env = repo_with_commit();
    let outside = elsewhere(&env);
    let wait = |args: &[&str]| {
        env.bin(&outside)
            .args(["wait", "--timeout", "1", "--interval", "1"])
            .args(args)
            .output()
            .unwrap()
            .status
            .code()
    };
    let dir = env.repo.to_str().unwrap();
    assert_eq!(wait(&["-C", dir, "--", "test", "-e", "a.txt"]), Some(0));
    assert_eq!(wait(&["--", "test", "-e", "a.txt"]), Some(3));
    // `-C` after `--` belongs to the check, not to agt.
    assert_eq!(
        wait(&["-C", dir, "--", "sh", "-c", r#"test "$1" = -C"#, "x", "-C"]),
        Some(0)
    );
}
