//! Auto-sync for `agt review` reads: a stale or unknown PR/MR is fetched before `list`, `show`
//! and `status`, a branch with no PR/MR is looked up once per max age, and a failure leaves the
//! local read with one line on stderr.

use std::fs;
use std::path::Path;
use std::process::Output;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::auto_sync::{LOCAL, SKIPPED, skipped_reason};
use crate::support::forge::{assert_read_only, github};
use crate::support::issue::NOW;
use crate::support::process::{assert_gone, slow_tool};
use crate::support::scenario::Setup;
use crate::support::{Env, executable};

const ORIGIN: &str = "git@github.com:me/repo.git";

fn review_now(env: &Env, now: i64, args: &[&str], more: &[(&str, &str)]) -> Output {
    let now = now.to_string();
    let mut vars = vec![("AGT_NOW", now.as_str())];
    vars.extend_from_slice(more);
    let config = env.path("config").join("agt/config.toml");
    if !fs::read_to_string(&config).unwrap().contains("[sync]") {
        env.write_config("[sync]\nauto_pull = true\n");
    }
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
        "[sync]\nauto_pull = false\n",
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
    holds_targets(root).expect("repository holds targets")
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
    review_json(&review_now(
        env,
        at,
        &["pass", "start", "--kind", "full"],
        &[],
    ));
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

#[test]
fn a_slow_forge_costs_one_cap_and_one_line_for_a_closed_pr_that_is_also_stale() {
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
    // Past the max age both the branch lookup and the sync of PR 42 are due.
    let (tool, pid) = slow_tool(env);
    let calls = env.path("slow-calls");
    let script = fs::read_to_string(&tool).unwrap();
    executable(
        Path::new(&tool),
        script.replacen(
            "#!/bin/sh\n",
            &format!("#!/bin/sh\necho invoked >> '{}'\n", calls.display()),
            1,
        ),
    );
    let output = review_now(
        env,
        NOW + 301,
        &["status"],
        &[("AGT_GH_BIN", &tool), ("AGT_SYNC_CAP_MS", "1500")],
    );
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches("sync skipped").count(), 1, "{stderr}");
    assert!(stderr.contains("timed out after 1.5s"), "{stderr}");
    assert_eq!(fs::read_to_string(calls).unwrap().lines().count(), 1);
    let saved: Value =
        serde_json::from_slice(&fs::read(repo_dir(&env.state()).join(".auto-sync.json")).unwrap())
            .unwrap();
    let failures = saved["failures"].as_object().unwrap();
    assert_eq!(failures.len(), 2);
    assert!(
        failures.keys().any(|key| key.starts_with("branch-")),
        "{saved}"
    );
    assert!(saved["failures"].get("pr-gh-42").is_some(), "{saved}");
    assert_gone(&pid);
}

fn auto_push(env: &Env) {
    env.write_config("[sync]\nauto_push = true\n");
}

#[test]
fn default_review_commands_never_call_a_forge() {
    let setup = Setup::reviewed(ORIGIN);
    let env = &setup.env;
    github::sync_fixtures(&setup);
    for args in [
        vec!["list"],
        vec!["status"],
        vec!["run", "end", "--run", &setup.run],
    ] {
        review_json(&env.exec(&args, None, &[]));
    }
    assert!(env.forge_calls().is_empty());
}

#[test]
fn offline_blocks_explicit_review_network_commands() {
    let setup = Setup::reviewed(ORIGIN);
    for args in [
        vec!["--offline", "sync"],
        vec!["--offline", "publish", "--dry-run"],
        vec!["publish"],
    ] {
        let output = setup.env.exec(&args, None, &[("AGT_OFFLINE", "1")]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("offline"));
    }
    assert!(setup.env.forge_calls().is_empty());
}

#[test]
fn automatic_publish_attaches_and_sends_only_the_completed_run() {
    use crate::support::forge::{Call, FakeService, Remote};
    use crate::support::scenario::{Review, start_review};
    let old = Review {
        reviewers: crate::support::scenario::PUBLISH.reviewers,
        findings: r#"[{"type":"nit","scenario":"OLD PRIVATE FINDING"}]"#,
    };
    let current = Review {
        reviewers: crate::support::scenario::PUBLISH.reviewers,
        findings: r#"[{"type":"nit","scenario":"CURRENT FINDING"}]"#,
    };
    let setup = Setup::branch(ORIGIN, &old, false, false);
    let env = &setup.env;
    env.run(&["run", "end", "--run", &setup.run]);
    env.run(&["comment", "--body", "UNRELATED PENDING THREAD"]);
    let run = start_review(env, &current, true);
    let finding = format!("{run}.c1p0.r1.f1");
    env.run(&["reply", &finding, "--body", "PRIVATE REPLY", "--local"]);
    env.run(&["reply", &finding, "--body", "UNRELATED PENDING REPLY"]);
    env.run(&[
        "comment",
        "--body",
        "CURRENT COMMENT",
        "--reviewer",
        &format!("{run}.c1p0.fix"),
        "--model",
        "m1",
        "--effort",
        "high",
        "--harness",
        "codex",
    ]);
    let service = github::Github::new();
    service.sync(&setup, &Remote::new());
    github::pulls(env, "me/repo", r#"[{"number":42,"state":"open"}]"#);
    service.accept(env, Call::PostNote, 601);
    auto_push(env);
    let local_target = env.run(&["status"])["target"].as_str().unwrap().to_owned();
    let output = env.exec(
        &["--target", &local_target, "run", "end", "--run", &run],
        None,
        &[],
    );
    let ended = review_json(&output);
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(ended["ended_at"].is_string());
    let inputs = env.forge_inputs();
    let sent = inputs
        .iter()
        .map(|(_, body)| body.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(sent.contains("CURRENT FINDING"), "{sent}");
    // An agent's comment with no diff line is no Conversation comment on your own PR.
    assert!(!sent.contains("CURRENT COMMENT"), "{sent}");
    assert!(sent.contains("id=summary attempt="), "{sent}");
    assert!(!sent.contains("/record"), "{sent}");
    for private in [
        "OLD PRIVATE FINDING",
        "UNRELATED PENDING THREAD",
        "PRIVATE REPLY",
        "UNRELATED PENDING REPLY",
        setup.run.as_str(),
    ] {
        assert!(!sent.contains(private), "leaked {private}: {sent}");
    }
    assert!(env.run(&["status"])["target"] == "gh-42");
}

#[test]
fn automatic_publish_sends_current_run_replies_on_imported_threads() {
    auto_publish_remote_reply(false);
}

#[test]
fn automatic_publish_sends_current_run_replies_on_published_findings() {
    auto_publish_remote_reply(true);
}

fn auto_publish_remote_reply(published_finding: bool) {
    use crate::support::forge::{Call, Comment, FakeService, Remote};

    let setup = if published_finding {
        Setup::reviewed(ORIGIN)
    } else {
        Setup::synced_branch(ORIGIN)
    };
    let env = &setup.env;
    let service = github::Github::new();
    let mut remote = Remote::new();
    remote.thread(
        "PRRT_old",
        false,
        3,
        vec![Comment::new(301, "alice", "REMOTE PARENT")],
    );
    service.sync(&setup, &remote);
    github::pulls(env, "me/repo", r#"[{"number":42,"state":"open"}]"#);
    let (thread, remote_thread) = if published_finding {
        service.land_findings(&setup);
        let path = env.state().join("gh-42.json");
        let mut target: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let parent_run = target["runs"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|run| run["id"] == setup.run)
            .unwrap();
        parent_run["ended_at"] = "2000-01-01T00:00:00Z".into();
        parent_run
            .as_object_mut()
            .unwrap()
            .remove("publication_target");
        fs::write(path, serde_json::to_vec_pretty(&target).unwrap()).unwrap();
        (setup.finding(1), "PRRT_f1")
    } else {
        env.run(&["sync"]);
        ("gh-t301".to_owned(), "PRRT_old")
    };
    let current = env.run(&["pass", "start", "--kind", "full"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    for (body, run, private) in [
        ("OLD RUN REPLY", Some(setup.run.as_str()), false),
        ("UNATTRIBUTED REPLY", None, false),
        ("PRIVATE CURRENT REPLY", Some(current.as_str()), true),
        ("CURRENT REMOTE REPLY", Some(current.as_str()), false),
    ] {
        let mut args = vec!["reply", &thread, "--body", body];
        let who;
        if let Some(run) = run {
            who = format!("{run}.c1p0.fix");
            args.extend([
                "--reviewer",
                &who,
                "--model",
                "m1",
                "--effort",
                "xhigh",
                "--harness",
                "codex",
            ]);
        }
        if private {
            args.push("--local");
        }
        env.run(&args);
    }
    service.accept(env, Call::Reply(remote_thread), 801);
    service.accept(env, Call::PostNote, 601);
    auto_push(env);
    env.run(&["pass", "end", "--run", &current, "--pass", "c1p0"]);
    let output = env.exec(&["run", "end", "--run", &current], None, &[]);
    review_json(&output);
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let sent = env
        .forge_inputs()
        .iter()
        .map(|(_, body)| body.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(sent.contains("CURRENT REMOTE REPLY"), "{sent}");
    for private in [
        "OLD RUN REPLY",
        "UNATTRIBUTED REPLY",
        "PRIVATE CURRENT REPLY",
    ] {
        assert!(!sent.contains(private), "leaked {private}: {sent}");
        let list = env.run(&["list"]);
        let conversation = crate::support::scenario::thread(&list, &thread);
        let comment = conversation["comments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|comment| comment["body"] == private)
            .unwrap();
        assert!(comment["remote"].is_null(), "{comment}");
        assert_ne!(comment["post"], "posted", "{comment}");
    }
}

#[test]
fn automatic_publish_failure_preserves_the_saved_run_and_pending_delivery() {
    use crate::support::forge::{Call, FakeService, Remote};
    let setup = Setup::reviewed(ORIGIN);
    let env = &setup.env;
    let old = crate::support::scenario::start_review(env, &crate::support::scenario::SYNC, false);
    env.run(&["run", "end", "--run", &old]);
    let service = github::Github::new();
    service.sync(&setup, &Remote::new());
    github::pulls(env, "me/repo", r#"[{"number":42,"state":"open"}]"#);
    service.lose(env, Call::PostInline);
    auto_push(env);
    let output = env.exec(&["run", "end", "--run", &setup.run], None, &[]);
    let ended = review_json(&output);
    assert!(ended["ended_at"].is_string());
    assert!(String::from_utf8_lossy(&output.stderr).contains("skipped"));
    let list = env.run(&["list"]);
    let finding = crate::support::scenario::thread(&list, &setup.finding(1));
    assert_eq!(finding["comments"][0]["post"], "posting");
    assert!(finding["comments"][0]["attempt"].is_string());
    assert!(env.run(&["status", "--full"])["runs"][0]["ended_at"].is_string());
    let path = env.state().join("gh-42.json");
    let mut target: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        target["runs"][0]["publication_target"],
        "https://github.com/me/repo/pull/42"
    );
    target["review"]["attached_at"] = Value::String("2099-01-01T00:00:00Z".into());
    fs::write(&path, serde_json::to_vec_pretty(&target).unwrap()).unwrap();
    let retry = env.run(&["publish", "--dry-run"]);
    let writes = retry["writes"].to_string();
    assert!(writes.contains(&setup.finding(1)), "{retry}");
    assert!(!writes.contains(&old), "{retry}");
    assert!(
        retry["skipped"].to_string().contains("ended before"),
        "{retry}"
    );
    target["runs"][0]["publication_target"] =
        Value::String("https://github.com/me/repo/pull/43".into());
    fs::write(&path, serde_json::to_vec_pretty(&target).unwrap()).unwrap();
    let different = env.run(&["publish", "--dry-run"]);
    assert!(
        !different["writes"].to_string().contains(&setup.finding(1)),
        "{different}"
    );
    assert!(
        different["skipped"].to_string().contains("ended before"),
        "{different}"
    );
    target["runs"][0]["publication_target"] =
        Value::String("https://github.com/me/repo/pull/42".into());
    fs::write(&path, serde_json::to_vec_pretty(&target).unwrap()).unwrap();
    service.restore(env, Call::PostInline);
    service.accept(env, Call::PostInline, 500);
    env.respond(
        &[
            "api",
            "--method",
            "GET",
            "repos/me/repo/pulls/42/reviews/500/comments",
        ],
        &serde_json::json!([{"id": 501, "body": finding["comments"][0]["sent"]}]).to_string(),
    );
    let retried = env.run(&["publish", "--only", &setup.finding(1)]);
    assert_eq!(retried["posted"][0]["kind"], "review", "{retried}");
    assert_eq!(
        retried["posted"][0]["items"][0],
        format!("{}/c1", setup.finding(1))
    );
}

#[test]
fn offline_run_end_saves_without_automatic_publication() {
    let setup = Setup::reviewed(ORIGIN);
    auto_push(&setup.env);
    let ended = review_json(&setup.env.exec(
        &["--offline", "run", "end", "--run", &setup.run],
        None,
        &[],
    ));
    assert!(ended["ended_at"].is_string());
    assert!(setup.env.forge_calls().is_empty());
}

#[test]
fn auto_push_without_forge_post_publishes_nothing() {
    let setup = Setup::reviewed(ORIGIN);
    let env = &setup.env;
    github::login(env);
    github::pulls(env, "me/repo", r#"[{"number":42,"state":"open"}]"#);
    env.write_config("[sync]\nauto_push = true\n[forge]\npost = false\n");
    let ended = review_json(&env.exec(&["run", "end", "--run", &setup.run], None, &[]));
    assert!(ended["ended_at"].is_string());
    assert!(env.forge_calls().is_empty(), "{:?}", env.forge_calls());
}

#[test]
fn automatic_publication_skips_a_held_publish_lock() {
    let setup = Setup::reviewed(ORIGIN);
    let env = &setup.env;
    github::login(env);
    github::pulls(env, "me/repo", r#"[{"number":42,"state":"open"}]"#);
    auto_push(env);
    let lock = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(env.state().join(".gh-42.publish.lock"))
        .unwrap();
    lock.lock().unwrap();
    let (release, waiting) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _ = waiting.recv_timeout(Duration::from_secs(3));
        drop(lock);
    });
    let output = env.exec(&["run", "end", "--run", &setup.run], None, &[]);
    let _ = release.send(());
    holder.join().unwrap();
    let ended = review_json(&output);
    assert!(ended["ended_at"].is_string());
    assert!(String::from_utf8_lossy(&output.stderr).contains("publish is busy"));
    assert_eq!(env.forge_calls().len(), 2);
    assert!(env.run(&["status", "--full"])["runs"][0]["ended_at"].is_string());
}

#[test]
fn automatic_publication_failure_obeys_one_deadline() {
    let setup = Setup::reviewed(ORIGIN);
    auto_push(&setup.env);
    let (tool, pid) = slow_tool(&setup.env);
    let started = Instant::now();
    let output = setup.env.exec(
        &["run", "end", "--run", &setup.run],
        None,
        &[("AGT_GH_BIN", &tool), ("AGT_SYNC_CAP_MS", "100")],
    );
    let ended = review_json(&output);
    assert!(ended["ended_at"].is_string());
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr)
            .matches("sync skipped")
            .count(),
        1
    );
    assert_gone(&pid);
}
