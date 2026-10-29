mod agt;
mod auto_sync;
mod branch;
mod check;
mod ci;
mod ci_wait;
mod cleanup;
mod commands;
mod doctor;
mod issue_discard;
mod issue_hierarchy;
mod issue_push;
mod issue_queue;
mod issue_read;
mod matrix;
mod offline;
mod pr;
mod ps;
mod publish;
mod publish_gitlab_outside;
mod qa;
mod review_auto_sync;
mod review_issue;
mod review_report;
mod reviewed;
mod secrets;
mod session_index;
mod sessions;
mod support;
mod sync;
mod sync_all;
mod task;
mod trackers;
mod wait;
mod worktree;
mod worktree_create;
mod worktree_hooks;

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
