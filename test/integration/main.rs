mod commands;
mod matrix;
mod publish;
mod reviewed;
mod support;
mod sync;

use assert_cmd::Command;

#[test]
fn prints_version() {
    Command::cargo_bin("agent-review")
        .expect("binary builds")
        .arg("--version")
        .assert()
        .success()
        .stdout(concat!("agent-review ", env!("CARGO_PKG_VERSION"), "\n"));
}
