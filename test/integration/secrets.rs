//! `agt secrets`: rows name path, line and rule; the secret itself is never printed.

use std::fs;
use std::process::Output;

use serde_json::{Value, json};

use crate::support::Env;

fn scan(env: &Env, args: &[&str]) -> Output {
    env.bin(&env.repo)
        .arg("secrets")
        .args(args)
        .output()
        .expect("agt runs")
}

fn secret() -> String {
    format!("{}{}", "ghp_", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8")
}

#[test]
fn a_planted_secret_is_reported_without_its_value() {
    let env = Env::new();
    let value = secret();
    fs::write(env.repo.join("a.txt"), format!("ok\ntoken: {value}\n")).unwrap();
    fs::write(env.repo.join("b.txt"), "clean\n").unwrap();
    env.git(&["add", "."]);
    for args in [
        &[][..],
        &["--json"],
        &["--staged"],
        &["--json", "b.txt", "a.txt"],
    ] {
        let output = scan(&env, args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let all = [output.stdout.clone(), output.stderr.clone()].concat();
        let all = String::from_utf8_lossy(&all);
        assert!(!all.contains(&value[4..]) && !all.contains(&value), "{all}");
        assert!(all.contains("a.txt"), "{all}");
    }
    let rows: Value = serde_json::from_slice(&scan(&env, &["--json"]).stdout).unwrap();
    assert_eq!(
        rows,
        json!([{"path": "a.txt", "line": 2, "rule": "github-token"}])
    );
    assert_eq!(
        String::from_utf8_lossy(&scan(&env, &[]).stdout),
        "a.txt:2 github-token\n"
    );
}

#[test]
fn clean_ignored_binary_allowed_and_unstaged_files_do_not_match() {
    let env = Env::new();
    let value = secret();
    fs::write(env.repo.join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(env.repo.join("ignored.txt"), &value).unwrap();
    let mut binary = vec![0_u8];
    binary.extend(value.as_bytes());
    fs::write(env.repo.join("blob.bin"), binary).unwrap();
    fs::write(
        env.repo.join("ok.txt"),
        format!("{value} # agt-secrets: allow\n"),
    )
    .unwrap();
    assert_eq!(scan(&env, &[]).status.code(), Some(0));
    fs::write(env.repo.join("late.txt"), &value).unwrap();
    assert_eq!(scan(&env, &["--staged"]).status.code(), Some(0));
    assert_eq!(scan(&env, &["ok.txt"]).status.code(), Some(0));
    assert_eq!(scan(&env, &["late.txt"]).status.code(), Some(1));
}

#[test]
fn a_missing_path_fails_instead_of_reporting_clean() {
    let env = Env::new();
    fs::write(env.repo.join("a.txt"), format!("token: {}\n", secret())).unwrap();
    env.git(&["add", "."]);
    for args in [&["scan"][..], &["scan", "--staged"][..]] {
        let output = scan(&env, args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("no such path: scan"), "{stderr}");
    }
    // Staged but deleted from disk: the index still holds the blob that would be committed.
    fs::remove_file(env.repo.join("a.txt")).unwrap();
    let output = scan(&env, &["--staged", "a.txt"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("a.txt:1 github-token"));
    // A staged deletion is part of the staged change and has nothing to scan.
    env.git(&["commit", "-qm", "a"]);
    env.git(&["rm", "-q", "--cached", "a.txt"]);
    assert_eq!(scan(&env, &["--staged", "a.txt"]).status.code(), Some(0));
}

#[test]
fn outside_a_repository_it_fails_with_exit_2() {
    let env = Env::new();
    let output = env
        .bin(&env.path("home"))
        .arg("secrets")
        .env("GIT_CEILING_DIRECTORIES", env.path(""))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn staged_submodules_and_colon_names_are_read_by_object_id() {
    let env = Env::new();
    let value = secret();
    fs::write(env.repo.join("x.txt"), "clean\n").unwrap();
    fs::write(env.repo.join("0:x.txt"), format!("token: {value}\n")).unwrap();
    env.git(&["add", "."]);
    let head = "1".repeat(40);
    env.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{head},sub"),
    ]);
    let output = scan(&env, &["--staged", "--json"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let rows: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        rows,
        json!([{"path": "0:x.txt", "line": 1, "rule": "github-token"}])
    );
}

#[cfg(unix)]
#[test]
fn symlinks_are_skipped_in_both_modes() {
    let env = Env::new();
    let fifo = env.path("fifo");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    std::os::unix::fs::symlink(&fifo, env.repo.join("link")).unwrap();
    let outside = env.path("outside.txt");
    fs::write(&outside, secret()).unwrap();
    std::os::unix::fs::symlink(&outside, env.repo.join("out")).unwrap();
    env.git(&["add", "."]);
    for args in [&[][..], &["--staged"]] {
        assert_eq!(scan(&env, args).status.code(), Some(0), "{args:?}");
    }
}

#[test]
fn a_symlink_replaced_by_a_file_is_scanned_when_staged() {
    let env = Env::new();
    fs::write(env.repo.join("example.json"), "{}\n").unwrap();
    std::os::unix::fs::symlink("example.json", env.repo.join("config.json")).unwrap();
    env.commit_all("link");
    fs::remove_file(env.repo.join("config.json")).unwrap();
    fs::write(env.repo.join("config.json"), format!("{}\n", secret())).unwrap();
    env.git(&["add", "config.json"]);
    let output = scan(&env, &["--staged"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "config.json:1 github-token\n"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_paths_are_scanned_in_discovery_and_explicit_arguments() {
    use std::os::unix::ffi::OsStrExt;
    let env = Env::new();
    let name = std::ffi::OsStr::from_bytes(b"token-\xff.txt");
    fs::write(env.repo.join(name), secret()).unwrap();
    env.git(&["add", "."]);
    for staged in [false, true] {
        for explicit in [false, true] {
            let mut cmd = env.bin(&env.repo);
            cmd.arg("secrets");
            if staged {
                cmd.arg("--staged");
            }
            if explicit {
                cmd.arg(name);
            }
            let output = cmd.output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(1),
                "staged={staged} explicit={explicit}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("github-token"));
            assert!(!String::from_utf8_lossy(&output.stdout).contains(&secret()));
        }
    }
    fs::remove_file(env.repo.join(name)).unwrap();
    let output = env
        .bin(&env.repo)
        .args(["secrets", "--staged"])
        .arg(name)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn filesystem_lookup_errors_fail_the_secret_scan() {
    let env = Env::new();
    fs::create_dir(env.repo.join("blocked")).unwrap();
    fs::write(env.repo.join("blocked/file"), "clean").unwrap();
    env.git(&["add", "."]);
    fs::remove_file(env.repo.join("blocked/file")).unwrap();
    fs::remove_dir(env.repo.join("blocked")).unwrap();
    std::os::unix::fs::symlink("blocked", env.repo.join("blocked")).unwrap();
    let output = scan(&env, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("blocked/file"));
}

#[test]
fn missing_tracked_files_are_skipped() {
    let env = Env::new();
    fs::write(env.repo.join("gone"), secret()).unwrap();
    env.git(&["add", "."]);
    fs::remove_file(env.repo.join("gone")).unwrap();
    assert_eq!(scan(&env, &[]).status.code(), Some(0));
}
