mod commands;
mod reviewed;
mod support;

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
