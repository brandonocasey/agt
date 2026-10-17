mod agt;
mod ci;
mod commands;
mod issue_queue;
mod issue_read;
mod matrix;
mod publish;
mod reviewed;
mod support;
mod sync;
mod task;
mod watch;

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
