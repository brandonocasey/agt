//! `agt pr --local`: a local branch is the PR, and merging it moves the local default branch. Every
//! test runs in its own temporary repository with a temporary HOME and state; no forge tool is
//! called and nothing reaches a network.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::Value;

use crate::support::Env;

/// A repository with `main` at one commit and `main` checked out.
fn setup() -> Env {
    let env = Env::new();
    env.write("a.txt", "a\n");
    env.commit_all("first");
    env
}

fn local(env: &Env, args: &[&str]) -> Output {
    local_in(env, &env.repo, args)
}

fn local_in(env: &Env, dir: &Path, args: &[&str]) -> Output {
    env.bin(dir)
        .args(["pr", "--local"])
        .args(args)
        .output()
        .unwrap()
}

fn ok(env: &Env, args: &[&str]) -> Value {
    let output = local(env, args);
    assert!(
        output.status.success(),
        "agt pr --local {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The stderr of a command that failed with status 1.
fn err(env: &Env, args: &[&str]) -> String {
    let output = local(env, args);
    assert_eq!(
        output.status.code(),
        Some(1),
        "agt pr --local {args:?} should fail: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn git_in(env: &Env, dir: &Path, args: &[&str]) -> Output {
    env.command("git", dir).args(args).output().unwrap()
}

fn git_ok(env: &Env, dir: &Path, args: &[&str]) -> String {
    let output = git_in(env, dir, args);
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Creates local branch `name` from `main`'s tip with one commit that writes `file`, and leaves
/// the checkout where it was. The name goes in through `update-ref`, so it may be one that
/// `git branch` would refuse.
fn add_branch(env: &Env, name: &str, file: &str, content: &str) -> String {
    let sha = commit_on(env, "main", file, content);
    point(env, name, &sha);
    sha
}

fn point(env: &Env, branch: &str, sha: &str) {
    git_ok(
        env,
        &env.repo,
        &["update-ref", &format!("refs/heads/{branch}"), sha],
    );
}

/// A commit on top of `parent` whose tree is `parent`'s with `entry` (`mode,object,path`) added,
/// made without moving any branch or checkout.
fn commit_entry(env: &Env, parent: &str, entry: &str, message: &str) -> String {
    let index = env.path("index");
    let _ = fs::remove_file(&index);
    let with_index = |args: &[&str]| {
        let output = env
            .command("git", &env.repo)
            .env("GIT_INDEX_FILE", &index)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    with_index(&["read-tree", parent]);
    with_index(&["update-index", "--add", "--cacheinfo", entry]);
    let tree = with_index(&["write-tree"]);
    with_index(&["commit-tree", &tree, "-p", parent, "-m", message])
}

/// A commit on top of `parent` that writes `file`.
fn commit_on(env: &Env, parent: &str, file: &str, content: &str) -> String {
    let path = env.path("blob");
    fs::write(&path, content).unwrap();
    let blob = git_ok(
        env,
        &env.repo,
        &["hash-object", "-w", path.to_str().unwrap()],
    );
    commit_entry(
        env,
        parent,
        &format!("100644,{blob},{file}"),
        &format!("add {file}"),
    )
}

/// A commit on top of `parent` that records a submodule at `sub`.
fn commit_submodule(env: &Env, parent: &str) -> String {
    let entry = format!("160000,{},sub", tip(env, "main"));
    commit_entry(env, parent, &entry, "add a submodule")
}

/// Moves `main` on by one commit that writes `file`, through the checkout that holds it.
fn advance_main(env: &Env, file: &str, content: &str) -> String {
    env.write(file, content);
    env.commit_all(&format!("main adds {file}"))
}

fn tip(env: &Env, reference: &str) -> String {
    git_ok(env, &env.repo, &["rev-parse", reference])
}

/// A linked work tree at `<root>/<name>`, made with `git worktree add <path> <args...>`.
fn add_worktree(env: &Env, name: &str, args: &[&str]) -> PathBuf {
    let path = env.path(name);
    let mut command = vec!["worktree", "add", "-q"];
    let place = path.to_str().unwrap();
    // The path goes before the branch, after any option.
    let (options, rest): (Vec<&str>, Vec<&str>) =
        args.iter().partition(|arg| arg.starts_with("--"));
    command.extend(options);
    command.push(place);
    command.extend(rest);
    git_ok(env, &env.repo, &command);
    path
}

/// Every ref, and each work tree's head and status; two equal states mean nothing moved.
fn state(env: &Env) -> String {
    let mut text = git_ok(env, &env.repo, &["for-each-ref"]);
    let listing = git_ok(env, &env.repo, &["worktree", "list", "--porcelain"]);
    for path in listing
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
    {
        let path = Path::new(path);
        if !path.exists() {
            text.push_str(&format!("\n{}: missing", path.display()));
            continue;
        }
        text.push_str(&format!(
            "\n{}: {}\n{}",
            path.display(),
            git_ok(env, path, &["rev-parse", "HEAD"]),
            git_ok(env, path, &["status", "--ignored"])
        ));
    }
    text
}

/// Runs a merge that must fail and checks that no ref, head or work tree changed.
fn refused(env: &Env, args: &[&str]) -> String {
    let before = state(env);
    let message = err(env, args);
    assert_eq!(state(env), before, "{message}");
    message
}

fn merge_args<'a>(sha: &'a str, branch: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["merge", "--sha", sha];
    args.extend(extra);
    args.extend(["--", branch]);
    args
}

#[test]
fn view_list_and_merge_work_on_a_branch_of_a_temporary_repository() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");

    let view = ok(&env, &["view", "feat"]);
    assert_eq!(view["number"], Value::Null);
    assert_eq!(view["url"], Value::Null);
    assert_eq!(view["title"], "add b.txt");
    assert_eq!(view["state"], "open");
    assert_eq!(view["head_branch"], "feat");
    assert_eq!(view["base_branch"], "main");
    assert_eq!(view["head_sha"], sha.as_str());
    assert_eq!(view["author"], "Tester");
    assert_eq!(view["auto_merge"], "off");
    assert_eq!(view["mergeable"], "mergeable");

    let open = ok(&env, &["list"]);
    assert_eq!(open.as_array().unwrap().len(), 1);
    assert_eq!(open[0]["head_branch"], "feat");
    assert_eq!(
        ok(&env, &["list", "--state", "merged"]),
        Value::Array(vec![])
    );
    assert_eq!(
        ok(&env, &["list", "--branch", "none"]),
        Value::Array(vec![])
    );
    let mine = ok(&env, &["list", "--mine"]);
    assert_eq!(mine.as_array().unwrap().len(), 1);

    let merged = ok(&env, &merge_args(&sha, "feat", &[]));
    assert_eq!(merged["state"], "merged");
    assert_eq!(tip(&env, "main"), sha);
    assert_eq!(fs::read_to_string(env.repo.join("b.txt")).unwrap(), "b\n");
    assert_eq!(ok(&env, &["list"]), Value::Array(vec![]));
    assert_eq!(
        ok(&env, &["list", "--state", "all"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn view_without_a_branch_reads_the_current_one_and_a_detached_head_needs_a_name() {
    let env = setup();
    add_branch(&env, "feat", "b.txt", "b\n");
    git_ok(&env, &env.repo, &["switch", "-q", "feat"]);
    assert_eq!(ok(&env, &["view"])["head_branch"], "feat");

    git_ok(&env, &env.repo, &["switch", "-q", "--detach"]);
    assert!(err(&env, &["view"]).contains("HEAD is detached"));
    assert_eq!(ok(&env, &["view", "feat"])["head_branch"], "feat");
    assert!(err(&env, &["view", "main"]).contains("is the local default branch"));
    assert!(err(&env, &["view", "missing"]).contains("no local branch missing"));
}

#[test]
fn a_merge_updates_a_target_that_no_work_tree_has_checked_out() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    git_ok(&env, &env.repo, &["switch", "-q", "-c", "elsewhere"]);
    let before = tip(&env, "elsewhere");

    assert_eq!(ok(&env, &merge_args(&sha, "feat", &[]))["state"], "merged");
    assert_eq!(tip(&env, "main"), sha);
    assert_eq!(tip(&env, "elsewhere"), before);
    assert!(!env.repo.join("b.txt").exists());
}

#[test]
fn a_merge_in_a_linked_work_tree_that_holds_the_target_and_is_locked() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    git_ok(&env, &env.repo, &["switch", "-q", "-c", "elsewhere"]);
    let held = add_worktree(&env, "held", &["main"]);
    git_ok(
        &env,
        &env.repo,
        &["worktree", "lock", held.to_str().unwrap()],
    );

    ok(&env, &merge_args(&sha, "feat", &[]));
    assert_eq!(tip(&env, "main"), sha);
    assert_eq!(fs::read_to_string(held.join("b.txt")).unwrap(), "b\n");
}

#[test]
fn a_target_whose_work_tree_is_gone_is_refused_until_it_is_pruned() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    git_ok(&env, &env.repo, &["switch", "-q", "-c", "elsewhere"]);
    let held = add_worktree(&env, "held", &["main"]);
    fs::remove_dir_all(&held).unwrap();

    let message = refused(&env, &merge_args(&sha, "feat", &[]));
    assert!(message.contains("no longer exists"), "{message}");
}

#[test]
fn a_merge_into_a_sparse_checkout_leaves_the_paths_outside_it_unwritten() {
    let env = setup();
    env.write("sub/keep.txt", "k\n");
    env.commit_all("sub");
    let first = commit_on(&env, "main", "other/x.txt", "x\n");
    let sha = commit_on(&env, &first, "sub/y.txt", "y\n");
    point(&env, "feat", &sha);
    git_ok(
        &env,
        &env.repo,
        &["sparse-checkout", "set", "--no-cone", "/sub/"],
    );

    ok(&env, &merge_args(&sha, "feat", &[]));
    assert_eq!(tip(&env, "main"), sha);
    assert_eq!(
        fs::read_to_string(env.repo.join("sub/y.txt")).unwrap(),
        "y\n"
    );
    assert!(!env.repo.join("other").exists());
}

#[test]
fn a_wrong_sha_a_short_sha_and_a_branch_behind_the_target_are_refused() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    let wrong = tip(&env, "main");
    let message = refused(&env, &merge_args(&wrong, "feat", &[]));
    assert!(message.contains("inspect it again"), "{message}");
    refused(&env, &merge_args(&sha[..6], "feat", &[]));

    advance_main(&env, "c.txt", "c\n");
    let message = refused(&env, &merge_args(&sha, "feat", &[]));
    assert!(message.contains("is behind main"), "{message}");
    assert!(message.contains("--method rebase"), "{message}");
}

#[test]
fn local_changes_and_ignored_files_in_the_target_work_tree_stop_the_merge() {
    let env = setup();
    let sha = add_branch(&env, "feat", "a.txt", "from the branch\n");
    env.write("a.txt", "mine, uncommitted\n");
    let message = refused(&env, &merge_args(&sha, "feat", &[]));
    assert!(message.contains("refused"), "{message}");
    assert_eq!(
        fs::read_to_string(env.repo.join("a.txt")).unwrap(),
        "mine, uncommitted\n"
    );
    assert_eq!(git_ok(&env, &env.repo, &["stash", "list"]), "");
}

#[test]
fn an_untracked_file_the_branch_would_write_stops_the_merge() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "from the branch\n");
    env.write("b.txt", "mine\n");
    refused(&env, &merge_args(&sha, "feat", &[]));
    assert_eq!(
        fs::read_to_string(env.repo.join("b.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn an_ignored_file_at_a_path_the_branch_writes_stops_the_merge() {
    let env = setup();
    env.write(".gitignore", "*.log\n");
    env.commit_all("ignore logs");
    let sha = add_branch(&env, "feat", "out.log", "tracked\n");
    env.write("out.log", "mine, ignored\n");
    let message = refused(&env, &merge_args(&sha, "feat", &[]));
    assert!(message.contains("ignored"), "{message}");
    assert_eq!(
        fs::read_to_string(env.repo.join("out.log")).unwrap(),
        "mine, ignored\n"
    );
}

#[test]
fn merge_autostash_does_not_stash_a_dirty_target() {
    let env = setup();
    let sha = add_branch(&env, "feat", "a.txt", "from the branch\n");
    git_ok(&env, &env.repo, &["config", "merge.autoStash", "true"]);
    git_ok(&env, &env.repo, &["config", "rebase.autoStash", "true"]);
    env.write("a.txt", "mine, uncommitted\n");
    refused(&env, &merge_args(&sha, "feat", &[]));
    assert_eq!(git_ok(&env, &env.repo, &["stash", "list"]), "");
    assert_eq!(
        fs::read_to_string(env.repo.join("a.txt")).unwrap(),
        "mine, uncommitted\n"
    );
}

#[test]
fn a_target_held_by_two_work_trees_is_refused() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    add_worktree(&env, "second", &["--force", "main"]);
    let message = refused(&env, &merge_args(&sha, "feat", &[]));
    assert!(message.contains("2 work trees"), "{message}");
}

#[test]
fn a_target_being_rebased_elsewhere_is_refused() {
    let env = setup();
    env.write("a.txt", "main\n");
    env.commit_all("main edits a");
    git_ok(&env, &env.repo, &["switch", "-q", "-c", "conf", "main~1"]);
    env.write("a.txt", "conf\n");
    env.commit_all("conf edits a");
    // `main` is free now; the feature branch descends from it.
    let sha = add_branch(&env, "feat", "b.txt", "b\n");

    let rebasing = add_worktree(&env, "rebasing", &["main"]);
    let rebase = git_in(&env, &rebasing, &["rebase", "conf"]);
    assert!(
        !rebase.status.success(),
        "the rebase should stop on a conflict"
    );
    let message = refused(&env, &merge_args(&sha, "feat", &[]));
    assert!(message.contains("git fetch refused"), "{message}");
    assert!(git_ok(&env, &rebasing, &["status"]).contains("rebase in progress"));
}

/// A refusal from git comes with the command that repeats the move once the cause is gone.
#[test]
fn a_refused_merge_gives_the_command_to_retry() {
    let env = setup();
    env.write("a.txt", "main\n");
    env.commit_all("main edits a");
    git_ok(&env, &env.repo, &["switch", "-q", "-c", "other", "main~1"]);
    let sha = add_branch(&env, "feat", "b.txt", "b\n");

    let rebasing = add_worktree(&env, "rebasing", &["main"]);
    let rebase = git_in(&env, &rebasing, &["rebase", "--exec", "false", "HEAD~1"]);
    assert!(!rebase.status.success(), "the exec should stop the rebase");
    let message = refused(&env, &merge_args(&sha, "feat", &[]));
    assert!(message.contains("git fetch refused"), "{message}");
    assert!(
        message.contains(&format!(
            "retry with: agt pr --local merge feat --sha {sha}"
        )),
        "{message}"
    );
    assert!(git_ok(&env, &rebasing, &["status"]).contains("rebase in progress"));

    git_ok(&env, &rebasing, &["rebase", "--abort"]);
    ok(&env, &merge_args(&sha, "feat", &[]));
    assert_eq!(tip(&env, "main"), sha);
}

#[test]
fn a_target_being_bisected_elsewhere_is_refused() {
    let env = setup();
    for count in 1..=4 {
        advance_main(&env, &format!("c{count}.txt"), "c\n");
    }
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    git_ok(&env, &env.repo, &["switch", "-q", "-c", "elsewhere"]);

    let bisecting = add_worktree(&env, "bisecting", &["main"]);
    git_ok(&env, &bisecting, &["bisect", "start", "main", "main~4"]);
    assert!(git_ok(&env, &bisecting, &["status"]).contains("bisecting"));
    let message = refused(&env, &merge_args(&sha, "feat", &[]));
    assert!(message.contains("git fetch refused"), "{message}");
}

/// Git refuses to move a branch that another work tree is rebasing or bisecting, also when that
/// work tree's directory is gone and only its entry in the git directory is left, so a rebase of
/// the feature branch must not start: it would move the branch and then fail to land it.
#[test]
fn a_rebase_merge_into_a_target_rebased_or_bisected_elsewhere_changes_nothing() {
    for (operation, deleted) in [
        ("rebase", false),
        ("bisect", false),
        ("rebase", true),
        ("bisect", true),
    ] {
        let env = setup();
        let sha = add_branch(&env, "feat", "b.txt", "b\n");
        for count in 1..=4 {
            advance_main(&env, &format!("c{count}.txt"), "c\n");
        }
        git_ok(&env, &env.repo, &["switch", "-q", "-c", "elsewhere"]);
        let work = add_worktree(&env, "work", &["feat"]);
        let stuck = add_worktree(&env, "stuck", &["main"]);
        if operation == "rebase" {
            let stopped = git_in(&env, &stuck, &["rebase", "--exec", "false", "HEAD~1"]);
            assert!(!stopped.status.success(), "the exec should stop the rebase");
        } else {
            git_ok(&env, &stuck, &["bisect", "start", "main", "main~4"]);
        }
        if deleted {
            fs::remove_dir_all(&stuck).unwrap();
        }

        let message = refused(&env, &merge_args(&sha, "feat", &["--method", "rebase"]));
        let case = format!("{operation}, work tree deleted: {deleted}");
        assert!(
            message.contains(&stuck.display().to_string()),
            "{case}: {message}"
        );
        assert!(message.contains(operation), "{case}: {message}");
        assert_eq!(
            message.contains("git worktree prune"),
            deleted,
            "{case}: {message}"
        );
        assert!(
            !message.contains("git fetch refused"),
            "the check comes before the rebase: {case}: {message}"
        );
        assert_eq!(tip(&env, "feat"), sha);
        assert_eq!(git_ok(&env, &work, &["rev-parse", "HEAD"]), sha);
    }
}

/// A work tree whose directory is gone still holds the branch it was rebasing, so the message
/// says that and not that no work tree has the branch.
#[test]
fn a_rebase_merge_of_a_branch_rebased_in_a_deleted_work_tree_changes_nothing() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    advance_main(&env, "c.txt", "c\n");
    git_ok(&env, &env.repo, &["switch", "-q", "-c", "elsewhere"]);
    let stuck = add_worktree(&env, "stuck", &["feat"]);
    let stopped = git_in(&env, &stuck, &["rebase", "--exec", "false", "main"]);
    assert!(!stopped.status.success(), "the exec should stop the rebase");
    fs::remove_dir_all(&stuck).unwrap();

    let message = refused(&env, &merge_args(&sha, "feat", &["--method", "rebase"]));
    assert!(
        message.contains(&format!("{}, which no longer exists", stuck.display())),
        "{message}"
    );
    assert_eq!(tip(&env, "feat"), sha);
}

#[test]
fn a_moved_submodule_pointer_is_refused() {
    let env = setup();
    let sha = commit_submodule(&env, "main");
    point(&env, "feat", &sha);
    let message = refused(&env, &merge_args(&sha, "feat", &[]));
    assert!(message.contains("a submodule moves"), "{message}");
}

#[test]
fn the_merge_methods_that_create_commits_are_refused() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    for method in ["merge", "squash"] {
        let message = refused(&env, &merge_args(&sha, "feat", &["--method", method]));
        assert!(message.contains("creates no commit"), "{message}");
    }
}

#[test]
fn a_branch_behind_the_target_rebases_and_fast_forwards() {
    let env = setup();
    let first = add_branch(&env, "feat", "b.txt", "b\n");
    let second = commit_on(&env, &first, "b2.txt", "b2\n");
    point(&env, "feat", &second);
    point(&env, "mid", &first);
    let old_main = advance_main(&env, "c.txt", "c\n");
    let work = add_worktree(&env, "work", &["feat"]);
    // Both are configured to move other branches and to stash, which a rebase here must not do.
    git_ok(&env, &env.repo, &["config", "rebase.updateRefs", "true"]);
    git_ok(&env, &env.repo, &["config", "rebase.autoStash", "true"]);

    let merged = ok(&env, &merge_args(&second, "feat", &["--method", "rebase"]));
    assert_eq!(merged["state"], "merged");
    let new_tip = tip(&env, "main");
    assert_ne!(new_tip, second);
    assert_eq!(tip(&env, "feat"), new_tip);
    assert_eq!(git_ok(&env, &env.repo, &["rev-parse", "main~2"]), old_main);
    assert_eq!(
        tip(&env, "mid"),
        first,
        "rebase.updateRefs must not move other branches"
    );
    assert_eq!(fs::read_to_string(work.join("c.txt")).unwrap(), "c\n");
    assert_eq!(fs::read_to_string(work.join("b2.txt")).unwrap(), "b2\n");
    assert_eq!(git_ok(&env, &work, &["status", "--porcelain"]), "");
}

#[test]
fn a_rebase_that_conflicts_leaves_the_branch_at_its_old_tip() {
    let env = setup();
    let sha = add_branch(&env, "feat", "a.txt", "feat\n");
    env.write("a.txt", "main\n");
    env.commit_all("main edits a");
    add_worktree(&env, "work", &["feat"]);

    let message = refused(&env, &merge_args(&sha, "feat", &["--method", "rebase"]));
    assert!(message.contains("conflicts in a.txt"), "{message}");
    assert!(message.contains(&sha), "{message}");
}

#[test]
fn a_rebase_never_aborts_one_the_user_started() {
    let env = setup();
    let sha = add_branch(&env, "feat", "a.txt", "feat\n");
    env.write("a.txt", "main\n");
    env.commit_all("main edits a");
    let work = add_worktree(&env, "work", &["feat"]);
    let own = git_in(&env, &work, &["rebase", "main"]);
    assert!(
        !own.status.success(),
        "the user's rebase should stop on a conflict"
    );

    let message = refused(&env, &merge_args(&sha, "feat", &["--method", "rebase"]));
    assert!(message.contains("rebase-merge in progress"), "{message}");
    assert!(
        !message.contains("not checked out in any work tree"),
        "{message}"
    );
    assert!(
        git_ok(&env, &work, &["status"]).contains("rebase in progress"),
        "the user's rebase is still there"
    );
}

#[test]
fn a_rebase_needs_the_branch_checked_out_in_exactly_one_clean_work_tree() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    advance_main(&env, "c.txt", "c\n");
    let rebase = merge_args(&sha, "feat", &["--method", "rebase"]);

    let message = refused(&env, &rebase);
    assert!(
        message.contains("not checked out in any work tree"),
        "{message}"
    );

    let first = add_worktree(&env, "first", &["feat"]);
    add_worktree(&env, "second", &["--force", "feat"]);
    let message = refused(&env, &rebase);
    assert!(message.contains("2 work trees"), "{message}");
    git_ok(
        &env,
        &env.repo,
        &[
            "worktree",
            "remove",
            "--force",
            env.path("second").to_str().unwrap(),
        ],
    );

    fs::write(first.join("dirty.txt"), "dirty\n").unwrap();
    let message = refused(&env, &rebase);
    assert!(message.contains("has local changes"), "{message}");
}

#[test]
fn a_rebase_stops_for_an_operation_in_progress_in_the_branchs_work_tree() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    advance_main(&env, "c.txt", "c\n");
    let work = add_worktree(&env, "work", &["feat"]);
    // Picking a commit the branch already has stops with a clean tree and a pick in progress.
    let pick = git_in(&env, &work, &["cherry-pick", "HEAD"]);
    assert!(!pick.status.success());

    let message = refused(&env, &merge_args(&sha, "feat", &["--method", "rebase"]));
    assert!(
        message.contains("CHERRY_PICK_HEAD in progress"),
        "{message}"
    );
}

/// Leaves `feat` checked out in a linked work tree and `other` a branch of `main` that edits
/// `a.txt` as `feat` does, so a rebase of `feat` onto `other` stops on a conflict.
fn conflicting_pair(env: &Env) -> (String, PathBuf) {
    let sha = add_branch(env, "feat", "a.txt", "feat\n");
    add_branch(env, "other", "a.txt", "other\n");
    (sha, add_worktree(env, "work", &["feat"]))
}

#[test]
fn a_fast_forward_stops_for_a_branch_held_by_two_work_trees() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    add_worktree(&env, "first", &["feat"]);
    add_worktree(&env, "second", &["--force", "feat"]);

    let message = refused(&env, &merge_args(&sha, "feat", &[]));
    assert!(message.contains("2 work trees"), "{message}");
}

#[test]
fn a_fast_forward_stops_for_a_merge_in_progress_in_the_branchs_work_tree() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    add_branch(&env, "other", "c.txt", "c\n");
    let work = add_worktree(&env, "work", &["feat"]);
    git_ok(&env, &work, &["merge", "--no-commit", "--no-ff", "other"]);

    let message = refused(&env, &merge_args(&sha, "feat", &[]));
    assert!(message.contains("MERGE_HEAD in progress"), "{message}");
}

#[test]
fn a_fast_forward_stops_for_a_rebase_the_user_started_and_leaves_it() {
    let env = setup();
    let (sha, work) = conflicting_pair(&env);
    let own = git_in(&env, &work, &["rebase", "other"]);
    assert!(
        !own.status.success(),
        "the rebase should stop on a conflict"
    );

    let message = refused(&env, &merge_args(&sha, "feat", &[]));
    assert!(message.contains("rebase-merge in progress"), "{message}");
    assert!(
        git_ok(&env, &work, &["status"]).contains("rebase in progress"),
        "the user's rebase is still there"
    );
}

#[test]
fn a_rebase_stops_for_an_ignored_file_it_would_write() {
    let env = setup();
    env.write(".gitignore", "*.log\n");
    env.commit_all("ignore logs");
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    env.write("out.log", "tracked on main\n");
    git_ok(&env, &env.repo, &["add", "-f", "out.log"]);
    git_ok(&env, &env.repo, &["commit", "-q", "-m", "track a log"]);
    let work = add_worktree(&env, "work", &["feat"]);
    fs::write(work.join("out.log"), "ignored, mine\n").unwrap();

    let message = refused(&env, &merge_args(&sha, "feat", &["--method", "rebase"]));
    assert!(message.contains("the ignored file out.log"), "{message}");
    assert_eq!(
        fs::read_to_string(work.join("out.log")).unwrap(),
        "ignored, mine\n"
    );
}

#[test]
fn a_rebase_stops_for_a_moved_submodule_pointer() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    let entry = format!("160000,{},sub", tip(&env, "main"));
    git_ok(
        &env,
        &env.repo,
        &["update-index", "--add", "--cacheinfo", &entry],
    );
    git_ok(
        &env,
        &env.repo,
        &["commit", "-q", "-m", "main adds a submodule"],
    );
    add_worktree(&env, "work", &["feat"]);

    let message = refused(&env, &merge_args(&sha, "feat", &["--method", "rebase"]));
    assert!(message.contains("a submodule moves"), "{message}");
}

#[test]
fn a_hook_that_refuses_the_rebase_leaves_everything_as_it_was() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    advance_main(&env, "c.txt", "c\n");
    add_worktree(&env, "work", &["feat"]);
    let hook = env.repo.join(".git/hooks/pre-rebase");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    crate::support::executable(&hook, "#!/bin/sh\necho no rebases today >&2\nexit 1\n");

    let message = refused(&env, &merge_args(&sha, "feat", &["--method", "rebase"]));
    assert!(message.contains("no rebases today"), "{message}");
}

#[test]
fn branch_names_with_unicode_a_slash_or_a_leading_dash_are_found_and_merged() {
    for name in ["fé/ü", "a/b/c", "-dash", "-dash/é"] {
        let env = setup();
        let sha = add_branch(&env, name, "b.txt", "b\n");
        assert_eq!(ok(&env, &["view", "--", name])["head_branch"], name);
        assert_eq!(ok(&env, &["list"])[0]["head_branch"], name);
        let only = format!("--branch={name}");
        assert_eq!(ok(&env, &["list", &only])[0]["head_branch"], name);

        let merged = ok(&env, &merge_args(&sha, name, &[]));
        assert_eq!(merged["state"], "merged", "{name}");
        assert_eq!(tip(&env, "main"), sha, "{name}");
    }
}

#[test]
fn a_rebase_works_for_a_leading_dash_branch_in_a_work_tree_with_a_space_and_unicode() {
    let env = setup();
    let sha = add_branch(&env, "-dash/é", "b.txt", "b\n");
    advance_main(&env, "c.txt", "c\n");
    let work = add_worktree(&env, "work tree é", &["--detach", "main"]);
    // `git worktree add` takes no branch whose name starts with a dash; the work tree is
    // pointed at it afterwards.
    git_ok(&env, &work, &["symbolic-ref", "HEAD", "refs/heads/-dash/é"]);
    git_ok(&env, &work, &["reset", "-q", "--hard"]);

    ok(&env, &merge_args(&sha, "-dash/é", &["--method", "rebase"]));
    assert_eq!(tip(&env, "main"), tip(&env, "refs/heads/-dash/é"));
    assert_eq!(fs::read_to_string(work.join("c.txt")).unwrap(), "c\n");
}

#[test]
fn a_repository_without_commits_has_no_local_default_branch() {
    let env = Env::new();
    for args in [&["list"][..], &["view", "feat"]] {
        let message = err(&env, args);
        assert!(message.contains("no local default branch"), "{message}");
    }
}

#[test]
fn a_merge_closes_the_local_issues_any_commit_names() {
    let env = setup();
    let named = env.issue(&["create", "--title", "Named"])["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let first = add_branch(&env, "feat", "b.txt", "b\n");
    let second = commit_on(&env, &first, "b2.txt", "b2\n");
    // The closing keyword is in the older commit; the tip's message has none.
    let older = git_ok(
        &env,
        &env.repo,
        &[
            "commit-tree",
            &format!("{first}^{{tree}}"),
            "-p",
            "main",
            "-m",
            &format!("older\n\nFixes {named}"),
        ],
    );
    let tip_on_older = git_ok(
        &env,
        &env.repo,
        &[
            "commit-tree",
            &format!("{second}^{{tree}}"),
            "-p",
            &older,
            "-m",
            "newer",
        ],
    );
    point(&env, "feat", &tip_on_older);

    let merged = ok(&env, &merge_args(&tip_on_older, "feat", &[]));
    assert_eq!(merged["closed_issues"], serde_json::json!([named]));
    assert_eq!(env.issue(&["show", &named])["state"], "closed");
}

#[test]
fn local_is_never_chosen_for_remote_and_needs_no_tracker_network_or_push() {
    let env = Env::new().offline();
    env.write_config("[sync]\npush = \"off\"\n");
    env.write("a.txt", "a\n");
    env.commit_all("first");
    let sha = add_branch(&env, "feat", "b.txt", "b\n");

    let both = local(&env, &["--remote", "origin", "view", "feat"]);
    assert_eq!(both.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&both.stderr).contains("cannot be used with"));

    // Without --local the same repository has no tracker to talk to.
    let plain = env
        .bin(&env.repo)
        .args(["pr", "view", "feat"])
        .output()
        .unwrap();
    assert_eq!(plain.status.code(), Some(1));

    assert_eq!(ok(&env, &merge_args(&sha, "feat", &[]))["state"], "merged");
}

#[test]
fn a_local_pr_has_no_title_body_label_or_image_to_store() {
    let env = setup();
    add_branch(&env, "feat", "b.txt", "b\n");
    git_ok(&env, &env.repo, &["switch", "-q", "feat"]);
    for args in [
        &["create", "--title", "T"][..],
        &["update", "feat", "--title", "T"],
        &["upload", "missing.png"],
    ] {
        let message = err(&env, args);
        assert!(message.contains("not available"), "{message}");
        assert!(message.contains("the branch itself"), "{message}");
    }
}

/// A wait that skips its sleeps, so a call that gives up does it at once.
fn waiting(env: &Env, args: &[&str]) -> Output {
    waiting_in(env, &env.repo, args)
}

fn waiting_in(env: &Env, dir: &Path, args: &[&str]) -> Output {
    env.bin(dir)
        .env("AGT_NOW", "1000000")
        .args(["pr", "--local"])
        .args(args)
        .output()
        .unwrap()
}

/// A call that really sleeps between its looks, started and left running.
fn start_waiter(env: &Env, args: &[&str]) -> std::process::Child {
    env.bin(&env.repo)
        .args(["pr", "--local"])
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

/// Waits until `branch` reports auto-merge on, which the waiter has recorded by then.
fn wait_until_on(env: &Env, branch: &str) {
    for _ in 0..400 {
        if ok(env, &["view", "--", branch])["auto_merge"] == "on" {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("{branch} never reported auto-merge on");
}

/// Records the gates for `branch`, except those named in `skip`: a check pass, a review, and an
/// approval at the head. The approval goes through `publish --approve --dry-run`, which records
/// it before it looks for a tracker and fails here, where there is none.
fn record_gates(env: &Env, branch: &str, skip: &[&str]) {
    git_ok(env, &env.repo, &["switch", "-q", branch]);
    if !skip.contains(&"checks") {
        let output = env
            .bin(&env.repo)
            .args(["check", "run", "--name", "t", "--", "true"])
            .output()
            .unwrap();
        assert!(output.status.success());
    }
    if !skip.contains(&"review") {
        env.review("full");
    }
    if !skip.contains(&"approval") {
        let output = env
            .agt(&env.repo)
            .args(["publish", "--dry-run", "--approve"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&output.stderr).contains("no tracker"));
    }
    git_ok(env, &env.repo, &["switch", "-q", "main"]);
}

/// The auto-merge intents on record.
fn intents(env: &Env) -> Value {
    let file = env.path("state").join("agents/pr/local-auto-merge.json");
    match fs::read(file) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap()["intents"].clone(),
        Err(_) => serde_json::json!({}),
    }
}

fn stdout_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn auto_merge_merges_at_once_when_all_four_gates_pass() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    record_gates(&env, "feat", &[]);

    let output = waiting(&env, &merge_args(&sha, "feat", &["--auto"]));
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout_json(&output)["state"], "merged");
    assert_eq!(tip(&env, "main"), sha);
    assert_eq!(intents(&env), serde_json::json!({}));
}

/// An auto-merge whose gates all pass lands through the same merge as `merge`: it fails when
/// the branch is held by two work trees or a rebase of it is in progress, with nothing changed.
#[test]
fn an_auto_merge_stops_for_a_branch_held_by_two_work_trees() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    record_gates(&env, "feat", &[]);
    add_worktree(&env, "first", &["feat"]);
    add_worktree(&env, "second", &["--force", "feat"]);
    let before = state(&env);

    let output = waiting(&env, &merge_args(&sha, "feat", &["--auto"]));
    assert!(!output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("2 work trees"),
        "{}",
        stderr(&output)
    );
    assert_eq!(state(&env), before);
}

#[test]
fn an_auto_merge_stops_for_a_rebase_the_user_started_and_leaves_it() {
    let env = setup();
    let sha = add_branch(&env, "feat", "a.txt", "feat\n");
    add_branch(&env, "other", "a.txt", "other\n");
    record_gates(&env, "feat", &[]);
    let work = add_worktree(&env, "work", &["feat"]);
    let own = git_in(&env, &work, &["rebase", "other"]);
    assert!(
        !own.status.success(),
        "the rebase should stop on a conflict"
    );
    let before = state(&env);

    let output = waiting(&env, &merge_args(&sha, "feat", &["--auto"]));
    assert!(!output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("rebase-merge in progress"),
        "{}",
        stderr(&output)
    );
    assert_eq!(state(&env), before);
    assert!(git_ok(&env, &work, &["status"]).contains("rebase in progress"));
}

/// A branch whose `gate` is not recorded gives up with exit 3, names that gate alone and keeps
/// the intent.
fn assert_gate_times_out(gate: &str) {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    record_gates(&env, "feat", &[gate]);
    let main = tip(&env, "main");

    let output = waiting(&env, &merge_args(&sha, "feat", &["--auto"]));
    assert_eq!(output.status.code(), Some(3), "{gate}: {}", stderr(&output));
    let message = stderr(&output);
    assert!(message.contains(&format!("{gate}:")), "{message}");
    for other in ["checks", "review", "approval"]
        .iter()
        .filter(|g| **g != gate)
    {
        assert!(!message.contains(&format!("{other}:")), "{gate}: {message}");
    }
    assert_eq!(stdout_json(&output)["auto_merge"], "on");
    assert_eq!(tip(&env, "main"), main);
    assert_eq!(intents(&env).as_object().unwrap().len(), 1);
}

#[test]
fn a_missing_check_pass_times_out_with_exit_3_and_keeps_the_intent() {
    assert_gate_times_out("checks");
}

#[test]
fn a_missing_review_times_out_with_exit_3_and_keeps_the_intent() {
    assert_gate_times_out("review");
}

#[test]
fn a_missing_approval_times_out_with_exit_3_and_keeps_the_intent() {
    assert_gate_times_out("approval");
}

#[test]
fn an_open_blocking_finding_fails_the_review_gate() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    git_ok(&env, &env.repo, &["switch", "-q", "feat"]);
    env.run(&["comment", "--type", "bug", "--body", "Broken."]);
    git_ok(&env, &env.repo, &["switch", "-q", "main"]);
    record_gates(&env, "feat", &["approval"]);

    let output = waiting(&env, &merge_args(&sha, "feat", &["--auto"]));
    assert_eq!(output.status.code(), Some(3));
    assert!(
        stderr(&output).contains("open findings"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_rerun_and_auto_merge_on_continue_a_wait_that_gave_up() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    let args = merge_args(&sha, "feat", &["--auto"]);
    assert_eq!(waiting(&env, &args).status.code(), Some(3));

    record_gates(&env, "feat", &[]);
    let again = waiting(&env, &args);
    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(tip(&env, "main"), sha);

    let sha = add_branch(&env, "second", "c.txt", "c\n");
    assert_eq!(
        waiting(&env, &merge_args(&sha, "second", &["--auto"]))
            .status
            .code(),
        Some(3)
    );
    record_gates(&env, "second", &[]);
    let on = waiting(&env, &["auto-merge", "on", "second"]);
    assert!(on.status.success(), "{}", stderr(&on));
    assert_eq!(stdout_json(&on)["merged"], true);
    assert_eq!(tip(&env, "main"), sha);
}

#[test]
fn a_plain_merge_ends_the_intent_of_a_branch_that_was_waiting() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    assert_eq!(
        waiting(&env, &merge_args(&sha, "feat", &["--auto"]))
            .status
            .code(),
        Some(3)
    );
    assert_eq!(intents(&env).as_object().unwrap().len(), 1);

    ok(&env, &merge_args(&sha, "feat", &[]));
    assert_eq!(intents(&env), serde_json::json!({}));
}

#[test]
fn the_branch_lands_gate_waits_on_a_branch_behind_the_target_unless_it_can_rebase() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    record_gates(&env, "feat", &[]);
    advance_main(&env, "c.txt", "c\n");
    let main = tip(&env, "main");

    let output = waiting(&env, &merge_args(&sha, "feat", &["--auto"]));
    assert_eq!(output.status.code(), Some(3));
    assert!(stderr(&output).contains("lands:"), "{}", stderr(&output));
    assert_eq!(tip(&env, "main"), main);

    // Rebasing needs the branch checked out in one work tree.
    let rebase = merge_args(&sha, "feat", &["--auto", "--method", "rebase"]);
    let output = waiting(&env, &rebase);
    assert_eq!(output.status.code(), Some(3));
    assert!(
        stderr(&output).contains("not checked out"),
        "{}",
        stderr(&output)
    );

    add_worktree(&env, "work", &["feat"]);
    let output = waiting(&env, &rebase);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_ne!(tip(&env, "main"), sha);
    assert_eq!(tip(&env, "main"), tip(&env, "feat"));
}

#[test]
fn a_rebase_that_conflicts_is_a_final_failure_that_clears_the_intent() {
    let env = setup();
    let sha = add_branch(&env, "feat", "a.txt", "feat\n");
    record_gates(&env, "feat", &[]);
    env.write("a.txt", "main\n");
    env.commit_all("main edits a");
    add_worktree(&env, "work", &["feat"]);

    let output = waiting(
        &env,
        &merge_args(&sha, "feat", &["--auto", "--method", "rebase"]),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("conflicts in a.txt"),
        "{}",
        stderr(&output)
    );
    assert_eq!(intents(&env), serde_json::json!({}));
    assert_eq!(tip(&env, "feat"), sha);
}

#[test]
fn a_gate_is_judged_for_the_named_branch_when_the_command_runs_from_another_work_tree() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    add_branch(&env, "other", "o.txt", "o\n");
    record_gates(&env, "feat", &[]);
    // `other` has no check, review or approval; its work tree is where the command runs.
    let elsewhere = add_worktree(&env, "elsewhere", &["other"]);

    let output = waiting_in(&env, &elsewhere, &merge_args(&sha, "feat", &["--auto"]));
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(tip(&env, "main"), sha);
}

#[test]
fn auto_merge_off_stops_a_waiter_and_on_restores_the_method_it_recorded() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    add_worktree(&env, "work", &["feat"]);
    let waiter = start_waiter(
        &env,
        &merge_args(&sha, "feat", &["--auto", "--method", "rebase"]),
    );
    wait_until_on(&env, "feat");

    let off = ok(&env, &["auto-merge", "off", "--", "feat"]);
    assert_eq!(off["changed"], true);
    assert_eq!(off["pr"]["auto_merge"], "off");
    let output = waiter.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("turned off"),
        "{}",
        stderr(&output)
    );
    assert_eq!(intents(&env), serde_json::json!({}));

    let on = waiting(&env, &["auto-merge", "on", "feat"]);
    assert_eq!(on.status.code(), Some(3), "{}", stderr(&on));
    assert_eq!(stdout_json(&on)["pr"]["auto_merge_method"], "rebase");
}

#[test]
fn auto_merge_on_with_nothing_recorded_fails() {
    let env = setup();
    add_branch(&env, "feat", "b.txt", "b\n");
    let message = err(&env, &["auto-merge", "on", "feat"]);
    assert!(message.contains("no paused auto-merge"), "{message}");
}

/// The hint names a command for a local branch, not the forge one.
#[test]
fn the_hint_for_a_missing_auto_merge_names_a_local_command() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    let message = err(&env, &["auto-merge", "on", "feat"]);
    let command = format!("agt pr --local merge feat --sha {sha} --auto");
    assert!(message.contains(&format!("`{command}`")), "{message}");
    assert!(!message.contains("`agt pr merge"), "{message}");
}

#[test]
fn a_second_waiter_replaces_the_first() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    let args = merge_args(&sha, "feat", &["--auto"]);
    let first = start_waiter(&env, &args);
    wait_until_on(&env, "feat");

    let second = waiting(&env, &args);
    assert_eq!(second.status.code(), Some(3));
    let output = first.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("took over"), "{}", stderr(&output));
    assert_eq!(ok(&env, &["view", "feat"])["auto_merge"], "on");
}

#[test]
fn a_waiter_whose_branch_moved_fails_and_clears_the_intent() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    let waiter = start_waiter(&env, &merge_args(&sha, "feat", &["--auto"]));
    wait_until_on(&env, "feat");

    let moved = commit_on(&env, &sha, "c.txt", "c\n");
    point(&env, "feat", &moved);
    let output = waiter.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("was cleared"),
        "{}",
        stderr(&output)
    );
    assert_eq!(intents(&env), serde_json::json!({}));
    assert_eq!(ok(&env, &["view", "feat"])["auto_merge"], "off");
}

#[test]
fn view_reports_auto_merge_only_while_the_intent_names_the_tip() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    assert_eq!(
        waiting(&env, &merge_args(&sha, "feat", &["--auto"]))
            .status
            .code(),
        Some(3)
    );
    assert_eq!(
        ok(&env, &["view", "feat"])["auto_merge_method"],
        "fast-forward"
    );

    let moved = commit_on(&env, &sha, "c.txt", "c\n");
    point(&env, "feat", &moved);
    assert_eq!(ok(&env, &["view", "feat"])["auto_merge"], "off");
    assert_eq!(intents(&env), serde_json::json!({}));
}

#[test]
fn a_head_that_is_not_the_sha_is_refused_before_any_intent_is_recorded() {
    let env = setup();
    let sha = add_branch(&env, "feat", "b.txt", "b\n");
    let moved = commit_on(&env, &sha, "c.txt", "c\n");
    point(&env, "feat", &moved);

    let message = err(&env, &merge_args(&sha, "feat", &["--auto"]));
    assert!(message.contains("inspect it again"), "{message}");
    assert_eq!(intents(&env), serde_json::json!({}));
}
