//! Auto-sync: a read pulls from the forge first when the local copy is stale, and falls back to
//! the local data, with one line on stderr, when the forge cannot be reached. The issue
//! scenarios run against GitHub and GitLab, the review ones against GitHub.

use std::fs;
use std::process::Output;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::issue_read::synced;
use crate::support::Env;
use crate::support::issue::{Github, Gitlab, IssueFake, NOW, Spec};
use crate::support::process::{assert_gone, assert_signal_kills_the_tool, slow_tool};

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
    a_failed_sync_pauses_the_next_reads_for_the_max_age,
    an_explicit_sync_clears_a_failure,
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
    let host = if fake.name() == "github" {
        "github.com"
    } else {
        "gitlab.com"
    };
    env.git(&[
        "remote",
        "add",
        "origin",
        &format!("https://{host}/{}.git", fake.repo()),
    ]);
    env.write_config(&format!(
        "{}\n[sync]\nauto_pull = true\n",
        fake.config(false)
    ));
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
    env.write_config(&format!(
        "{}\n[sync]\nauto_pull = false\n",
        fake.config(false)
    ));
    let config = read(&env, STALE, &["show", "1"], &[]);
    assert_quiet_success(&config);
    assert_eq!(title(&config), "Login fails");
    assert_eq!(env.forge_calls().len(), before);
    // `AGT_OFFLINE=0` is not offline.
    env.write_config(&format!(
        "{}\n[sync]\nauto_pull = true\n",
        fake.config(false)
    ));
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

#[test]
fn a_signal_to_agt_kills_the_forge_tool_and_its_children() {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let env = changed_on_the_host(&Github);
        let (tool, pid) = slow_tool(&env);
        let agt = env
            .bin(&env.repo)
            .args(["issue", "list"])
            .env("AGT_NOW", STALE.to_string())
            .env("AGT_GH_BIN", &tool)
            .env("AGT_GLAB_BIN", &tool)
            .env("AGT_SYNC_CAP_MS", "60000")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        assert_signal_kills_the_tool(agt, signal, &pid);
    }
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
    env.write_config(&format!(
        "{}\n{}\n[sync]\nauto_pull = true\n",
        fake.config(false),
        other_config
    ));
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
fn a_bad_config_does_not_break_a_local_read() {
    let env = Env::new();
    for (text, key) in [
        ("[sync]\nbogus = 1\n", "bogus"),
        ("[sync]\nauto = true\n", "auto"),
        ("[sync]\nauto_pull = \"yes\"\n", ""),
        ("[sync]\nmax_age_secs = \"soon\"\n", ""),
    ] {
        env.write_config(text);
        let output = read(&env, STALE, &["list"], &[]);
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(stdout(&output)["count"], 0);
        let reason = skipped_reason(&output);
        assert!(
            reason.contains("config.toml") && reason.contains(key),
            "{reason}"
        );
        // Explicit sync keeps failing on it.
        let error = env.issue_fail(&["sync"]);
        assert!(error.contains("config.toml"), "{error}");
    }
}

fn failure_file(env: &Env, fake: Fake) -> String {
    fs::read_to_string(
        env.issue_state()
            .join(fake.tracker())
            .join(".auto-sync.json"),
    )
    .unwrap_or_default()
}

fn a_failed_sync_pauses_the_next_reads_for_the_max_age(fake: Fake) {
    let env = changed_on_the_host(fake);
    let list = fake.list_call("2026-10-10T09%3A55%3A00Z");
    let list: Vec<&str> = list.iter().map(String::as_str).collect();
    env.fail_call_with(&list, "HTTP 503: unavailable");
    let before = env.forge_calls().len();
    let first = read(&env, STALE, &["list"], &[]);
    assert!(skipped_reason(&first).contains("503"));
    let tried = env.forge_calls().len();
    assert!(tried > before);
    assert!(failure_file(&env, fake).contains("failures"));
    // Within the window: no call, no line, the local data.
    let paused = read(&env, STALE + 299, &["list"], &[]);
    assert_quiet_success(&paused);
    assert_eq!(paused.stdout, first.stdout);
    assert_eq!(env.forge_calls().len(), tried);
    // After it the host is asked again, and a success forgets the failure.
    env.pass_call(&list);
    let later = read(&env, STALE + 301, &["show", "1"], &[]);
    assert_quiet_success(&later);
    assert_eq!(title(&later), "Login fails, renamed");
    assert!(env.forge_calls().len() > tried);
    assert!(
        !failure_file(&env, fake).contains("tracker"),
        "{}",
        failure_file(&env, fake)
    );
}

fn an_explicit_sync_clears_a_failure(fake: Fake) {
    let env = changed_on_the_host(fake);
    let list = fake.list_call("2026-10-10T09%3A55%3A00Z");
    let list: Vec<&str> = list.iter().map(String::as_str).collect();
    env.fail_call(&list);
    skipped_reason(&read(&env, STALE, &["list"], &[]));
    assert!(failure_file(&env, fake).contains("sync"));
    env.pass_call(&list);
    let synced = read(&env, STALE + 1, &["sync"], &[]);
    assert!(synced.status.success());
    assert!(!failure_file(&env, fake).contains("\"sync\""));
}

#[test]
fn max_age_secs_sets_how_long_a_sync_stays_fresh() {
    let fake: Fake = &Github;
    let env = changed_on_the_host(fake);
    env.write_config(&format!(
        "{}\n[sync]\nauto_pull = true\nmax_age_secs = 3600\n",
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
