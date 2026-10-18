mod agt;
mod ci;
mod cleanup;
mod commands;
mod issue_push;
mod issue_queue;
mod issue_read;
mod matrix;
mod publish;
mod reviewed;
mod sessions;
mod support;
mod sync;
mod task;
mod wait;
mod watch;
mod worktree;

use assert_cmd::Command;

#[test]
fn prints_version() {
    Command::cargo_bin("agt")
        .expect("binary builds")
        .arg("--version")
        .assert()
        .success()
        .stdout(concat!("agt ", env!("CARGO_PKG_VERSION"), "\n"));
}
