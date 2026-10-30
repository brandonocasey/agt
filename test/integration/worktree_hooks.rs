//! `[worktree.hooks]`, `create --background` and `prepare --wait` in a temporary repository
//! with temporary HOME, XDG and git configuration.

use std::fs;
use std::path::Path;
use std::process::Output;

use serde_json::Value;

use crate::support::Env;

fn agt(env: &Env, dir: &Path, args: &[&str]) -> Output {
    env.bin(dir)
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

/// Writes the repository config and trusts it.
fn repo_config(env: &Env, text: &str) {
    env.write_repo_config(text);
    let output = agt(env, &env.repo, &["trust"]);
    assert!(output.status.success(), "{output:?}");
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
    let port = json(&agt(&env, &tree, &["port"]))["port"].to_string();
    let expected = format!("create|feat/x|{}|{}|{port}", real(&tree), real(&env.repo));
    assert_eq!(seen.trim(), expected);
    assert!(tree.join("second").exists());
}

#[test]
fn create_hooks_run_for_a_detached_worktree_with_an_empty_branch() {
    let env = repo();
    repo_config(
        &env,
        "[worktree.hooks]\ncreate = [\"echo \\\"[$AGT_BRANCH]\\\" > env.txt\"]\n",
    );
    let output = agt(&env, &env.repo, &["create", "review-1", "--detach", "HEAD"]);
    assert_eq!(json(&output)["hooks"]["ok"], true);
    let seen = fs::read_to_string(env.repo.join(".worktrees/review-1/env.txt")).unwrap();
    assert_eq!(seen.trim(), "[]");
}

#[test]
fn a_create_hook_gets_the_worktrees_port_as_port() {
    let env = repo();
    repo_config(
        &env,
        "[worktree.hooks]\ncreate = [\"echo $PORT > port.txt\"]\n",
    );
    let created = json(&agt(&env, &env.repo, &["create", "p"]));
    assert!(created.get("port").is_none());
    let tree = env.repo.join(".worktrees/p");
    let seen = fs::read_to_string(tree.join("port.txt")).unwrap();
    let recorded = json(&agt(&env, &tree, &["port"]));
    assert_eq!(seen.trim(), recorded["port"].to_string());
}

#[test]
fn the_repository_config_overrides_the_global_hooks_per_key() {
    let env = repo();
    env.write_config("[worktree.hooks]\ncreate = [\"touch global\"]\nremove = [\"true\"]\n");
    repo_config(&env, "[worktree.hooks]\ncreate = [\"touch local\"]\n");
    json(&agt(&env, &env.repo, &["create", "o"]));
    let tree = env.repo.join(".worktrees/o");
    assert!(tree.join("local").exists());
    assert!(!tree.join("global").exists());
    let plan = json(&agt(&env, &env.repo, &["remove", tree.to_str().unwrap()]));
    assert_eq!(plan["hooks"]["remove"][0], "true");
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
    assert_eq!(plan["hooks"]["remove"].as_array().unwrap().len(), 1);
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
    let output = agt(&env, &tree, &["prepare", "--wait", "--timeout", "1"]);
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn an_inherited_port_is_not_passed_to_a_remove_hook_when_none_is_recorded() {
    let env = repo();
    let marker = env.path("marker");
    repo_config(
        &env,
        &format!(
            "[worktree.hooks]\nremove = [\"echo ${{PORT:-none}} > {}\"]\n",
            marker.display()
        ),
    );
    json(&agt(&env, &env.repo, &["create", "np"]));
    let tree = env.repo.join(".worktrees/np");
    let output = agt(
        &env,
        &env.repo,
        &["remove", tree.to_str().unwrap(), "--yes"],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fs::read_to_string(&marker).unwrap().trim(), "none");
}

#[test]
fn an_invalid_config_fails_before_the_worktree_is_created() {
    let env = repo();
    env.write_repo_config("[worktree.hooks]\ncreate = 5\n");
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
fn a_second_background_create_with_nothing_new_still_reports_the_running_setup() {
    let env = repo();
    repo_config(&env, "[worktree.hooks]\ncreate = [\"sleep 3\"]\n");
    let first = json(&agt(&env, &env.repo, &["create", "twice", "--background"]));
    let again = json(&agt(&env, &env.repo, &["create", "twice", "--background"]));
    assert_eq!(again["background"]["already_running"], true);
    assert_eq!(again["background"]["pid"], first["background"]["pid"]);
}

#[test]
fn prepare_refuses_while_a_background_setup_runs() {
    let env = repo();
    repo_config(&env, "[worktree.hooks]\ncreate = [\"sleep 3\"]\n");
    let first = json(&agt(&env, &env.repo, &["create", "busy", "--background"]));
    let tree = first["path"].as_str().unwrap();
    for args in [
        vec!["prepare", tree],
        vec!["prepare", "--hooks", tree],
        vec!["create", "busy", "--prepare"],
    ] {
        let output = agt(&env, &env.repo, &args);
        assert!(!output.status.success(), "{args:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("prepare --wait"), "{args:?}: {error}");
    }
    let waited = agt(&env, &env.repo, &["prepare", "--wait", tree]);
    assert!(waited.status.success());
    assert!(
        agt(&env, &env.repo, &["prepare", "--hooks", tree])
            .status
            .success()
    );
}

#[test]
fn a_branch_named_like_a_tag_reaches_the_hook_unshortened() {
    let env = repo();
    env.git(&["-C", env.repo.to_str().unwrap(), "tag", "v1"]);
    repo_config(
        &env,
        "[worktree.hooks]\ncreate = [\"echo \\\"$AGT_BRANCH\\\" > branch.txt\"]\n",
    );
    let value = json(&agt(&env, &env.repo, &["create", "v1"]));
    let tree = Path::new(value["path"].as_str().unwrap());
    let seen = fs::read_to_string(tree.join("branch.txt")).unwrap();
    assert_eq!(seen.trim(), "v1");
}

#[test]
fn a_damaged_ports_file_does_not_block_a_remove_without_hooks() {
    let env = repo();
    json(&agt(&env, &env.repo, &["create", "plain"]));
    let ports = env.path("state/agents/worktrees/ports.json");
    fs::create_dir_all(ports.parent().unwrap()).unwrap();
    fs::write(&ports, "{bad").unwrap();
    let tree = env.repo.join(".worktrees/plain");
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
    assert!(!tree.exists());
}

#[test]
fn work_a_remove_hook_leaves_behind_blocks_the_removal() {
    let env = repo();
    repo_config(
        &env,
        "[worktree.hooks]\nremove = [\"git init -q nested && git -C nested -c user.name=t -c user.email=t@t commit -q --allow-empty -m x\"]\n",
    );
    json(&agt(&env, &env.repo, &["create", "late"]));
    let tree = env.repo.join(".worktrees/late");
    let output = agt(
        &env,
        &env.repo,
        &["remove", tree.to_str().unwrap(), "--yes"],
    );
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("nested"), "{error}");
    assert!(tree.join("nested").exists());
}

#[test]
fn a_status_stuck_at_starting_does_not_block_a_new_setup() {
    let env = repo();
    let first = json(&agt(&env, &env.repo, &["create", "stuck"]));
    let tree = Path::new(first["path"].as_str().unwrap()).to_path_buf();
    let git_dir = env.git(&[
        "-C",
        tree.to_str().unwrap(),
        "rev-parse",
        "--absolute-git-dir",
    ]);
    let status = Path::new(git_dir.trim()).join("agt-setup.json");
    fs::write(&status, r#"{"state":"running","pid":0,"log":null}"#).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(120);
    fs::File::options()
        .write(true)
        .open(&status)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let waited = agt(&env, &tree, &["prepare", "--wait", "--timeout", "5"]);
    assert_eq!(waited.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&waited.stderr).contains("never started"));
    let again = json(&agt(
        &env,
        &env.repo,
        &["create", "stuck", "--background", "--prepare"],
    ));
    assert!(again["background"]["already_running"].is_null());
    let done = agt(&env, &tree, &["prepare", "--wait", "--timeout", "60"]);
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
    // Only the hooks run: no install steps.
    assert!(value.get("steps").is_none());
    assert!(tree.join("ran").exists());
}

fn hooks_of(report: &Value) -> &Value {
    &report["hooks"]
}

#[test]
fn untrusted_repository_hooks_do_not_run_and_are_reported() {
    let env = repo();
    env.write_repo_config(
        "[worktree.hooks]\ncreate = [\"touch ran\"]\nremove = [\"touch gone\"]\n",
    );
    let output = agt(&env, &env.repo, &["create", "feature"]);
    assert!(output.status.success(), "{output:?}");
    let report = json(&output);
    let path = report["path"].as_str().unwrap();
    assert!(!Path::new(path).join("ran").exists());
    let untrusted = &hooks_of(&report)["untrusted"];
    assert_eq!(untrusted["create"][0], "touch ran");
    assert_eq!(untrusted["remove"][0], "touch gone");
    assert!(
        untrusted["file"]
            .as_str()
            .unwrap()
            .ends_with(".config/agt.toml")
    );
    assert!(
        untrusted["next"]
            .as_str()
            .unwrap()
            .contains("ask the user to review")
    );
    assert_eq!(hooks_of(&report)["create"].as_array().unwrap().len(), 0);
}

#[test]
fn trusting_makes_the_hooks_run_and_editing_untrusts_them() {
    let env = repo();
    repo_config(&env, "[worktree.hooks]\ncreate = [\"touch ran\"]\n");
    let report = json(&agt(&env, &env.repo, &["create", "one"]));
    assert!(
        Path::new(report["path"].as_str().unwrap())
            .join("ran")
            .exists()
    );
    assert!(hooks_of(&report).get("untrusted").is_none());
    env.write_repo_config("[worktree.hooks]\ncreate = [\"touch ran2\"]\n");
    let report = json(&agt(&env, &env.repo, &["create", "two"]));
    assert!(
        !Path::new(report["path"].as_str().unwrap())
            .join("ran2")
            .exists()
    );
    assert_eq!(hooks_of(&report)["untrusted"]["create"][0], "touch ran2");
}

#[test]
fn global_hooks_run_when_the_repository_hooks_are_untrusted() {
    let env = repo();
    env.write_config("[worktree.hooks]\ncreate = [\"touch global\"]\n");
    env.write_repo_config("[worktree.hooks]\ncreate = [\"touch local\"]\n");
    let report = json(&agt(&env, &env.repo, &["create", "feature"]));
    let path = Path::new(report["path"].as_str().unwrap());
    assert!(path.join("global").exists());
    assert!(!path.join("local").exists());
    assert_eq!(hooks_of(&report)["untrusted"]["create"][0], "touch local");
}

#[test]
fn a_repository_without_hooks_reports_nothing_untrusted() {
    let env = repo();
    env.write_repo_config("[sync]\npull = \"manual\"\n");
    let report = json(&agt(&env, &env.repo, &["create", "feature"]));
    assert!(report.get("hooks").is_none());
}

#[test]
fn background_child_reports_untrusted_hooks() {
    let env = repo();
    env.write_repo_config("[worktree.hooks]\ncreate = [\"touch local\"]\n");
    let created = json(&agt(
        &env,
        &env.repo,
        &["create", "feature", "--background"],
    ));
    assert_eq!(created["hooks"]["untrusted"]["create"][0], "touch local");
    let path = created["path"].as_str().unwrap();
    let waited = json(&agt(&env, &env.repo, &["prepare", "--wait", path]));
    assert_eq!(waited["hooks"]["untrusted"]["create"][0], "touch local");
    assert!(!Path::new(path).join("local").exists());
}

#[test]
fn remove_plan_and_yes_honor_the_trust() {
    let env = repo();
    env.write_repo_config("[worktree.hooks]\nremove = [\"touch ../gone\"]\n");
    let created = json(&agt(&env, &env.repo, &["create", "feature"]));
    let path = created["path"].as_str().unwrap();
    let plan = json(&agt(&env, &env.repo, &["remove", path]));
    assert_eq!(plan["hooks"]["remove"].as_array().unwrap().len(), 0);
    assert_eq!(plan["hooks"]["untrusted"]["remove"][0], "touch ../gone");
    let done = json(&agt(&env, &env.repo, &["remove", path, "--yes"]));
    assert_eq!(done["hooks"]["untrusted"]["remove"][0], "touch ../gone");
    assert!(!Path::new(path).parent().unwrap().join("gone").exists());
}

#[test]
fn trust_remove_and_list() {
    let env = repo();
    env.write_repo_config("[worktree.hooks]\ncreate = [\"touch ran\"]\n");
    let trusted = json(&agt(&env, &env.repo, &["trust"]));
    assert_eq!(trusted["create"][0], "touch ran");
    assert_eq!(trusted["sha256"].as_str().unwrap().len(), 64);
    let listed = json(&agt(&env, &env.repo, &["trust", "--list"]));
    assert_eq!(listed["trusted"][0]["current"], true);
    assert_eq!(listed["trusted"][0]["sha256"], trusted["sha256"]);
    env.write_repo_config("[worktree.hooks]\ncreate = [\"touch other\"]\n");
    let listed = json(&agt(&env, &env.repo, &["trust", "--list"]));
    assert_eq!(listed["trusted"][0]["current"], false);
    fs::remove_file(env.repo.join(".config/agt.toml")).unwrap();
    let listed = json(&agt(&env, &env.repo, &["trust", "--list"]));
    assert_eq!(listed["trusted"][0]["current"], "missing");
    env.write_repo_config("[worktree.hooks]\ncreate = [\"touch ran\"]\n");
    let removed = json(&agt(&env, &env.repo, &["trust", "--remove"]));
    assert_eq!(removed["removed"], true);
    let listed = json(&agt(&env, &env.repo, &["trust", "--list"]));
    assert_eq!(listed["trusted"].as_array().unwrap().len(), 0);
}

#[test]
fn trust_list_reports_a_config_replaced_by_a_symlink() {
    let env = repo();
    let text = "[worktree.hooks]\ncreate = [\"touch ran\"]\n";
    env.write_repo_config(text);
    agt(&env, &env.repo, &["trust"]);
    let elsewhere = env.repo.parent().unwrap().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("agt.toml"), text).unwrap();
    fs::remove_dir_all(env.repo.join(".config")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, env.repo.join(".config")).unwrap();
    let listed = json(&agt(&env, &env.repo, &["trust", "--list"]));
    assert_eq!(listed["trusted"][0]["current"], false);
    assert!(
        listed["trusted"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("symlink")
    );
}

#[test]
fn trust_errors_without_a_file_or_hooks() {
    let env = repo();
    assert!(!agt(&env, &env.repo, &["trust"]).status.success());
    env.write_repo_config("[sync]\npull = \"manual\"\n");
    assert!(!agt(&env, &env.repo, &["trust"]).status.success());
}

#[test]
fn the_next_command_quotes_the_path() {
    let env = repo();
    env.write_repo_config("[worktree.hooks]\ncreate = [\"true\"]\n");
    let report = json(&agt(&env, &env.repo, &["create", "feature"]));
    let next = hooks_of(&report)["untrusted"]["next"].as_str().unwrap();
    assert!(
        next.ends_with(&format!("agt worktree trust {}", {
            let main = env.repo.canonicalize().unwrap();
            let text = main.display().to_string();
            if text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-./@:+,=%".contains(c))
            {
                text
            } else {
                format!("'{text}'")
            }
        })),
        "{next}"
    );
}

#[test]
fn a_symlinked_config_cannot_borrow_another_repositorys_trust() {
    let env = repo();
    repo_config(&env, "[worktree.hooks]\ncreate = [\"touch ran\"]\n");
    let victim = env.repo.clone();
    let attacker = env.path("attacker");
    fs::create_dir_all(&attacker).unwrap();
    let at = attacker.to_str().unwrap();
    env.git(&["init", "-q", "-b", "main", at]);
    fs::write(attacker.join("first"), "1").unwrap();
    env.git(&["-C", at, "add", "-A"]);
    env.git(&["-C", at, "commit", "-q", "-m", "first"]);
    std::os::unix::fs::symlink(victim.join(".config"), attacker.join(".config")).unwrap();
    let report = json(&agt(&env, &attacker, &["create", "feature"]));
    let path = Path::new(report["path"].as_str().unwrap());
    assert!(!path.join("ran").exists());
    let untrusted = &hooks_of(&report)["untrusted"];
    assert_eq!(untrusted["create"][0], "touch ran");
    assert!(untrusted["reason"].as_str().unwrap().contains("symlink"));
    let refused = agt(&env, &attacker, &["trust"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("symlink"));
}

#[test]
fn a_symlinked_config_file_is_untrusted_too() {
    let env = repo();
    let real = env.path("real.toml");
    fs::write(&real, "[worktree.hooks]\ncreate = [\"touch ran\"]\n").unwrap();
    fs::create_dir_all(env.repo.join(".config")).unwrap();
    std::os::unix::fs::symlink(&real, env.repo.join(".config/agt.toml")).unwrap();
    assert!(!agt(&env, &env.repo, &["trust"]).status.success());
    let report = json(&agt(&env, &env.repo, &["create", "feature"]));
    assert!(hooks_of(&report)["untrusted"]["reason"].is_string());
}

#[test]
fn remove_no_hooks_ignores_a_bad_repository_config() {
    let env = repo();
    let created = json(&agt(&env, &env.repo, &["create", "feature"]));
    let path = created["path"].as_str().unwrap();
    env.write_repo_config("[worktree.hooks]\ncreate = 5\n");
    let plan = agt(&env, &env.repo, &["remove", path, "--no-hooks"]);
    assert!(plan.status.success(), "{plan:?}");
    assert!(!agt(&env, &env.repo, &["remove", path]).status.success());
}
