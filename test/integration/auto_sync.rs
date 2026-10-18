//! Auto-sync: a read pulls from the forge first when the local copy is stale, and falls back to
//! the local data, with one line on stderr, when the forge cannot be reached. The issue
//! scenarios run against GitHub and GitLab, the review ones against GitHub.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::issue_read::synced;
use crate::support::Env;
use crate::support::issue::{Github, Gitlab, IssueFake, NOW, Spec};

type Fake<'a> = &'a dyn IssueFake;

/// The default max age is 300 seconds.
const STALE: i64 = NOW + 301;
const FRESH: i64 = NOW + 299;
pub(crate) const SKIPPED: &str = "agt: sync skipped (";
pub(crate) const LOCAL: &str = "); showing local data\n";

macro_rules! matrix {
    ($($scenario:ident),* $(,)?) => {
        $(
            mod $scenario {
                #[test]
                fn github() {
                    super::$scenario(&crate::support::issue::Github);
                }

                #[test]
                fn gitlab() {
                    super::$scenario(&crate::support::issue::Gitlab);
                }
            }
        )*
    };
}

matrix!(
    a_stale_issue_read_syncs_first_and_shows_the_new_data,
    a_fresh_issue_read_makes_no_call,
    every_issue_read_syncs_when_stale,
    offline_keeps_issue_reads_local,
    a_failing_forge_leaves_the_local_read_and_one_line,
    a_missing_tool_leaves_the_local_read_and_one_line,
    a_slow_forge_is_killed_at_the_cap,
    a_held_lock_skips_the_issue_sync,
    a_tracker_filter_syncs_only_that_tracker,
);

fn read(env: &Env, now: i64, args: &[&str], more: &[(&str, &str)]) -> Output {
    let mut vars = vec![("AGT_NOW", now.to_string())];
    vars.extend(more.iter().map(|(key, value)| (*key, (*value).to_owned())));
    let vars: Vec<(&str, &str)> = vars
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    env.issue_exec(args, "", &vars)
}

fn stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_quiet_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

/// One line on stderr that says the sync was skipped, and why.
pub(crate) fn skipped_reason(output: &Output) -> String {
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    let reason = stderr
        .strip_prefix(SKIPPED)
        .and_then(|rest| rest.strip_suffix(LOCAL))
        .unwrap_or_else(|| panic!("not the skip line: {stderr}"));
    reason.to_owned()
}

/// The tracker as `synced` leaves it, then the host renames issue 1 after the watermark.
fn changed_on_the_host(fake: Fake) -> Env {
    let env = synced(fake, &[Spec::new(1, "Login fails")]);
    let renamed = Spec::new(1, "Login fails, renamed").updated("2026-10-12T10:00:00Z");
    // The watermark is 2026-10-10T10:00:00Z; a listing starts five minutes before it.
    fake.serve_since(&env, "2026-10-10T09%3A55%3A00Z", &[renamed]);
    env
}

fn title(output: &Output) -> String {
    stdout(output)["title"].as_str().unwrap().to_owned()
}

fn a_stale_issue_read_syncs_first_and_shows_the_new_data(fake: Fake) {
    let env = changed_on_the_host(fake);
    let before = env.forge_calls().len();
    let shown = read(&env, STALE, &["show", "1"], &[]);
    assert_quiet_success(&shown);
    assert_eq!(title(&shown), "Login fails, renamed");
    assert!(env.forge_calls().len() > before);
    assert!(env.forge_writes(fake).is_empty());
    let tracker = env.issue_doc(fake.tracker(), "tracker");
    assert_eq!(tracker["synced_at"], "2026-10-17T12:05:01Z");
}

fn a_fresh_issue_read_makes_no_call(fake: Fake) {
    let env = changed_on_the_host(fake);
    let before = env.forge_calls().len();
    let shown = read(&env, FRESH, &["show", "1"], &[]);
    assert_quiet_success(&shown);
    assert_eq!(title(&shown), "Login fails");
    assert_eq!(env.forge_calls().len(), before);
}

fn every_issue_read_syncs_when_stale(fake: Fake) {
    for args in [
        vec!["list"],
        vec!["show", "1"],
        vec!["search", "login"],
        vec!["status"],
        vec!["find", "--branch", "feat"],
    ] {
        let env = changed_on_the_host(fake);
        let before = env.forge_calls().len();
        assert_quiet_success(&read(&env, STALE, &args, &[]));
        assert!(env.forge_calls().len() > before, "{args:?} did not sync");
    }
}

fn offline_keeps_issue_reads_local(fake: Fake) {
    let env = changed_on_the_host(fake);
    let before = env.forge_calls().len();
    let flag = read(&env, STALE, &["--offline", "show", "1"], &[]);
    assert_quiet_success(&flag);
    assert_eq!(title(&flag), "Login fails");
    let variable = read(&env, STALE, &["show", "1"], &[("AGT_OFFLINE", "1")]);
    assert_quiet_success(&variable);
    assert_eq!(title(&variable), "Login fails");
    env.write_config(&format!("{}\n[sync]\nauto = false\n", fake.config(false)));
    let config = read(&env, STALE, &["show", "1"], &[]);
    assert_quiet_success(&config);
    assert_eq!(title(&config), "Login fails");
    assert_eq!(env.forge_calls().len(), before);
    // `AGT_OFFLINE=0` is not offline.
    env.write_config(&fake.config(false));
    let online = read(&env, STALE, &["show", "1"], &[("AGT_OFFLINE", "0")]);
    assert_eq!(title(&online), "Login fails, renamed");
}

fn a_failing_forge_leaves_the_local_read_and_one_line(fake: Fake) {
    let env = changed_on_the_host(fake);
    let local = read(&env, STALE, &["list"], &[("AGT_OFFLINE", "1")]);
    env.fail_call_with(
        &fake
            .list_call("2026-10-10T09%3A55%3A00Z")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        "HTTP 403: API rate limit exceeded",
    );
    let output = read(&env, STALE, &["list"], &[]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, local.stdout);
    assert!(skipped_reason(&output).contains("rate limit"));
}

fn a_missing_tool_leaves_the_local_read_and_one_line(fake: Fake) {
    let env = changed_on_the_host(fake);
    let local = read(&env, STALE, &["list"], &[("AGT_OFFLINE", "1")]);
    let output = read(
        &env,
        STALE,
        &["list"],
        &[
            ("AGT_GH_BIN", "/nonexistent/gh"),
            ("AGT_GLAB_BIN", "/nonexistent/glab"),
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, local.stdout);
    assert!(skipped_reason(&output).contains("run "));
}

/// A tool that starts a child, records its pid and then sleeps for a minute.
pub(crate) fn slow_tool(env: &Env) -> (String, std::path::PathBuf) {
    let tool = env.path("slow-forge.sh");
    let pid = env.path("slow.pid");
    fs::write(
        &tool,
        format!(
            "#!/bin/sh\nsleep 60 &\necho $! > '{}'\nwait\n",
            pid.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    (tool.display().to_string(), pid)
}

pub(crate) fn assert_gone(pid: &Path) {
    let pid: i32 = fs::read_to_string(pid).unwrap().trim().parse().unwrap();
    for _ in 0..100 {
        // SAFETY: signal 0 only asks whether the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("process {pid} is still running");
}

fn a_slow_forge_is_killed_at_the_cap(fake: Fake) {
    let env = changed_on_the_host(fake);
    let local = read(&env, STALE, &["list"], &[("AGT_OFFLINE", "1")]);
    let (tool, pid) = slow_tool(&env);
    let started = Instant::now();
    let output = read(
        &env,
        STALE,
        &["list"],
        &[
            ("AGT_GH_BIN", &tool),
            ("AGT_GLAB_BIN", &tool),
            ("AGT_SYNC_CAP_MS", "400"),
        ],
    );
    assert!(started.elapsed() < Duration::from_secs(20));
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, local.stdout);
    assert!(skipped_reason(&output).contains("timed out"));
    assert_gone(&pid);
}

fn a_held_lock_skips_the_issue_sync(fake: Fake) {
    let env = changed_on_the_host(fake);
    let before = env.forge_calls().len();
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(
            env.issue_state()
                .join(fake.tracker())
                .join(".sync.publish.lock"),
        )
        .unwrap();
    lock.lock().unwrap();
    let output = read(&env, STALE, &["show", "1"], &[]);
    assert_eq!(title(&output), "Login fails");
    assert!(skipped_reason(&output).contains("another agt is syncing"));
    assert_eq!(env.forge_calls().len(), before);
    drop(lock);
    let output = read(&env, STALE, &["show", "1"], &[]);
    assert_quiet_success(&output);
    assert_eq!(title(&output), "Login fails, renamed");
}

fn a_tracker_filter_syncs_only_that_tracker(fake: Fake) {
    let other_config = match fake.name() {
        "github" => Gitlab.config(false),
        _ => Github.config(false),
    };
    let env = changed_on_the_host(fake);
    env.write_config(&format!("{}\n{}", fake.config(false), other_config));
    // The other tracker has no answers: syncing it would fail and print the skip line.
    let before = env.forge_calls().len();
    let output = read(&env, STALE, &["--tracker", fake.repo(), "show", "1"], &[]);
    assert_quiet_success(&output);
    assert_eq!(title(&output), "Login fails, renamed");
    assert!(env.forge_calls().len() > before);
}

#[test]
fn no_configured_tracker_means_no_sync_and_no_output() {
    let env = Env::new();
    let output = read(&env, STALE, &["list"], &[]);
    assert_quiet_success(&output);
    assert_eq!(stdout(&output)["count"], 0);
    assert!(env.forge_calls().is_empty());
}

#[test]
fn a_bad_sync_config_fails_like_the_rest_of_the_config() {
    let env = Env::new();
    for text in [
        "[sync]\nbogus = 1\n",
        "[sync]\nauto = \"yes\"\n",
        "[sync]\nmax_age_secs = \"soon\"\n",
    ] {
        env.write_config(text);
        let error = env.issue_fail(&["list"]);
        assert!(error.contains("config.toml"), "{error}");
    }
}

#[test]
fn max_age_secs_sets_how_long_a_sync_stays_fresh() {
    let fake: Fake = &Github;
    let env = changed_on_the_host(fake);
    env.write_config(&format!(
        "{}\n[sync]\nmax_age_secs = 3600\n",
        fake.config(false)
    ));
    let before = env.forge_calls().len();
    assert_eq!(
        title(&read(&env, NOW + 3000, &["show", "1"], &[])),
        "Login fails"
    );
    assert_eq!(env.forge_calls().len(), before);
    assert_eq!(
        title(&read(&env, NOW + 3601, &["show", "1"], &[])),
        "Login fails, renamed"
    );
}

#[test]
fn explicit_sync_has_no_cap_and_keeps_its_errors() {
    let fake: Fake = &Github;
    let env = changed_on_the_host(fake);
    let output = read(&env, STALE, &["sync"], &[("AGT_GH_BIN", "/nonexistent/gh")]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("agt: error: "), "{stderr}");
    assert!(!stderr.contains("sync skipped"), "{stderr}");
}
