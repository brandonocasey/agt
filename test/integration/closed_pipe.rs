//! A reader that closes stdout early, such as `head`, ends a command quietly. The exit code is 0.

use std::fs;
use std::io::Read;
use std::process::Stdio;

use serde_json::json;

use crate::support::Env;

/// One worktree backup whose record lists many skipped paths. They fill any pipe many times over.
fn large_backup(env: &Env) {
    let dir = env.path("state/agents/backups/repo/big");
    fs::create_dir_all(&dir).unwrap();
    let skipped: Vec<String> = (0..100_000)
        .map(|n| format!("ignored/file-{n:010}"))
        .collect();
    let record = json!({
        "id": "big",
        "repository": env.repo,
        "worktree": env.path("trees/big"),
        "branch": null,
        "head": "0".repeat(40),
        "snapshot": "0".repeat(40),
        "ref": "refs/agt/backups/big",
        "created": "2026-01-01T00:00:00Z",
        "copied": [],
        "skipped": skipped,
    });
    fs::write(dir.join("backup.json"), record.to_string()).unwrap();
}

#[test]
fn stdout_closed_early_exits_0_and_prints_nothing_on_stderr() {
    let env = Env::new();
    large_backup(&env);
    let mut child = env
        .bin(&env.repo)
        .args(["worktree", "backups"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("agt runs");
    let mut stdout = child.stdout.take().unwrap();
    let mut head = [0_u8; 100];
    stdout.read_exact(&mut head).unwrap();
    assert!(head.starts_with(b"{"), "{head:?}");
    drop(stdout);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

#[test]
fn the_full_output_still_arrives_when_nobody_closes_the_pipe() {
    let env = Env::new();
    large_backup(&env);
    let output = env
        .bin(&env.repo)
        .args(["worktree", "backups"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.len() > 1_000_000);
    assert_eq!(output.stderr, b"");
}
