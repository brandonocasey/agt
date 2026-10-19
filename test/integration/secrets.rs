//! `agt secrets scan`: rows name path, line and rule; the secret itself is never printed.

use std::fs;
use std::process::Output;

use serde_json::{Value, json};

use crate::support::Env;

fn scan(env: &Env, args: &[&str]) -> Output {
    env.command(env!("CARGO_BIN_EXE_agt"), &env.repo)
        .args(["secrets", "scan"])
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
fn outside_a_repository_it_fails_with_exit_2() {
    let env = Env::new();
    let output = env
        .command(env!("CARGO_BIN_EXE_agt"), &env.path("home"))
        .args(["secrets", "scan"])
        .env("GIT_CEILING_DIRECTORIES", env.path(""))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}
