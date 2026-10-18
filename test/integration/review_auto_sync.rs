//! Auto-sync for `agt review` reads: a stale or unknown PR/MR is fetched before `list`, `show`
//! and `status`, a branch with no PR/MR is looked up once per max age, and a failure leaves the
//! local read with one line on stderr.

use std::fs;
use std::path::Path;
use std::process::Output;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::auto_sync::{LOCAL, SKIPPED, assert_gone, skipped_reason, slow_tool};
use crate::support::Env;
use crate::support::forge::{assert_read_only, github};
use crate::support::issue::NOW;
use crate::support::scenario::Setup;

const ORIGIN: &str = "git@github.com:me/repo.git";

fn review_now(env: &Env, now: i64, args: &[&str], more: &[(&str, &str)]) -> Output {
    let now = now.to_string();
    let mut vars = vec![("AGT_NOW", now.as_str())];
    vars.extend_from_slice(more);
    env.exec(args, None, &vars)
}

fn review_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn lookups(env: &Env) -> usize {
    env.forge_calls()
        .iter()
        .filter(|call| call.contains("pulls?head="))
        .count()
}

/// A reviewed branch whose PR exists on the host and is not yet imported.
fn unsynced_pr() -> Setup {
    let setup = Setup::synced_branch(ORIGIN);
    github::sync_fixtures(&setup);
    setup
}

#[test]
fn a_review_read_finds_the_pr_and_syncs_it() {
    let setup = unsynced_pr();
    let local = review_now(&setup.env, NOW, &["list"], &[("AGT_OFFLINE", "1")]);
    assert!(!String::from_utf8_lossy(&local.stdout).contains("gh-c2002"));
    assert!(setup.env.forge_calls().is_empty());
    for (args, wanted) in [
        (vec!["list"], "gh-c2002"),
        (vec!["status"], "gh-42"),
        (vec!["show", "gh-c2002"], "gh-c2002"),
    ] {
        let setup = unsynced_pr();
        let output = review_now(&setup.env, NOW, &args, &[]);
        assert_eq!(String::from_utf8_lossy(&output.stderr), "", "{args:?}");
        let text = review_json(&output).to_string();
        assert!(text.contains(wanted), "{args:?}: {text}");
        assert_read_only(&github::Github::new(), &setup.env);
    }
}

#[test]
fn a_synced_review_is_read_again_only_when_stale() {
    let setup = unsynced_pr();
    let env = &setup.env;
    let at = NOW;
    review_json(&review_now(env, at, &["sync"], &[]));
    let synced = env.forge_calls().len();
    // Fresh: nothing is called.
    let fresh = review_now(env, NOW + 299, &["status"], &[]);
    assert_eq!(String::from_utf8_lossy(&fresh.stderr), "");
    assert_eq!(env.forge_calls().len(), synced);
    // Stale: the PR is fetched again, with no lookup of the branch.
    let stale = review_now(env, NOW + 301, &["status"], &[]);
    assert_eq!(String::from_utf8_lossy(&stale.stderr), "");
    assert!(env.forge_calls().len() > synced);
    assert_eq!(lookups(env), 1);
    let target = env.run_with(&["status", "--full"], None, &[("AGT_OFFLINE", "1")]);
    assert_eq!(target["target"], "gh-42");
}

#[test]
fn offline_keeps_review_reads_local() {
    let setup = unsynced_pr();
    let env = &setup.env;
    let at = NOW;
    for output in [
        review_now(env, at, &["--offline", "status"], &[]),
        review_now(env, at, &["status"], &[("AGT_OFFLINE", "1")]),
    ] {
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
        assert!(output.status.success());
    }
    fs::create_dir_all(env.path("config").join("agt")).unwrap();
    fs::write(
        env.path("config").join("agt").join("config.toml"),
        "[sync]\nauto = false\n",
    )
    .unwrap();
    let output = review_now(env, at, &["list"], &[]);
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert!(env.forge_calls().is_empty());
}

#[test]
fn a_branch_without_a_pr_is_looked_up_once_per_window() {
    let setup = Setup::synced_branch(ORIGIN);
    let env = &setup.env;
    github::login(env);
    github::pulls(env, "me/repo", "[]");
    // The origin has no parent to look in.
    env.respond(&["api", "--method", "GET", "repos/me/repo"], "{}");
    let start = NOW;
    let local = review_now(env, start, &["status"], &[("AGT_OFFLINE", "1")]);
    for at in [start, NOW + 60, NOW + 299] {
        let output = review_now(env, at, &["status"], &[]);
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
        assert_eq!(output.stdout, local.stdout);
    }
    assert_eq!(lookups(env), 1, "{:?}", env.forge_calls());
    let output = review_now(env, NOW + 301, &["list"], &[]);
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert_eq!(lookups(env), 2);
    // An agt that does not know the lookup file reads the store as before.
    assert!(env.run_with(&["status"], None, &[("AGT_OFFLINE", "1")])["target"].is_string());
}

#[test]
fn a_failing_forge_leaves_the_local_review_read_and_one_line() {
    let setup = Setup::synced_branch(ORIGIN);
    let env = &setup.env;
    let at = NOW;
    let local = review_now(env, at, &["list"], &[("AGT_OFFLINE", "1")]);
    // The fake refuses every call it has no answer for.
    let output = review_now(env, at, &["list"], &[]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, local.stdout);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with(SKIPPED) && stderr.ends_with(LOCAL),
        "{stderr}"
    );
    // A failure pauses the lookup for the max age: no call and no line, then it is tried again.
    let calls = env.forge_calls().len();
    let paused = review_now(env, at, &["list"], &[]);
    assert_eq!(String::from_utf8_lossy(&paused.stderr), "");
    assert_eq!(env.forge_calls().len(), calls);
    skipped_reason(&review_now(env, at + 301, &["list"], &[]));
    assert!(env.forge_calls().len() > calls);
}

#[test]
fn a_slow_forge_is_killed_for_a_review_read() {
    let setup = Setup::synced_branch(ORIGIN);
    let env = &setup.env;
    let at = NOW;
    let local = review_now(env, at, &["list"], &[("AGT_OFFLINE", "1")]);
    let (tool, pid) = slow_tool(env);
    let started = Instant::now();
    let output = review_now(
        env,
        at,
        &["list"],
        &[("AGT_GH_BIN", &tool), ("AGT_SYNC_CAP_MS", "400")],
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, local.stdout);
    assert!(skipped_reason(&output).contains("timed out"));
    assert_gone(&pid);
}

/// The directory under `root` that holds target documents.
fn repo_dir(root: &Path) -> std::path::PathBuf {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir()
            && let Some(found) = holds_targets(&path)
        {
            return found;
        }
    }
    panic!("no repository directory under {}", root.display());
}

fn holds_targets(dir: &Path) -> Option<std::path::PathBuf> {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if let Some(found) = holds_targets(&path) {
                return Some(found);
            }
        } else if !name.starts_with('.') && name.ends_with(".json") {
            return Some(dir.to_owned());
        }
    }
    None
}

#[test]
fn a_held_lock_skips_the_review_sync() {
    let setup = unsynced_pr();
    let env = &setup.env;
    let at = NOW;
    review_now(env, at, &["status"], &[("AGT_OFFLINE", "1")]);
    let dir = repo_dir(&env.state());
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".auto-sync.publish.lock"))
        .unwrap();
    lock.lock().unwrap();
    let output = review_now(env, at, &["list"], &[]);
    assert!(output.status.success());
    assert!(skipped_reason(&output).contains("another agt is syncing"));
    assert!(env.forge_calls().is_empty());
}

#[test]
fn no_origin_or_an_unknown_host_skips_quietly() {
    let env = Env::with_branch();
    let output = review_now(&env, NOW, &["status"], &[]);
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert!(output.status.success());
    assert!(env.forge_calls().is_empty());
    env.git(&[
        "remote",
        "add",
        "origin",
        "git@git.example.invalid:me/repo.git",
    ]);
    let output = review_now(&env, NOW, &["status"], &[]);
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert!(output.status.success());
    // A skip that tried nothing leaves no lock or memory behind.
    assert!(!mentions(&env.state(), "auto-sync"));
}

#[test]
fn review_writes_do_not_sync() {
    let setup = unsynced_pr();
    let env = &setup.env;
    let at = NOW;
    review_json(&review_now(env, at, &["run", "start"], &[]));
    assert!(env.forge_calls().is_empty());
}

#[test]
fn a_failed_lookup_pauses_the_next_reads_for_the_max_age() {
    let setup = Setup::synced_branch(ORIGIN);
    let env = &setup.env;
    // The fake refuses every call it has no answer for.
    let first = review_now(env, NOW, &["list"], &[]);
    skipped_reason(&first);
    let tried = env.forge_calls().len();
    assert!(tried > 0);
    for at in [NOW + 1, NOW + 299] {
        let paused = review_now(env, at, &["list"], &[]);
        assert_eq!(String::from_utf8_lossy(&paused.stderr), "");
        assert_eq!(paused.stdout, first.stdout);
    }
    assert_eq!(env.forge_calls().len(), tried);
    let again = review_now(env, NOW + 301, &["list"], &[]);
    skipped_reason(&again);
    assert!(env.forge_calls().len() > tried);
}

#[test]
fn a_failed_sync_of_a_known_pr_pauses_and_a_success_clears_it() {
    let setup = unsynced_pr();
    let env = &setup.env;
    review_json(&review_now(env, NOW, &["sync"], &[]));
    // Stale, and the host now fails: the PR queries are answered by nothing.
    let pull = github::graphql("me", "repo", "42", "PullRequest");
    let pull: Vec<&str> = pull.iter().map(String::as_str).collect();
    env.fail_call(&pull);
    skipped_reason(&review_now(env, NOW + 301, &["status"], &[]));
    let tried = env.forge_calls().len();
    let paused = review_now(env, NOW + 400, &["status"], &[]);
    assert_eq!(String::from_utf8_lossy(&paused.stderr), "");
    assert_eq!(env.forge_calls().len(), tried);
    env.pass_call(&pull);
    let recovered = review_now(env, NOW + 301 + 301, &["status"], &[]);
    assert_eq!(String::from_utf8_lossy(&recovered.stderr), "");
    assert!(env.forge_calls().len() > tried);
}

#[test]
fn a_bad_config_does_not_break_a_local_review_read() {
    let setup = Setup::synced_branch(ORIGIN);
    let env = &setup.env;
    fs::create_dir_all(env.path("config").join("agt")).unwrap();
    fs::write(
        env.path("config").join("agt").join("config.toml"),
        "[sync]\nbogus = 1\n",
    )
    .unwrap();
    let local = review_now(env, NOW, &["list"], &[("AGT_OFFLINE", "1")]);
    let output = review_now(env, NOW, &["list"], &[]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, local.stdout);
    let reason = skipped_reason(&output);
    assert!(
        reason.contains("config.toml") && reason.contains("bogus"),
        "{reason}"
    );
    assert!(env.forge_calls().is_empty());
}

#[test]
fn a_newer_pr_of_the_branch_is_found_when_the_stored_one_is_closed() {
    let setup = Setup::synced_branch(ORIGIN);
    let env = &setup.env;
    github::login(env);
    github::pulls(env, "me/repo", r#"[{"number":42,"state":"closed"}]"#);
    github::empty_pr(
        env,
        "me/repo",
        "42",
        "CLOSED",
        "\"2026-10-10T00:00:00Z\"",
        &setup.head,
        &setup.base,
    );
    assert_eq!(
        review_json(&review_now(env, NOW, &["sync"], &[]))["target"],
        "gh-42"
    );
    // The branch gets a new PR.
    github::pulls(
        env,
        "me/repo",
        r#"[{"number":42,"state":"closed"},{"number":43,"state":"open"}]"#,
    );
    github::empty_pr(
        env,
        "me/repo",
        "43",
        "OPEN",
        "null",
        &setup.head,
        &setup.base,
    );
    let before = lookups(env);
    let output = review_now(env, NOW + 301, &["status"], &[]);
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    assert_eq!(lookups(env), before + 1);
    let status = env.run_with(&["status"], None, &[("AGT_OFFLINE", "1")]);
    assert_eq!(status["target"], "gh-43");
    // Once per window: the open PR is now the stored one and nothing more is asked.
    let asked = env.forge_calls().len();
    review_now(env, NOW + 302, &["status"], &[]);
    assert_eq!(env.forge_calls().len(), asked);
}

#[test]
fn a_closed_pr_with_nothing_newer_is_looked_up_once_per_window() {
    let setup = Setup::synced_branch(ORIGIN);
    let env = &setup.env;
    github::login(env);
    github::pulls(env, "me/repo", r#"[{"number":42,"state":"closed"}]"#);
    github::empty_pr(
        env,
        "me/repo",
        "42",
        "CLOSED",
        "\"2026-10-10T00:00:00Z\"",
        &setup.head,
        &setup.base,
    );
    review_json(&review_now(env, NOW, &["sync"], &[]));
    let before = lookups(env);
    for at in [NOW + 1, NOW + 60, NOW + 299] {
        let output = review_now(env, at, &["status"], &[]);
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    }
    assert_eq!(lookups(env), before + 1);
    review_now(env, NOW + 301, &["status"], &[]);
    assert_eq!(lookups(env), before + 2);
}

/// Whether a file under `root` has `needle` in its name.
fn mentions(root: &Path, needle: &str) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    entries.map(|entry| entry.unwrap().path()).any(|path| {
        path.file_name().unwrap().to_string_lossy().contains(needle)
            || (path.is_dir() && mentions(&path, needle))
    })
}
