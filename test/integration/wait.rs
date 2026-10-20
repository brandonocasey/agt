//! `agt wait`: exit 0 when the check passes, 3 when `--max` passes first, 2 on bad usage.

use std::time::{Duration, Instant};

use crate::support::Env;

/// Exit code and stdout of `agt wait` in the isolated environment.
fn wait(env: &Env, args: &[&str]) -> (Option<i32>, String) {
    let output = env
        .bin(&env.repo)
        .arg("wait")
        .args(args)
        .output()
        .expect("agt runs");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

#[test]
fn a_passing_check_ends_the_wait() {
    let env = Env::new();
    assert_eq!(
        wait(&env, &["--", "true"]),
        (Some(0), "agt wait: done after 0s\n".into())
    );
}

#[test]
fn the_check_runs_until_it_passes() {
    let env = Env::new();
    let flag = env.path("flag");
    let script = format!("[ -e {0} ] || {{ touch {0}; exit 1; }}", flag.display());
    let started = Instant::now();
    let (code, out) = wait(
        &env,
        &["--every", "1", "--max", "10", "--", "sh", "-c", &script],
    );
    assert_eq!(code, Some(0));
    assert!(out.starts_with("agt wait: done after "), "{out}");
    assert!(started.elapsed() >= Duration::from_secs(1));
}

#[test]
fn a_failing_check_times_out_with_exit_3() {
    let env = Env::new();
    assert_eq!(
        wait(&env, &["--max", "1", "--every", "1", "--", "false"]),
        (
            Some(3),
            "agt wait: still running after 1s; check the job and re-arm\n".into()
        )
    );
    let missing = ["--max", "1", "--every", "1", "--", "/nonexistent/check"];
    assert_eq!(wait(&env, &missing).0, Some(3));
}

#[test]
fn bad_usage_exits_2() {
    let env = Env::new();
    for args in [
        &["true"][..],
        &["--max", "0", "--", "true"],
        &["--every", "x", "--", "true"],
        &[],
    ] {
        assert_eq!(wait(&env, args).0, Some(2), "{args:?}");
    }
}

#[test]
fn a_sleep_interval_never_extends_the_deadline() {
    let env = Env::new();
    let started = Instant::now();
    let (code, _) = wait(&env, &["--max", "1", "--every", "4", "--", "false"]);
    assert_eq!(code, Some(3));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn an_overdue_check_cannot_report_late_success() {
    let env = Env::new();
    let started = Instant::now();
    let (code, _) = wait(&env, &["--max", "1", "--", "sh", "-c", "sleep 3; exit 0"]);
    assert_eq!(code, Some(3));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn an_overdue_check_loses_its_descendants() {
    let env = Env::new();
    let pid = env.path("child.pid");
    let script = format!("sleep 3 & echo $! > '{}'; wait", pid.display());
    let (code, _) = wait(&env, &["--max", "1", "--", "sh", "-c", &script]);
    assert_eq!(code, Some(3));
    crate::support::process::assert_gone(&pid);
}
