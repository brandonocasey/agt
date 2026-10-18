//! `agt worktree create`, `prepare` and `port` against local bare repositories, fake package
//! managers on a temporary PATH and a temporary state directory.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::Value;

use crate::support::Env;

const BIN: &str = env!("CARGO_BIN_EXE_agt");
const FAKE_TOOL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixtures/fake-tool.sh");

fn agt(env: &Env, dir: &Path, args: &[&str]) -> Output {
    env.command(BIN, dir)
        .arg("worktree")
        .args(args)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "protocol.file.allow")
        .env("GIT_CONFIG_VALUE_0", "always")
        .output()
        .expect("agt runs")
}

fn agt_json(env: &Env, dir: &Path, args: &[&str]) -> Value {
    let output = agt(env, dir, args);
    assert!(
        output.status.success(),
        "agt worktree {args:?}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON output")
}

fn git(env: &Env, dir: &Path, args: &[&str]) -> String {
    let mut all = vec!["-C", dir.to_str().unwrap()];
    all.extend_from_slice(args);
    env.git(&all)
}

/// Writes `file` into `dir` and commits it; returns the new commit.
fn commit(env: &Env, dir: &Path, file: &str, text: &str) -> String {
    fs::write(dir.join(file), text).unwrap();
    git(env, dir, &["add", "-A"]);
    git(env, dir, &["commit", "-q", "-m", file]);
    git(env, dir, &["rev-parse", "HEAD"])
}

/// A bare repository `name` that `env.repo` has as remote `remote`, holding `main`.
fn remote(env: &Env, name: &str, remote: &str) -> PathBuf {
    let bare = env.path(name);
    git(
        env,
        &env.repo,
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    git(
        env,
        &env.repo,
        &["remote", "add", remote, bare.to_str().unwrap()],
    );
    bare
}

/// Adds a commit on `main` to the bare repository through a scratch clone.
fn advance(env: &Env, bare: &Path, file: &str) -> String {
    let clone = env.path(&format!("clone-{file}"));
    git(
        env,
        &env.repo,
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let tip = commit(env, &clone, file, file);
    git(env, &clone, &["push", "-q", "origin", "HEAD:main"]);
    tip
}

fn repo_with_commit() -> (Env, String) {
    let env = Env::new();
    let first = commit(&env, &env.repo.clone(), "first", "1");
    (env, first)
}

#[test]
fn a_new_branch_starts_at_the_remote_default_ahead_of_the_local_one() {
    let (env, first) = repo_with_commit();
    let bare = remote(&env, "origin.git", "origin");
    git(&env, &env.repo, &["push", "-q", "origin", "main"]);
    git(&env, &env.repo, &["fetch", "-q", "origin"]);
    let tip = advance(&env, &bare, "second");
    let value = agt_json(&env, &env.repo, &["create", "feat/login"]);
    assert_eq!(value["reused"], false);
    assert_eq!(value["base"]["commit"], tip.as_str());
    assert_eq!(value["base"]["diverged"], false);
    assert_eq!(value["base"]["candidates"].as_array().unwrap().len(), 2);
    assert_eq!(value["base"]["candidates"][0]["remote"], "origin");
    assert_eq!(value["base"]["candidates"][1]["commit"], first.as_str());
    assert_eq!(
        value["path"],
        env.repo.join(".worktrees/feat/login").to_str().unwrap()
    );
    assert_eq!(git(&env, &env.repo, &["rev-parse", "feat/login"]), tip);
    // Nothing existing moved: not the local branch, not the remote-tracking branch.
    assert_eq!(git(&env, &env.repo, &["rev-parse", "main"]), first);
    assert_eq!(git(&env, &env.repo, &["rev-parse", "origin/main"]), first);
}

#[test]
fn with_no_remote_the_local_default_is_the_base() {
    let (env, first) = repo_with_commit();
    let value = agt_json(&env, &env.repo, &["create", "topic"]);
    assert_eq!(value["base"]["commit"], first.as_str());
    assert_eq!(value["base"]["candidates"].as_array().unwrap().len(), 1);
}

#[test]
fn the_newest_of_two_remotes_wins_when_they_do_not_diverge() {
    let (env, _) = repo_with_commit();
    let origin = remote(&env, "origin.git", "origin");
    let upstream = remote(&env, "upstream.git", "upstream");
    git(&env, &env.repo, &["push", "-q", "origin", "main"]);
    git(&env, &env.repo, &["push", "-q", "upstream", "main"]);
    let tip = advance(&env, &upstream, "ahead");
    let _ = origin;
    let value = agt_json(&env, &env.repo, &["create", "work"]);
    assert_eq!(value["base"]["commit"], tip.as_str());
    assert_eq!(value["base"]["diverged"], false);
    assert_eq!(value["base"]["candidates"].as_array().unwrap().len(), 3);
}

#[test]
fn diverged_candidates_use_origin_and_are_all_reported() {
    let (env, _) = repo_with_commit();
    let origin = remote(&env, "origin.git", "origin");
    let upstream = remote(&env, "upstream.git", "upstream");
    git(&env, &env.repo, &["push", "-q", "origin", "main"]);
    git(&env, &env.repo, &["push", "-q", "upstream", "main"]);
    let from_origin = advance(&env, &origin, "one");
    let from_upstream = advance(&env, &upstream, "two");
    let value = agt_json(&env, &env.repo, &["create", "work"]);
    assert_eq!(value["base"]["diverged"], true);
    assert_eq!(value["base"]["commit"], from_origin.as_str());
    let listed: Vec<&str> = value["base"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["commit"].as_str().unwrap())
        .collect();
    assert!(listed.contains(&from_origin.as_str()));
    assert!(listed.contains(&from_upstream.as_str()));
    assert_eq!(listed.len(), 3);
}

#[test]
fn a_remote_that_cannot_be_reached_is_an_error_naming_it() {
    let (env, _) = repo_with_commit();
    let missing = env.path("missing.git");
    git(
        &env,
        &env.repo,
        &["remote", "add", "gone", missing.to_str().unwrap()],
    );
    let output = agt(&env, &env.repo, &["create", "work"]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("remote gone"), "{error}");
    assert!(!env.repo.join(".worktrees/work").exists());
}

#[test]
fn an_explicit_base_is_used_as_given() {
    let (env, first) = repo_with_commit();
    commit(&env, &env.repo.clone(), "second", "2");
    let value = agt_json(&env, &env.repo, &["create", "old", "--base", &first]);
    assert_eq!(value["base"]["commit"], first.as_str());
    assert_eq!(git(&env, &env.repo, &["rev-parse", "old"]), first);
}

#[test]
fn a_checked_out_branch_is_reused() {
    let (env, _) = repo_with_commit();
    let first = agt_json(&env, &env.repo, &["create", "feat/a"]);
    let again = agt_json(&env, &env.repo, &["create", "feat/a"]);
    assert_eq!(again["reused"], true);
    assert_eq!(again["path"], first["path"]);
    let main = agt_json(&env, &env.repo, &["create", "main"]);
    assert_eq!(main["reused"], true);
    assert_eq!(main["path"], env.repo.to_str().unwrap());
}

#[test]
fn an_existing_branch_without_a_worktree_gets_one_and_stays_put() {
    let (env, first) = repo_with_commit();
    git(&env, &env.repo, &["branch", "topic"]);
    commit(&env, &env.repo.clone(), "second", "2");
    let value = agt_json(&env, &env.repo, &["create", "topic"]);
    assert_eq!(value["reused"], false);
    assert_eq!(value["commit"], first.as_str());
    assert_eq!(git(&env, &env.repo, &["rev-parse", "topic"]), first);
    assert!(env.repo.join(".worktrees/topic/first").exists());
}

#[test]
fn a_bare_main_checkout_needs_a_path() {
    let (env, _) = repo_with_commit();
    let bare = env.path("bare.git");
    git(
        &env,
        &env.repo,
        &[
            "clone",
            "-q",
            "--bare",
            env.repo.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let output = agt(&env, &bare, &["create", "work"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--path"));
    let target = env.path("elsewhere");
    let value = agt_json(
        &env,
        &bare,
        &["create", "work", "--path", target.to_str().unwrap()],
    );
    assert_eq!(value["path"], target.to_str().unwrap());
    assert!(target.join("first").exists());
}

#[test]
fn submodules_are_initialized_in_the_new_worktree() {
    let (env, _) = repo_with_commit();
    let library = env.path("library");
    fs::create_dir_all(&library).unwrap();
    git(&env, &library, &["init", "-q", "-b", "main"]);
    commit(&env, &library, "lib.txt", "lib");
    let main = env.repo.clone();
    git(
        &env,
        &main,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            library.to_str().unwrap(),
            "vendor",
        ],
    );
    git(&env, &main, &["commit", "-q", "-m", "add submodule"]);
    let value = agt_json(&env, &env.repo, &["create", "with-sub"]);
    assert_eq!(value["submodules"], true);
    assert!(env.repo.join(".worktrees/with-sub/vendor/lib.txt").exists());
}

#[test]
fn prepare_and_port_run_after_create_on_request() {
    let (env, _) = repo_with_commit();
    let tools = fake_tools(&env);
    fs::write(env.repo.join("Cargo.lock"), "").unwrap();
    git(&env, &env.repo, &["add", "-A"]);
    git(&env, &env.repo, &["commit", "-q", "-m", "lock"]);
    let value = prepared(
        &env,
        &tools,
        &env.repo,
        &["create", "both", "--prepare", "--port"],
    );
    assert_eq!(value["prepare"]["ok"], true);
    assert_eq!(
        value["prepare"]["steps"][0]["command"],
        "cargo fetch --locked"
    );
    assert!(value["port"]["port"].as_u64().is_some());
}

/// A directory of fake package managers: each appends `<name> <args>` to `$FAKE_LOG`, prints a
/// line, and fails when `$FAKE_FAIL` names it with its first argument (`npm ci`).
fn fake_tools(env: &Env) -> PathBuf {
    let bin = env.path("bin");
    fs::create_dir_all(&bin).unwrap();
    let script = fs::read(FAKE_TOOL).unwrap();
    for name in [
        "mise", "npm", "pnpm", "yarn", "bun", "uv", "poetry", "cargo", "go",
    ] {
        let file = bin.join(name);
        fs::write(&file, &script).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

fn prepared(env: &Env, tools: &Path, dir: &Path, args: &[&str]) -> Value {
    let output = prepare_output(env, tools, dir, args, "");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn prepare_output(env: &Env, tools: &Path, dir: &Path, args: &[&str], fail: &str) -> Output {
    let path = format!(
        "{}:{}",
        tools.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    env.command(BIN, dir)
        .arg("worktree")
        .args(args)
        .env("PATH", path)
        .env("FAKE_LOG", env.path("fake.log"))
        .env("FAKE_FAIL", fail)
        .output()
        .unwrap()
}

fn fake_calls(env: &Env) -> Vec<String> {
    fs::read_to_string(env.path("fake.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn each_lockfile_picks_its_install() {
    let cases: [(&[&str], &str, &[&str]); 12] = [
        (&["package-lock.json"], "", &["npm ci"]),
        (&["pnpm-lock.yaml"], "", &["pnpm install --frozen-lockfile"]),
        (&["yarn.lock"], "", &["yarn install --frozen-lockfile"]),
        (
            &["yarn.lock", ".yarnrc.yml"],
            "",
            &["yarn install --immutable"],
        ),
        (
            &["yarn.lock", "package.json"],
            r#"{"packageManager":"yarn@4.1.0"}"#,
            &["yarn install --immutable"],
        ),
        (
            &["yarn.lock", "package.json"],
            r#"{"packageManager":"yarn@1.22.0"}"#,
            &["yarn install --frozen-lockfile"],
        ),
        (&["bun.lock"], "", &["bun install --frozen-lockfile"]),
        (
            &["bun.lockb", "bun.lock"],
            "",
            &["bun install --frozen-lockfile"],
        ),
        (&["uv.lock"], "", &["uv sync --frozen"]),
        (&["poetry.lock"], "", &["poetry install"]),
        (
            &["Cargo.lock", "go.sum"],
            "",
            &["cargo fetch --locked", "go mod download"],
        ),
        (
            &[".tool-versions", "package-lock.json"],
            "",
            &["mise install", "npm ci"],
        ),
    ];
    for (files, manifest, expected) in cases {
        let env = Env::new();
        let tools = fake_tools(&env);
        for file in files {
            let text = if *file == "package.json" {
                manifest
            } else {
                ""
            };
            fs::write(env.repo.join(file), text).unwrap();
        }
        let value = prepared(&env, &tools, &env.repo, &["prepare"]);
        let commands: Vec<&str> = value["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| step["command"].as_str().unwrap())
            .collect();
        assert_eq!(commands, expected, "{files:?}");
        assert_eq!(value["deps_from"], "install");
        for step in value["steps"].as_array().unwrap() {
            assert_eq!(step["exit"], 0);
            let log = fs::read_to_string(step["log"].as_str().unwrap()).unwrap();
            assert!(log.contains("ran "), "{log}");
            assert!(
                step["log"]
                    .as_str()
                    .unwrap()
                    .contains("/cache/agents/output/")
            );
            assert!(step["duration_ms"].is_u64());
        }
        assert_eq!(fake_calls(&env).len(), expected.len(), "{files:?}");
    }
}

#[test]
fn mise_is_skipped_without_the_executable_and_nothing_runs_without_a_lockfile() {
    let env = Env::new();
    let tools = fake_tools(&env);
    fs::remove_file(tools.join("mise")).unwrap();
    fs::write(env.repo.join("mise.toml"), "").unwrap();
    let output = env
        .command(BIN, &env.repo)
        .args(["worktree", "prepare"])
        .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["steps"].as_array().unwrap().len(), 0);
    assert_eq!(value["ok"], true);
}

/// A main checkout with a committed lockfile and one linked worktree `feat`.
fn linked() -> (Env, PathBuf, PathBuf) {
    let env = Env::new();
    let tools = fake_tools(&env);
    let main = env.repo.clone();
    fs::write(main.join("package-lock.json"), "{}").unwrap();
    commit(&env, &main, "package.json", "{}");
    let value = agt_json(&env, &main, &["create", "feat"]);
    let worktree = PathBuf::from(value["path"].as_str().unwrap());
    (env, tools, worktree)
}

#[test]
fn a_failed_install_with_the_main_lockfile_offers_a_clone() {
    let (env, tools, worktree) = linked();
    // The clone has something to copy only when the main checkout has a `node_modules`.
    fs::create_dir_all(env.repo.join("node_modules/dep")).unwrap();
    let output = prepare_output(&env, &tools, &worktree, &["prepare"], "npm ci");
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["steps"][0]["exit"], 1);
    assert_eq!(value["clone_available"], true);
    assert_eq!(value["steps"][0]["clone_available"], true);

    fs::write(worktree.join("package-lock.json"), "{\"changed\":1}").unwrap();
    let output = prepare_output(&env, &tools, &worktree, &["prepare"], "npm ci");
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["clone_available"], false);
}

#[test]
fn a_missing_tool_is_a_failed_step_with_an_error() {
    let env = Env::new();
    let tools = fake_tools(&env);
    fs::remove_file(tools.join("poetry")).unwrap();
    fs::write(env.repo.join("poetry.lock"), "").unwrap();
    let output = env
        .command(BIN, &env.repo)
        .args(["worktree", "prepare"])
        .env("PATH", format!("{}:/usr/bin:/bin", tools.display()))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["steps"][0]["ok"], false);
    assert_eq!(value["steps"][0]["exit"], Value::Null);
    assert!(
        value["steps"][0]["error"]
            .as_str()
            .unwrap()
            .contains("poetry")
    );
}

#[test]
fn clone_deps_copies_every_node_modules_then_runs_the_hooks_step() {
    let (env, tools, worktree) = linked();
    let main = env.repo.clone();
    for dir in ["node_modules/dep", "packages/web/node_modules/other"] {
        fs::create_dir_all(main.join(dir)).unwrap();
        fs::write(main.join(dir).join("index.js"), dir).unwrap();
    }
    fs::create_dir_all(main.join("node_modules/dep/node_modules/inner")).unwrap();
    fs::create_dir_all(worktree.join("packages/web")).unwrap();
    let value = prepared(&env, &tools, &worktree, &["prepare", "--clone-deps"]);
    assert_eq!(value["deps_from"], "clone");
    assert_eq!(
        value["cloned"],
        serde_json::json!(["node_modules", "packages/web/node_modules"])
    );
    assert_eq!(value["steps"][0]["command"], "npm run prepare --if-present");
    assert_eq!(value["steps"].as_array().unwrap().len(), 1);
    for dir in ["node_modules/dep", "packages/web/node_modules/other"] {
        let copy = worktree.join(dir).join("index.js");
        assert_eq!(fs::read_to_string(copy).unwrap(), dir);
    }
    assert!(
        !fs::symlink_metadata(worktree.join("node_modules"))
            .unwrap()
            .is_symlink()
    );
    assert!(
        worktree
            .join("node_modules/dep/node_modules/inner")
            .is_dir()
    );
    assert_eq!(fake_calls(&env), ["npm run prepare --if-present"]);
}

#[test]
fn clone_deps_fails_in_the_main_checkout() {
    let env = Env::new();
    let tools = fake_tools(&env);
    let output = prepare_output(&env, &tools, &env.repo, &["prepare", "--clone-deps"], "");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("main checkout"));
}

fn ports_file(env: &Env) -> PathBuf {
    env.path("state").join("agents/worktrees/ports.json")
}

fn port_of(env: &Env, dir: &Path) -> u64 {
    agt_json(env, dir, &["port"])["port"].as_u64().unwrap()
}

#[test]
fn a_worktree_keeps_its_port_and_each_worktree_has_its_own() {
    let (env, _) = repo_with_commit();
    let one = PathBuf::from(
        agt_json(&env, &env.repo, &["create", "one"])["path"]
            .as_str()
            .unwrap(),
    );
    let two = PathBuf::from(
        agt_json(&env, &env.repo, &["create", "two"])["path"]
            .as_str()
            .unwrap(),
    );
    let first = port_of(&env, &one);
    assert!((10000..=32767).contains(&first), "{first}");
    assert_eq!(port_of(&env, &one), first);
    assert_eq!(
        agt_json(&env, &env.repo, &["port", one.to_str().unwrap()])["port"],
        first
    );
    let other = port_of(&env, &two);
    assert_ne!(other, first);
    // From a subdirectory the worktree is the same one.
    fs::create_dir_all(one.join("sub")).unwrap();
    assert_eq!(port_of(&env, &one.join("sub")), first);
    let recorded: Value = serde_json::from_slice(&fs::read(ports_file(&env)).unwrap()).unwrap();
    assert_eq!(recorded.as_object().unwrap().len(), 2);
}

#[test]
fn env_prints_the_port_as_an_assignment() {
    let (env, _) = repo_with_commit();
    let port = port_of(&env, &env.repo);
    let output = agt(&env, &env.repo, &["port", "--env"]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("PORT={port}\n")
    );
}

#[test]
fn entries_for_worktrees_that_are_gone_are_dropped_on_the_next_write() {
    let (env, _) = repo_with_commit();
    let gone = PathBuf::from(
        agt_json(&env, &env.repo, &["create", "gone"])["path"]
            .as_str()
            .unwrap(),
    );
    let kept = PathBuf::from(
        agt_json(&env, &env.repo, &["create", "kept"])["path"]
            .as_str()
            .unwrap(),
    );
    port_of(&env, &gone);
    port_of(&env, &kept);
    fs::remove_dir_all(&gone).unwrap();
    let fresh = PathBuf::from(
        agt_json(&env, &env.repo, &["create", "fresh"])["path"]
            .as_str()
            .unwrap(),
    );
    port_of(&env, &fresh);
    let recorded: Value = serde_json::from_slice(&fs::read(ports_file(&env)).unwrap()).unwrap();
    let keys: Vec<&String> = recorded.as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert!(keys.iter().all(|key| !key.ends_with("/gone")));
}

#[test]
fn a_port_outside_a_git_work_tree_is_an_error() {
    let env = Env::new();
    let outside = env.path("plain");
    fs::create_dir_all(&outside).unwrap();
    let output = agt(&env, &env.repo, &["port", outside.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(!ports_file(&env).exists());
}

#[test]
fn a_branch_that_exists_only_on_origin_gets_a_tracking_local_branch() {
    let (env, first) = repo_with_commit();
    remote(&env, "origin.git", "origin");
    git(&env, &env.repo, &["push", "-q", "origin", "main"]);
    let work = env.path("scratch");
    git(
        &env,
        &env.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "remote-only",
            work.to_str().unwrap(),
        ],
    );
    let tip = commit(&env, &work, "remote-file", "r");
    git(&env, &env.repo, &["push", "-q", "origin", "remote-only"]);
    git(
        &env,
        &env.repo,
        &["worktree", "remove", "--force", work.to_str().unwrap()],
    );
    git(&env, &env.repo, &["branch", "-D", "remote-only"]);
    git(&env, &env.repo, &["fetch", "-q", "origin"]);

    let value = agt_json(&env, &env.repo, &["create", "remote-only"]);
    assert_eq!(value["reused"], false);
    assert_eq!(value["tracking"], "origin/remote-only");
    assert_eq!(value["commit"], tip.as_str());
    assert_ne!(tip, first);
    let dir = env.repo.join(".worktrees/remote-only");
    assert_eq!(git(&env, &dir, &["rev-parse", "HEAD"]), tip);
    assert_eq!(
        git(&env, &dir, &["branch", "--show-current"]),
        "remote-only"
    );
    assert_eq!(
        git(
            &env,
            &dir,
            &["rev-parse", "--abbrev-ref", "remote-only@{upstream}"]
        ),
        "origin/remote-only"
    );
}

#[test]
fn a_local_and_a_remote_branch_keep_the_local_one() {
    let (env, first) = repo_with_commit();
    remote(&env, "origin.git", "origin");
    git(&env, &env.repo, &["push", "-q", "origin", "main"]);
    git(&env, &env.repo, &["branch", "both"]);
    git(&env, &env.repo, &["push", "-q", "origin", "both"]);
    let local = commit(&env, &env.repo, "local-only", "l");
    git(&env, &env.repo, &["branch", "-f", "both", &local]);
    let value = agt_json(&env, &env.repo, &["create", "both"]);
    assert_eq!(value["commit"], local.as_str());
    assert!(value.get("tracking").is_none());
    assert_ne!(local, first);
}

#[test]
fn a_branch_pushed_after_the_last_fetch_is_fetched_and_tracked() {
    let (env, _first) = repo_with_commit();
    let bare = remote(&env, "origin.git", "origin");
    git(&env, &env.repo, &["push", "-q", "origin", "main"]);
    git(&env, &env.repo, &["fetch", "-q", "origin"]);
    // Another clone pushes a branch this repository has never fetched.
    let other = env.path("other-clone");
    git(
        &env,
        &env.repo,
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    git(&env, &other, &["switch", "-q", "-c", "late"]);
    let tip = commit(&env, &other, "late-file", "late");
    git(&env, &other, &["push", "-q", "origin", "late"]);
    assert_eq!(
        git(&env, &env.repo, &["branch", "-r", "--list", "origin/late"]),
        ""
    );

    let value = agt_json(&env, &env.repo, &["create", "late"]);
    assert_eq!(value["tracking"], "origin/late");
    assert_eq!(value["commit"], tip.as_str());
    let dir = env.repo.join(".worktrees/late");
    assert_eq!(git(&env, &dir, &["rev-parse", "HEAD"]), tip);
    assert!(dir.join("late-file").exists());
}

#[test]
fn a_branch_origin_does_not_have_still_starts_new_and_an_unreachable_origin_is_an_error() {
    let (env, first) = repo_with_commit();
    remote(&env, "origin.git", "origin");
    git(&env, &env.repo, &["push", "-q", "origin", "main"]);
    let value = agt_json(&env, &env.repo, &["create", "brand-new"]);
    assert!(value.get("tracking").is_none());
    assert_eq!(value["base"]["commit"], first.as_str());

    git(
        &env,
        &env.repo,
        &[
            "remote",
            "set-url",
            "origin",
            env.path("gone.git").to_str().unwrap(),
        ],
    );
    let output = agt(&env, &env.repo, &["create", "unknowable"]);
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("unknowable") && error.contains("--base"),
        "{error}"
    );
    assert!(!env.repo.join(".worktrees/unknowable").exists());
    // --base starts the new branch without asking origin.
    let value = agt_json(&env, &env.repo, &["create", "unknowable", "--base", "main"]);
    assert_eq!(value["base"]["commit"], first.as_str());
}

/// A main checkout with `package-lock.json` and `uv.lock` committed and a linked worktree.
fn linked_with_uv(node_modules: bool) -> (Env, PathBuf, PathBuf) {
    let env = Env::new();
    let tools = fake_tools(&env);
    let main = env.repo.clone();
    fs::write(main.join("package-lock.json"), "{}").unwrap();
    fs::write(main.join("uv.lock"), "lock").unwrap();
    commit(&env, &main, "package.json", "{}");
    if node_modules {
        fs::create_dir_all(main.join("node_modules/dep")).unwrap();
    }
    let value = agt_json(&env, &main, &["create", "feat"]);
    (env, tools, PathBuf::from(value["path"].as_str().unwrap()))
}

#[test]
fn a_failed_non_node_install_never_offers_a_clone() {
    let (env, tools, worktree) = linked_with_uv(true);
    let output = prepare_output(&env, &tools, &worktree, &["prepare"], "uv sync");
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let steps = value["steps"].as_array().unwrap();
    let uv = steps
        .iter()
        .find(|s| s["command"] == "uv sync --frozen")
        .unwrap();
    assert_eq!(uv["ok"], false);
    assert!(uv.get("clone_available").is_none(), "{uv}");
    assert_eq!(value["clone_available"], false);

    // A failed node install is not cloneable either when the main checkout has no node_modules.
    let (env, tools, worktree) = linked_with_uv(false);
    let output = prepare_output(&env, &tools, &worktree, &["prepare"], "npm ci");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["clone_available"], false);
}

#[test]
fn clone_deps_still_runs_the_other_installs_and_fails_when_one_fails() {
    let (env, tools, worktree) = linked_with_uv(true);
    let value = prepared(&env, &tools, &worktree, &["prepare", "--clone-deps"]);
    assert_eq!(value["deps_from"], "clone");
    let commands: Vec<&str> = value["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["command"].as_str().unwrap())
        .collect();
    assert_eq!(
        commands,
        ["npm run prepare --if-present", "uv sync --frozen"]
    );
    assert!(worktree.join("node_modules/dep").is_dir());

    let (env, tools, worktree) = linked_with_uv(true);
    let output = prepare_output(
        &env,
        &tools,
        &worktree,
        &["prepare", "--clone-deps"],
        "uv sync",
    );
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["deps_from"], "clone");
}
