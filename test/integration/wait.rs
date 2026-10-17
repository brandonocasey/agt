//! `agt wait`: exit 0 when the check passes, 3 when `--max` passes first, 2 on bad usage.

use std::time::{Duration, Instant};

use assert_cmd::Command;

fn wait(args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("agt").expect("binary builds");
    command.arg("wait").args(args);
    command
}

#[test]
fn a_passing_check_ends_the_wait() {
    wait(&["--", "true"])
        .assert()
        .code(0)
        .stdout("agt wait: done after 0s\n");
}

#[test]
fn the_check_runs_until_it_passes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let flag = dir.path().join("flag");
    let script = format!("[ -e {0} ] || {{ touch {0}; exit 1; }}", flag.display());
    let started = Instant::now();
    let output = wait(&["--every", "1", "--max", "10", "--", "sh", "-c", &script])
        .output()
        .expect("agt runs");
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("agt wait: done after "));
    assert!(started.elapsed() >= Duration::from_secs(1));
}

#[test]
fn a_failing_check_times_out_with_exit_3() {
    wait(&["--max", "1", "--every", "1", "--", "false"])
        .assert()
        .code(3)
        .stdout("agt wait: still running after 1s; check the job and re-arm\n");
    wait(&["--max", "1", "--every", "1", "--", "/nonexistent/check"])
        .assert()
        .code(3);
}

#[test]
fn bad_usage_exits_2() {
    wait(&["true"]).assert().code(2);
    wait(&["--max", "0", "--", "true"]).assert().code(2);
    wait(&["--every", "x", "--", "true"]).assert().code(2);
    wait(&[]).assert().code(2);
}
