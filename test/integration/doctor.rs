//! `agt doctor`: rows, fix hints and exit status, with fake gh/glab/mise and an isolated repo.

use serde_json::Value;

use crate::support::forge::forgejo;
use crate::support::{Env, executable};

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
    executable(&mise, format!("#!/bin/sh\necho '{listing}'\n"));
    env
}

/// Exit code and the JSON report of `agt doctor --json`.
fn doctor(env: &Env) -> (Option<i32>, Value) {
    let output = env
        .bin(&env.repo)
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
fn an_unignored_agt_directory_is_a_warning() {
    let env = setup("0.0.1");
    let global = env.path("gitignore");
    std::fs::write(&global, ".worktrees/\n/.cache/agents/scratch/\n").unwrap();
    let (_, report) = doctor(&env);
    let ignore = row(&report, "git ignore");
    assert_eq!(ignore["status"], "warn", "{report}");
    assert_eq!(ignore["detail"], "not ignored: /.agt/");
    env.write(
        ".gitignore",
        ".worktrees/\n/.agt/\n/.cache/agents/scratch/\n",
    );
    let (_, report) = doctor(&env);
    assert_eq!(row(&report, "git ignore")["status"], "ok", "{report}");
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
    env.write_repo_config("[sync]\nauto_pul = true\n");
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
        .bin(&env.repo)
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

#[test]
fn a_failing_mise_shows_its_error_and_a_matching_fix() {
    let env = setup("0.0.1");
    env.respond(&AUTH, "Logged in to github.com account octo (keyring)");
    let mise = env.path("mise");
    executable(
        &mise,
        "#!/bin/sh\necho 'mise ERROR Config files are not trusted' >&2\necho 'more' >&2\nexit 1\n",
    );
    let (_, report) = doctor(&env);
    let version = row(&report, "agt version");
    assert_eq!(version["status"], "warn");
    assert!(
        version["detail"]
            .as_str()
            .unwrap()
            .contains("mise ERROR Config files are not trusted"),
        "{version}"
    );
    assert!(!version["detail"].as_str().unwrap().contains("more"));
    assert_eq!(version["fix"], "mise trust");

    executable(
        &mise,
        "#!/bin/sh\necho 'mise WARN old config' >&2\necho 'mise ERROR real cause' >&2\necho 'trailer' >&2\nexit 1\n",
    );
    let (_, report) = doctor(&env);
    let detail = row(&report, "agt version")["detail"].to_string();
    assert!(
        detail.contains("real cause") && !detail.contains("WARN"),
        "{detail}"
    );

    executable(
        &mise,
        "#!/bin/sh\necho 'warming up' >&2\necho 'boom' >&2\nexit 1\n",
    );
    let (_, report) = doctor(&env);
    let detail = row(&report, "agt version")["detail"].to_string();
    assert!(
        detail.contains("boom") && !detail.contains("warming"),
        "{detail}"
    );

    executable(&mise, "#!/bin/sh\necho 'boom' >&2\nexit 1\n");
    let (_, report) = doctor(&env);
    assert_eq!(
        row(&report, "agt version")["fix"],
        "fix the mise error above"
    );
}

#[test]
fn an_origin_that_is_not_a_forge_warns_instead_of_failing() {
    let env = setup("0.0.1");
    env.git(&[
        "remote",
        "set-url",
        "origin",
        "https://git.example.invalid/o/r.git",
    ]);
    let (_, report) = doctor(&env);
    let auth = row(&report, "auth git.example.invalid");
    assert_eq!(auth["status"], "warn", "{report}");
    assert!(
        auth["detail"].as_str().unwrap().contains(
            "no gh, glab or tea login for git.example.invalid; forge features unavailable"
        )
    );
    assert_ne!(report["status"], "fail", "{report}");
}

#[test]
fn a_repo_without_a_default_branch_gets_a_plain_detail() {
    let env = Env::new();
    env.git(&["symbolic-ref", "HEAD", "refs/heads/trunk"]);
    env.write("a.txt", "a");
    env.commit_all("a");
    let (_, report) = doctor(&env);
    let branch = row(&report, "default branch");
    assert_eq!(branch["status"], "fail");
    assert_eq!(branch["detail"], "cannot find the default branch");
    assert_eq!(
        branch["fix"],
        "git remote set-head origin --auto, or create main"
    );
}

#[test]
fn offline_skips_the_login_checks() {
    let env = setup("99.0.0");
    let output = env
        .bin(&env.repo)
        .env("AGT_MISE_BIN", env.path("mise"))
        .args(["--offline", "doctor", "--json"])
        .output()
        .expect("agt runs");
    let report: Value = serde_json::from_slice(&output.stdout).expect("JSON report");
    assert_eq!(row(&report, "auth")["detail"], "skipped: offline");
    assert!(
        !env.forge_calls()
            .iter()
            .any(|call| call.contains("auth status")),
        "{:?}",
        env.forge_calls()
    );
}

/// Like [`setup`], with an origin on codeberg.org and a tea that prints its version.
fn forgejo_setup() -> Env {
    let env = setup("99.0.0");
    env.git(&[
        "remote",
        "set-url",
        "origin",
        "https://codeberg.org/o/r.git",
    ]);
    env.respond(&["--version"], "tea version 0.13.0");
    env
}

/// tea 0.11 has no `tea api`; 0.12 has it but no `-d`, which every request body goes through.
#[test]
fn a_tea_without_api_data_warns_with_the_minimum_version() {
    for old in ["0.11.1", "0.12.0"] {
        let env = forgejo_setup();
        forgejo::logins(&env);
        env.respond(&["--version"], &format!("tea version {old}"));
        let (_, report) = doctor(&env);
        let tea = row(&report, "tea");
        assert_eq!(tea["status"], "warn", "{report}");
        assert!(
            tea["detail"]
                .as_str()
                .unwrap()
                .contains(&format!("{old}, older than 0.13")),
            "{tea}"
        );
        assert!(tea["fix"].as_str().unwrap().contains("0.13"), "{tea}");
    }
}

#[test]
fn a_forgejo_origin_checks_the_tea_tool_and_its_login() {
    let env = forgejo_setup();
    forgejo::logins(&env);
    let (_, report) = doctor(&env);
    assert_eq!(row(&report, "tea")["status"], "ok", "{report}");
    assert_eq!(row(&report, "tea")["detail"], "0.13.0");
    let auth = row(&report, "auth codeberg.org");
    assert_eq!(auth["status"], "ok", "{report}");
    assert_eq!(auth["detail"], "tea as me");
    assert_eq!(row(&report, "origin")["detail"], "codeberg.org/o/r");
}

#[test]
fn a_forgejo_origin_without_a_tea_login_fails_with_the_login_command() {
    let env = forgejo_setup();
    let (code, report) = doctor(&env);
    assert_eq!(code, Some(1), "{report}");
    let auth = row(&report, "auth codeberg.org");
    assert_eq!(auth["status"], "fail");
    assert_eq!(auth["detail"], "tea: not logged in");
    assert_eq!(auth["fix"], "tea login add --url https://codeberg.org");
}

#[test]
fn a_missing_tea_warns_for_the_tool_and_fails_the_forgejo_origin_login() {
    let env = forgejo_setup();
    let output = env
        .bin(&env.repo)
        .env("AGT_MISE_BIN", env.path("mise"))
        .env("AGT_TEA_BIN", "/nonexistent/agt-test-tea")
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let tool = row(&report, "tea");
    assert_eq!(tool["status"], "warn", "{report}");
    assert!(
        tool["fix"]
            .as_str()
            .unwrap()
            .contains("https://gitea.com/gitea/tea")
    );
    assert_eq!(
        row(&report, "auth codeberg.org")["detail"],
        "tea: not installed"
    );
}

#[test]
fn a_self_hosted_origin_that_only_a_tea_login_names_is_checked_with_tea() {
    let env = forgejo_setup();
    env.git(&[
        "remote",
        "set-url",
        "origin",
        "https://git.example.invalid/o/r.git",
    ]);
    forgejo::logins_of(&env, &[("selfhost", "https://git.example.invalid")]);
    let (_, report) = doctor(&env);
    let auth = row(&report, "auth git.example.invalid");
    assert_eq!(auth["status"], "ok", "{report}");
    assert_eq!(auth["detail"], "tea as me");
}

#[test]
fn a_github_origin_reports_a_tea_that_is_not_logged_in_as_a_warning_only() {
    let env = setup("99.0.0");
    env.respond(&AUTH, "Logged in to github.com account octo (keyring)");
    let (_, report) = doctor(&env);
    let auth = row(&report, "auth tea");
    assert_eq!(auth["status"], "warn", "{report}");
    assert_eq!(auth["fix"], "tea login add");
    forgejo::logins(&env);
    let (_, report) = doctor(&env);
    assert_eq!(row(&report, "auth tea")["detail"], "logged in as me");
}

#[test]
fn offline_does_not_ask_tea_for_its_logins() {
    let env = forgejo_setup();
    let output = env
        .bin(&env.repo)
        .env("AGT_MISE_BIN", env.path("mise"))
        .args(["--offline", "doctor", "--json"])
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(row(&report, "auth")["detail"], "skipped: offline");
    assert!(
        !env.forge_calls()
            .iter()
            .any(|call| call.contains("logins list")),
        "{:?}",
        env.forge_calls()
    );
}
