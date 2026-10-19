//! `agt doctor`: rows, fix hints and exit status, with fake gh/glab/mise and an isolated repo.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use serde_json::Value;

use crate::support::Env;

const AUTH: [&str; 4] = ["auth", "status", "--hostname", "github.com"];

/// A repository with a commit and a GitHub origin, and a fake mise that pins agt to `pin`.
fn setup(pin: &str) -> Env {
    let env = Env::new();
    env.write("README.md", "hello\n");
    env.commit_all("init");
    env.git(&[
        "remote",
        "add",
        "origin",
        "https://user:secret@github.com/o/r.git",
    ]);
    let mise = env.path("mise");
    let listing =
        format!(r#"{{"github:brandonocasey/agt":[{{"version":"{pin}","active":true}}]}}"#);
    fs::write(&mise, format!("#!/bin/sh\necho '{listing}'\n")).unwrap();
    fs::set_permissions(&mise, fs::Permissions::from_mode(0o755)).unwrap();
    env
}

/// Exit code and the JSON report of `agt doctor --json`.
fn doctor(env: &Env) -> (Option<i32>, Value) {
    let output = env
        .command(env!("CARGO_BIN_EXE_agt"), &env.repo)
        .env("AGT_MISE_BIN", env.path("mise"))
        .args(["doctor", "--json"])
        .output()
        .expect("agt runs");
    let report = serde_json::from_slice(&output.stdout).expect("JSON report");
    (output.status.code(), report)
}

fn row<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("no row {name}: {report}"))
}

#[test]
fn a_logged_in_repo_with_only_warnings_exits_2() {
    let env = setup("99.0.0");
    env.respond(&AUTH, "Logged in to github.com account octo (keyring)");
    let (code, report) = doctor(&env);
    assert_eq!(code, Some(2), "{report}");
    assert_eq!(report["status"], "warn");
    let auth = row(&report, "auth github.com");
    assert_eq!(auth["status"], "ok");
    assert_eq!(auth["detail"], "gh as octo");
    let version = row(&report, "agt version");
    assert_eq!(version["status"], "warn");
    assert_eq!(version["fix"], "mise install");
    let ignore = row(&report, "git ignore");
    assert_eq!(ignore["status"], "warn");
    assert!(ignore["fix"].as_str().unwrap().contains(".worktrees/"));
    assert_eq!(row(&report, "origin")["detail"], "github.com/o/r");
    assert_eq!(row(&report, "default branch")["status"], "ok");
    assert!(!report.to_string().contains("secret"));
}

#[test]
fn missing_auth_for_the_origin_forge_fails_with_exit_1() {
    let env = setup("0.0.1");
    let (code, report) = doctor(&env);
    assert_eq!(code, Some(1), "{report}");
    let auth = row(&report, "auth github.com");
    assert_eq!(auth["status"], "fail");
    assert_eq!(auth["fix"], "gh auth login --hostname github.com");
    assert_eq!(row(&report, "agt version")["status"], "ok");
}

#[test]
fn a_broken_repo_config_fails_and_names_the_file() {
    let env = setup("0.0.1");
    env.respond(&AUTH, "Logged in to github.com account octo (keyring)");
    env.write(".config/agt.toml", "[sync]\nauto_pul = true\n");
    let (code, report) = doctor(&env);
    assert_eq!(code, Some(1), "{report}");
    let config = row(&report, "repo config");
    assert_eq!(config["status"], "fail");
    assert!(config["detail"].as_str().unwrap().contains("agt.toml"));
}

#[test]
fn the_table_lists_a_fix_under_each_failed_row() {
    let env = setup("0.0.1");
    let output = env
        .command(env!("CARGO_BIN_EXE_agt"), &env.repo)
        .env("AGT_MISE_BIN", env.path("mise"))
        .arg("doctor")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("fail  auth github.com"), "{text}");
    assert!(
        text.contains("fix: gh auth login --hostname github.com"),
        "{text}"
    );
}
