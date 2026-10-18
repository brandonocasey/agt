//! `agt task` records and the session index.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{Value, json};

use crate::support::Env;

const BIN: &str = env!("CARGO_BIN_EXE_agt");

pub(crate) struct Task {
    env: Env,
    /// The `agt` command the helpers run: `task` or `sessions`.
    domain: &'static str,
}

impl Task {
    pub(crate) fn new() -> Self {
        Self {
            env: Env::new(),
            domain: "task",
        }
    }

    pub(crate) fn sessions() -> Self {
        Self {
            env: Env::new(),
            domain: "sessions",
        }
    }

    pub(crate) fn root(&self) -> PathBuf {
        self.env.path("state")
    }

    pub(crate) fn home(&self) -> PathBuf {
        self.env.path("home")
    }

    pub(crate) fn tasks(&self) -> PathBuf {
        self.root().join("agents").join("tasks")
    }

    pub(crate) fn command(&self, args: &[&str]) -> std::process::Command {
        let mut command = self.env.command(BIN, &self.env.repo);
        command.arg(self.domain).args(args);
        command
    }

    pub(crate) fn raw(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    pub(crate) fn run(&self, args: &[&str]) -> Value {
        let output = self.raw(args);
        assert!(
            output.status.success(),
            "agt task {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    pub(crate) fn fail(&self, args: &[&str]) -> String {
        let output = self.raw(args);
        assert!(!output.status.success(), "agt task {args:?} succeeded");
        String::from_utf8(output.stderr).unwrap()
    }

    pub(crate) fn write(&self, path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    pub(crate) fn file(&self, name: &str) -> PathBuf {
        self.root().join(name)
    }
}

pub(crate) fn lines(entries: &[Value]) -> String {
    let mut text: String = entries.iter().map(|entry| format!("{entry}\n")).collect();
    if text.is_empty() {
        text.push('\n');
    }
    text
}

pub(crate) fn user(session: &str, text: &str) -> Value {
    json!({"type": "user", "sessionId": session, "message": {"role": "user", "content": text}})
}

pub(crate) fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

pub(crate) fn texts(results: &Value) -> Vec<&str> {
    results
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["text"].as_str().unwrap())
        .collect()
}

pub(crate) fn sources(result: &Value) -> Vec<&str> {
    let mut sources: Vec<&str> = result["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|source| source.as_str().unwrap())
        .collect();
    sources.sort_unstable();
    sources
}

#[test]
fn creates_checks_updates_and_lists_a_private_task() {
    let task = Task::new();
    let created = task.run(&[
        "create",
        "--id",
        "demo",
        "--objective",
        "Finish work",
        "--authorization",
        "user request",
        "--scope",
        "local edits",
        "--parent-pr",
        "42",
        "--target-branch",
        "main",
        "--task-boundary",
        "session lookup",
    ]);
    assert_eq!(created["revision"], 1);
    assert_eq!(created["parentPr"], "42");
    assert_eq!(created["targetBranch"], "main");
    assert_eq!(created["taskBoundary"], "session lookup");
    assert_eq!(mode(&task.tasks()), 0o700);
    assert_eq!(mode(&task.tasks().join("demo.json")), 0o600);
    let checked = task.run(&[
        "check",
        "demo",
        "--revision",
        "1",
        "--name",
        "unit",
        "--result",
        "pass",
        "--inputs",
        "fixture",
    ]);
    assert_eq!(checked["revision"], 2);
    assert_eq!(checked["checks"][0]["inputs"], "fixture");
    let updated = task.run(&[
        "update",
        "demo",
        "--revision",
        "2",
        "--correction",
        "Use B",
        "--next-step",
        "Ship",
        "--status",
        "paused",
        "--parent-pr",
        "43",
        "--target-branch",
        "release",
        "--task-boundary",
        "follow-up",
    ]);
    assert_eq!(updated["revision"], 3);
    assert_eq!(updated["corrections"][0]["text"], "Use B");
    assert_eq!(updated["parentPr"], "43");
    assert_eq!(updated["targetBranch"], "release");
    assert_eq!(updated["taskBoundary"], "follow-up");
    assert_eq!(updated["status"], "paused");
    assert_eq!(task.run(&["list"])[0]["id"], "demo");
}

#[test]
fn rejects_traversal_and_stale_revisions() {
    let task = Task::new();
    task.run(&["create", "--id", "demo", "--objective", "Finish work"]);
    let stale = task.fail(&["update", "demo", "--revision", "2", "--next-step", "No"]);
    assert!(stale.contains("stale revision"), "{stale}");
    assert_eq!(
        task.run(&["update", "demo", "--revision", "1", "--next-step", "Yes"])["revision"],
        2
    );
    let duplicate = task.fail(&["create", "--id", "demo", "--objective", "Again"]);
    assert!(duplicate.contains("already exists"), "{duplicate}");
    let after = task.run(&[
        "update",
        "demo",
        "--revision",
        "2",
        "--next-step",
        "Still works",
    ]);
    assert_eq!(after["revision"], 3);
    let traversal = task.fail(&["show", "../demo"]);
    assert!(traversal.contains("id must match"), "{traversal}");
}

#[test]
fn help_lists_the_task_commands_and_a_bare_run_is_a_usage_error() {
    let task = Task::new();
    for flag in ["--help", "help"] {
        let output = task.raw(&[flag]);
        assert!(output.status.success());
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(help.contains("create") && help.contains("check"), "{help}");
        assert!(
            !help.contains("resolve") && !help.contains("index"),
            "{help}"
        );
    }
    assert_eq!(task.raw(&[]).status.code(), Some(2));
}

#[test]
fn refuses_symbolic_link_task_records() {
    let task = Task::new();
    fs::create_dir_all(task.tasks()).unwrap();
    let target = task.file("target.json");
    fs::write(&target, r#"{"id":"linked","revision":1}"#).unwrap();
    symlink(&target, task.tasks().join("linked.json")).unwrap();
    let error = task.fail(&["show", "linked"]);
    assert!(error.contains("must not be a symbolic link"), "{error}");
}

#[test]
fn rejects_oversized_persisted_fields_without_changing_or_locking_the_task() {
    let task = Task::new();
    let oversized = "x".repeat(16_385);
    let error = task.fail(&[
        "create",
        "--id",
        "large-create",
        "--objective",
        "Work",
        "--next-step",
        &oversized,
    ]);
    assert!(error.contains("next-step exceeds"), "{error}");
    assert!(!task.tasks().join("large-create.json").exists());
    assert!(!task.tasks().join(".large-create.lock").exists());
    task.run(&["create", "--id", "large-check", "--objective", "Work"]);
    for field in ["code-revision", "inputs", "environment"] {
        let flag = format!("--{field}");
        let error = task.fail(&[
            "check",
            "large-check",
            "--revision",
            "1",
            "--name",
            "check",
            "--result",
            "pass",
            &flag,
            &oversized,
        ]);
        assert!(error.contains(&format!("{field} exceeds")), "{error}");
        let unchanged = task.run(&["show", "large-check"]);
        assert_eq!(unchanged["revision"], 1);
        assert_eq!(unchanged["checks"], json!([]));
        assert!(!task.tasks().join(".large-check.lock").exists());
    }
}

#[test]
fn a_held_task_lock_fails_fast_and_is_not_left_behind() {
    let task = Task::new();
    task.run(&["create", "--id", "busy", "--objective", "Work"]);
    let lock = task.tasks().join(".busy.lock");
    fs::create_dir(&lock).unwrap();
    let error = task.fail(&["update", "busy", "--revision", "1", "--next-step", "x"]);
    assert!(error.contains("is being updated; retry"), "{error}");
    assert!(lock.exists(), "a lock the tool did not take stays");
    fs::remove_dir(&lock).unwrap();
    assert_eq!(
        task.run(&["update", "busy", "--revision", "1", "--next-step", "x"])["revision"],
        2
    );
    assert!(!lock.exists());
}

#[test]
fn input_errors_fail_with_a_reason() {
    let task = Task::new();
    // Usage errors are clap's and exit with 2; the rest exit with 1.
    let usage = |args: &[&str], reason: &str| {
        let output = task.raw(args);
        let error = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {error}");
        assert!(error.contains(reason), "{args:?}: {error}");
    };
    usage(&["frobnicate"], "unrecognized subcommand 'frobnicate'");
    usage(&["create", "--id"], "a value is required for '--id <ID>'");
    usage(
        &["create", "--id", "a", "--objective"],
        "a value is required for '--objective <OBJECTIVE>'",
    );
    usage(&["create", "--id", "a"], "--objective <OBJECTIVE>");
    usage(&["update", "a", "--revision", "0"], "--revision <REVISION>");
    usage(
        &["update", "a", "--revision", "1", "--status", "weird"],
        "invalid value 'weird' for '--status <STATUS>'",
    );
    assert!(
        task.fail(&["create", "--id", "Bad", "--objective", "x"])
            .contains("id must match")
    );
    assert!(
        task.fail(&["create", "--id", "a", "--objective", ""])
            .contains("objective must not be empty")
    );
    assert!(
        task.fail(&["show", "missing"])
            .contains("task missing does not exist")
    );
    task.run(&["create", "a", "--objective", "x"]);
    assert!(
        task.fail(&["update", "a", "--revision", "1", "--scope", "s"])
            .contains("authorization must not be empty")
    );
    assert!(
        task.fail(&["update", "a", "--revision", "1", "--next-step", ""])
            .contains("next-step must not be empty")
    );
    assert!(
        task.fail(&[
            "check",
            "a",
            "--revision",
            "1",
            "--name",
            "unit",
            "--result",
            ""
        ])
        .contains("result must not be empty")
    );
    // The index commands live under `agt sessions`.
    usage(&["search"], "unrecognized subcommand 'search'");
}
