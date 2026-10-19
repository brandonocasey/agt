//! `[worktree.hooks]`, `create --background` and `prepare --wait` in a temporary repository
//! with temporary HOME, XDG and git configuration.

use std::fs;
use std::path::Path;
use std::process::Output;

use serde_json::Value;

use crate::support::Env;

const BIN: &str = env!("CARGO_BIN_EXE_agt");

fn agt(env: &Env, dir: &Path, args: &[&str]) -> Output {
    env.command(BIN, dir)
        // An inherited PORT must never reach a hook unless one is recorded.
        .env("PORT", "54321")
        .arg("worktree")
        .args(args)
        .output()
        .expect("agt runs")
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "JSON output ({error}): {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn repo() -> Env {
    let env = Env::new();
    let dir = env.repo.clone();
    fs::write(dir.join("first"), "1").unwrap();
    env.git(&["-C", dir.to_str().unwrap(), "add", "-A"]);
    env.git(&["-C", dir.to_str().unwrap(), "commit", "-q", "-m", "first"]);
    env
}

fn repo_config(env: &Env, text: &str) {
    let dir = env.repo.join(".config");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("agt.toml"), text).unwrap();
}

fn global_config(env: &Env, text: &str) {
    let dir = env.path("config/agt");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.toml"), text).unwrap();
}

#[test]
fn create_hooks_run_in_the_worktree_with_the_hook_environment() {
    let env = repo();
    repo_config(
        &env,
        "[worktree.hooks]\ncreate = [\"echo \\\"$AGT_HOOK|$AGT_BRANCH|$AGT_WORKTREE|$AGT_MAIN_CHECKOUT|${PORT:-none}\\\" > env.txt\", \"touch second\"]\n",
    );
    let output = agt(&env, &env.repo, &["create", "feat/x"]);
    assert!(output.status.success());
    let value = json(&output);
    assert_eq!(value["hooks"]["ok"], true);
    assert_eq!(value["hooks"]["create"].as_array().unwrap().len(), 2);
    assert_eq!(value["hooks"]["create"][1]["command"], "touch second");
    let tree = env.repo.join(".worktrees/feat/x");
    let seen = fs::read_to_string(tree.join("env.txt")).unwrap();
    let real = |path: &Path| fs::canonicalize(path).unwrap().display().to_string();
    let expected = format!("create|feat/x|{}|{}|none", real(&tree), real(&env.repo));
    assert_eq!(seen.trim(), expected);
    assert!(tree.join("second").exists());
}

#[test]
fn a_recorded_port_reaches_the_hook_as_port() {
    let env = repo();
    repo_config(
        &env,
        "[worktree.hooks]\ncreate = [\"echo $PORT > port.txt\"]\n",
    );
    let value = json(&agt(&env, &env.repo, &["create", "p", "--port"]));
    let seen = fs::read_to_string(env.repo.join(".worktrees/p/port.txt")).unwrap();
    assert_eq!(seen.trim(), value["port"]["port"].to_string());
}

#[test]
fn the_repository_config_overrides_the_global_hooks_per_key() {
    let env = repo();
    global_config(
        &env,
        "[worktree.hooks]\ncreate = [\"touch global\"]\nremove = [\"true\"]\n",
    );
    repo_config(&env, "[worktree.hooks]\ncreate = [\"touch local\"]\n");
    json(&agt(&env, &env.repo, &["create", "o"]));
    let tree = env.repo.join(".worktrees/o");
    assert!(tree.join("local").exists());
    assert!(!tree.join("global").exists());
    let plan = json(&agt(&env, &env.repo, &["remove", tree.to_str().unwrap()]));
    assert_eq!(plan["hooks"][0], "true");
}

#[test]
fn a_failing_create_hook_exits_1_with_the_report_and_stops() {
    let env = repo();
    repo_config(
        &env,
        "[worktree.hooks]\ncreate = [\"echo boom; exit 4\", \"touch never\"]\n",
    );
    let output = agt(&env, &env.repo, &["create", "bad"]);
    assert_eq!(output.status.code(), Some(1));
    let value = json(&output);
    assert_eq!(value["hooks"]["ok"], false);
    let steps = value["hooks"]["create"].as_array().unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["exit"], 4);
    let log = fs::read_to_string(steps[0]["log"].as_str().unwrap()).unwrap();
    assert!(log.contains("boom"));
    assert!(!env.repo.join(".worktrees/bad/never").exists());
}

#[test]
fn a_reused_worktree_and_no_hooks_run_nothing() {
    let env = repo();
    repo_config(&env, "[worktree.hooks]\ncreate = [\"touch ran\"]\n");
    let skipped = json(&agt(&env, &env.repo, &["create", "r", "--no-hooks"]));
    assert!(skipped.get("hooks").is_none());
    let tree = env.repo.join(".worktrees/r");
    assert!(!tree.join("ran").exists());
    let again = json(&agt(&env, &env.repo, &["create", "r"]));
    assert_eq!(again["reused"], true);
    assert!(again.get("hooks").is_none());
    assert!(!tree.join("ran").exists());
}

#[test]
fn remove_hooks_run_before_the_removal_and_the_plan_lists_them() {
    let env = repo();
    let marker = env.path("marker");
    repo_config(
        &env,
        &format!(
            "[worktree.hooks]\nremove = [\"test -d \\\"$AGT_WORKTREE\\\" && echo $AGT_HOOK > {}\"]\n",
            marker.display()
        ),
    );
    json(&agt(&env, &env.repo, &["create", "gone"]));
    let tree = env.repo.join(".worktrees/gone");
    let plan = json(&agt(&env, &env.repo, &["remove", tree.to_str().unwrap()]));
    assert_eq!(plan["hooks"].as_array().unwrap().len(), 1);
    assert!(!marker.exists());
    let output = agt(
        &env,
        &env.repo,
        &["remove", tree.to_str().unwrap(), "--yes"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(&marker).unwrap().trim(), "remove");
    assert!(!tree.exists());
}

#[test]
fn a_failing_remove_hook_blocks_the_removal_and_no_hooks_skips_it() {
    let env = repo();
    repo_config(&env, "[worktree.hooks]\nremove = [\"exit 7\"]\n");
    json(&agt(&env, &env.repo, &["create", "keep"]));
    let tree = env.repo.join(".worktrees/keep");
    let output = agt(
        &env,
        &env.repo,
        &["remove", tree.to_str().unwrap(), "--yes"],
    );
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("exit 7"), "{error}");
    assert!(tree.exists());
    let output = agt(
        &env,
        &env.repo,
        &["remove", tree.to_str().unwrap(), "--yes", "--no-hooks"],
    );
    assert!(output.status.success());
    assert!(!tree.exists());
}

#[test]
fn background_returns_before_a_slow_hook_and_wait_reports_it() {
    let env = repo();
    repo_config(
        &env,
        "[worktree.hooks]\ncreate = [\"sleep 2; touch done\"]\n",
    );
    let started = std::time::Instant::now();
    let output = agt(&env, &env.repo, &["create", "bg", "--background"]);
    assert!(output.status.success());
    assert!(started.elapsed().as_secs() < 2);
    let value = json(&output);
    assert!(value["background"]["pid"].as_u64().unwrap() > 0);
    let tree = env.repo.join(".worktrees/bg");
    assert!(!tree.join("done").exists());
    let status = value["background"]["status"].as_str().unwrap();
    assert!(Path::new(status).starts_with(env.repo.join(".git")));
    let waited = agt(&env, &tree, &["prepare", "--wait"]);
    assert!(
        waited.status.success(),
        "{}",
        String::from_utf8_lossy(&waited.stderr)
    );
    let report = json(&waited);
    assert_eq!(report["state"], "done");
    assert_eq!(report["hooks"]["create"][0]["ok"], true);
    assert!(tree.join("done").exists());
}

#[test]
fn background_with_nothing_to_run_starts_no_child() {
    let env = repo();
    let value = json(&agt(&env, &env.repo, &["create", "idle", "--background"]));
    assert_eq!(value["background"]["started"], false);
    assert!(value["background"].get("pid").is_none());
}

#[test]
fn wait_without_a_status_file_is_an_error() {
    let env = repo();
    json(&agt(&env, &env.repo, &["create", "w"]));
    let tree = env.repo.join(".worktrees/w");
    let output = agt(&env, &tree, &["prepare", "--wait"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no background setup for"));
}

#[test]
fn wait_gives_up_after_max_with_exit_3() {
    let env = repo();
    repo_config(&env, "[worktree.hooks]\ncreate = [\"sleep 5\"]\n");
    json(&agt(&env, &env.repo, &["create", "slow", "--background"]));
    let tree = env.repo.join(".worktrees/slow");
    let output = agt(&env, &tree, &["prepare", "--wait", "--max", "1"]);
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn an_inherited_port_is_not_passed_when_none_is_recorded() {
    let env = repo();
    repo_config(
        &env,
        "[worktree.hooks]\ncreate = [\"echo ${PORT:-none} > port.txt\"]\n",
    );
    json(&agt(&env, &env.repo, &["create", "np"]));
    let seen = fs::read_to_string(env.repo.join(".worktrees/np/port.txt")).unwrap();
    assert_eq!(seen.trim(), "none");
}

#[test]
fn an_invalid_config_fails_before_the_worktree_is_created() {
    let env = repo();
    repo_config(&env, "[worktree.hooks]\ncreate = 5\n");
    let output = agt(&env, &env.repo, &["create", "inv"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(!env.repo.join(".worktrees/inv").exists());
    let branches = env.git(&["-C", env.repo.to_str().unwrap(), "branch", "--list", "inv"]);
    assert!(branches.trim().is_empty(), "{branches}");
}

#[test]
fn a_second_background_create_reports_the_running_setup() {
    let env = repo();
    repo_config(&env, "[worktree.hooks]\ncreate = [\"sleep 3\"]\n");
    let first = json(&agt(&env, &env.repo, &["create", "twice", "--background"]));
    let again = json(&agt(
        &env,
        &env.repo,
        &["create", "twice", "--background", "--prepare"],
    ));
    assert_eq!(again["background"]["already_running"], true);
    assert_eq!(again["background"]["log"], first["background"]["log"]);
    assert_eq!(again["background"]["status"], first["background"]["status"]);
}

#[test]
fn a_status_stuck_at_starting_does_not_block_a_new_setup() {
    let env = repo();
    let first = json(&agt(&env, &env.repo, &["create", "stuck"]));
    let tree = Path::new(first["path"].as_str().unwrap()).to_path_buf();
    let git_dir = String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "--absolute-git-dir"])
            .current_dir(&tree)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let status = Path::new(git_dir.trim()).join("agt-setup.json");
    fs::write(&status, r#"{"state":"running","pid":0,"log":null}"#).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    fs::File::options()
        .write(true)
        .open(&status)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let waited = agt(&env, &tree, &["prepare", "--wait", "--max", "5"]);
    assert_eq!(waited.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&waited.stderr).contains("never started"));
    let again = json(&agt(
        &env,
        &env.repo,
        &["create", "stuck", "--background", "--prepare"],
    ));
    assert!(again["background"]["already_running"].is_null());
    let done = agt(&env, &tree, &["prepare", "--wait", "--max", "60"]);
    assert!(done.status.success());
}

#[test]
fn prepare_hooks_reruns_a_failed_create_hook() {
    let env = repo();
    repo_config(
        &env,
        "[worktree.hooks]\ncreate = [\"test -f fixed\", \"touch ran\"]\n",
    );
    let failed = agt(&env, &env.repo, &["create", "re"]);
    assert_eq!(failed.status.code(), Some(1));
    let tree = env.repo.join(".worktrees/re");
    let still = agt(&env, &tree, &["prepare", "--hooks"]);
    assert_eq!(still.status.code(), Some(1));
    assert_eq!(json(&still)["hooks"]["ok"], false);
    fs::write(tree.join("fixed"), "").unwrap();
    let output = agt(&env, &tree, &["prepare", "--hooks"]);
    assert!(output.status.success());
    let value = json(&output);
    assert_eq!(value["hooks"]["ok"], true);
    assert!(value["steps"].is_array());
    assert!(tree.join("ran").exists());
}
