//! `agt task` records and the session index.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{Value, json};

use crate::support::Env;

const BIN: &str = env!("CARGO_BIN_EXE_agt");

struct Task {
    env: Env,
}

impl Task {
    fn new() -> Self {
        Self { env: Env::new() }
    }

    fn root(&self) -> PathBuf {
        self.env.path("state")
    }

    fn home(&self) -> PathBuf {
        self.env.path("home")
    }

    fn tasks(&self) -> PathBuf {
        self.root().join("agents").join("tasks")
    }

    fn command(&self, args: &[&str]) -> std::process::Command {
        let mut command = self.env.command(BIN, &self.env.repo);
        command.arg("task").args(args);
        command
    }

    fn raw(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Value {
        let output = self.raw(args);
        assert!(
            output.status.success(),
            "agt task {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn fail(&self, args: &[&str]) -> String {
        let output = self.raw(args);
        assert!(!output.status.success(), "agt task {args:?} succeeded");
        String::from_utf8(output.stderr).unwrap()
    }

    fn write(&self, path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn file(&self, name: &str) -> PathBuf {
        self.root().join(name)
    }
}

fn lines(entries: &[Value]) -> String {
    let mut text: String = entries.iter().map(|entry| format!("{entry}\n")).collect();
    if text.is_empty() {
        text.push('\n');
    }
    text
}

fn user(session: &str, text: &str) -> Value {
    json!({"type": "user", "sessionId": session, "message": {"role": "user", "content": text}})
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn texts(results: &Value) -> Vec<&str> {
    results
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["text"].as_str().unwrap())
        .collect()
}

fn sources(result: &Value) -> Vec<&str> {
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
fn indexes_metadata_ids_and_searches_human_turns_with_bounded_context() {
    let task = Task::new();
    let source = task.file("history.jsonl");
    let item = |role: &str, kind: &str, text: &str| {
        json!({"type": "response_item", "payload": {"type": "message", "role": role,
            "content": [{"type": kind, "text": text}]}})
    };
    task.write(
        &source,
        &lines(&[
            json!({"type": "session_meta", "payload": {"id": "codex-session"}}),
            item("developer", "input_text", "needle injected AGENTS"),
            json!({"type": "response_item", "payload": {"type": "function_call_output", "output": "needle tool output"}}),
            item("user", "input_text", "# AGENTS.md instructions\nneedle injected"),
            item("user", "input_text", "needle codex request"),
            item("assistant", "output_text", "codex reply"),
            json!({"type": "user", "isMeta": true, "sessionId": "ignored", "message": {"role": "user", "content": "needle injected skill"}}),
            json!({"type": "user", "sessionId": "claude-user", "message": {"role": "user", "content": [{"type": "text", "text": "needle claude request"}]}}),
            json!({"type": "assistant", "sessionId": "claude-user", "message": {"role": "assistant", "content": [{"type": "text", "text": "claude reply"}, {"type": "tool_use", "name": "ignored"}]}}),
        ]),
    );
    let file = source.to_str().unwrap();
    let results = task.run(&[
        "search", "--query", "needle", "--file", file, "--limit", "3",
    ]);
    assert_eq!(results.as_array().unwrap().len(), 2);
    assert_eq!(
        texts(&results),
        ["needle claude request", "needle codex request"]
    );
    assert_eq!(results[1]["sessionId"], "codex-session");
    let roles: Vec<&str> = results[1]["context"]
        .as_array()
        .unwrap()
        .iter()
        .map(|turn| turn["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant"]);
    let resolved = task.run(&["resolve", "--session", "codex-session", "--file", file]);
    assert_eq!(resolved["sources"][0], file);
    let excessive = task.fail(&[
        "search", "--query", "needle", "--file", file, "--limit", "101",
    ]);
    assert!(excessive.contains("101 is not in 1..=100"), "{excessive}");
}

#[test]
fn discovers_old_claude_history_archive_and_alternate_profiles() {
    let task = Task::new();
    let home = task.home();
    let files = [
        home.join(".claude/history.jsonl"),
        home.join(".claude/archives/old.jsonl"),
        home.join(".claude-work/projects/team/session.jsonl"),
    ];
    for (index, file) in files.iter().enumerate() {
        let entry = if index == 0 {
            json!({"display": format!("archive needle {index}"), "sessionId": format!("claude-{index}")})
        } else {
            user(
                &format!("claude-{index}"),
                &format!("archive needle {index}"),
            )
        };
        task.write(file, &lines(&[entry]));
    }
    let results = task.run(&["search", "--query", "archive needle"]);
    let mut sessions: Vec<&str> = results
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["sessionId"].as_str().unwrap())
        .collect();
    sessions.sort_unstable();
    assert_eq!(sessions, ["claude-0", "claude-1", "claude-2"]);
}

#[test]
fn filters_injected_and_inherited_duplicate_events_from_index_counts() {
    let task = Task::new();
    let (first, second) = (task.file("first.jsonl"), task.file("second.jsonl"));
    let duplicate = json!({"type": "user", "sessionId": "shared", "timestamp": 1_767_225_600,
        "message": {"role": "user", "content": "human needle"}});
    let mut transcript = duplicate.clone();
    transcript["timestamp"] = json!("2026-01-01T00:00:00.000Z");
    let meta = json!({"type": "user", "isMeta": true, "sessionId": "shared", "message": {"role": "user", "content": "meta needle"}});
    task.write(&first, &lines(&[duplicate, meta]));
    task.write(&second, &lines(&[transcript]));
    task.run(&["index", "--file", first.to_str().unwrap()]);
    let stats = task.run(&["index", "--file", second.to_str().unwrap()]);
    assert_eq!(stats["humanTurns"], 1);
    let results = task.run(&[
        "search",
        "--query",
        "needle",
        "--file",
        second.to_str().unwrap(),
    ]);
    assert_eq!(results.as_array().unwrap().len(), 1);
    assert_eq!(
        sources(&results[0]),
        [first.to_str().unwrap(), second.to_str().unwrap()]
    );
}

#[test]
fn incrementally_handles_append_truncation_malformed_lines_and_private_modes() {
    let task = Task::new();
    let source = task.file("changing.jsonl");
    let file = source.to_str().unwrap();
    task.write(
        &source,
        &format!("{}\n{{bad}}\n", user("one", "first needle")),
    );
    assert_eq!(task.run(&["index", "--file", file])["malformedLines"], 1);
    let mut appended = fs::read_to_string(&source).unwrap();
    appended.push_str(&format!("{}\n", user("one", "second needle")));
    fs::write(&source, appended).unwrap();
    let both = task.run(&["search", "--query", "needle", "--file", file]);
    assert_eq!(both.as_array().unwrap().len(), 2);
    let replacement = format!("replacement needle {}", "x".repeat(300));
    fs::write(&source, lines(&[user("two", &replacement)])).unwrap();
    let replaced = task.run(&["search", "--query", "needle", "--file", file]);
    assert_eq!(replaced[0]["sessionId"], "two");
    assert_eq!(replaced.as_array().unwrap().len(), 1);
    let index = task.tasks().join("session-index/index.json");
    assert_eq!(mode(index.parent().unwrap()), 0o700);
    assert_eq!(mode(&index), 0o600);
    assert_eq!(task.run(&["index", "--file", file])["indexedTurns"], 1);
}

#[test]
fn removes_deleted_and_rotated_source_contributions_without_stale_provenance() {
    let task = Task::new();
    let directory = task.home().join(".codex/sessions");
    let (first, second) = (
        directory.join("first.jsonl"),
        directory.join("second.jsonl"),
    );
    let entry = json!({"type": "user", "sessionId": "shared", "timestamp": 1_700_000_000,
        "message": {"role": "user", "content": "shared needle"}});
    task.write(&first, &lines(std::slice::from_ref(&entry)));
    task.write(&second, &lines(std::slice::from_ref(&entry)));
    let result = &task.run(&["search", "--query", "shared needle"])[0];
    assert_eq!(
        sources(result),
        [first.to_str().unwrap(), second.to_str().unwrap()]
    );
    fs::remove_file(&second).unwrap();
    fs::rename(&first, &second).unwrap();
    let mut replacement = entry;
    replacement["message"]["content"] = json!("replacement text");
    fs::write(&first, lines(&[replacement])).unwrap();
    let result = &task.run(&["search", "--query", "shared needle"])[0];
    assert_eq!(result["sources"], json!([second.to_str().unwrap()]));
}

#[test]
fn detects_same_size_rewrites_outside_the_prefix() {
    let task = Task::new();
    let source = task.file("same-size.jsonl");
    let file = source.to_str().unwrap();
    let entry = |word: &str| lines(&[user("rewrite", &format!("{} {word}", "p".repeat(5_000)))]);
    task.write(&source, &entry("alpha"));
    let count = |word: &str| {
        task.run(&["search", "--query", word, "--file", file])
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!(count("alpha"), 1);
    fs::write(&source, entry("bravo")).unwrap();
    let future = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
    fs::File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_modified(future)
        .unwrap();
    assert_eq!(count("alpha"), 0);
    assert_eq!(count("bravo"), 1);
}

#[test]
fn prioritizes_an_exact_session_filename_beyond_a_discovery_cap() {
    let task = Task::new();
    let directory = task.home().join(".codex/sessions");
    for index in 0..4 {
        task.write(
            &directory.join(format!("a-{index}.jsonl")),
            &lines(&[user(&format!("other-{index}"), "other")]),
        );
    }
    let target = directory.join("z-hidden.jsonl");
    let message = json!({"type": "response_item", "payload": {"type": "message", "role": "user",
        "content": [{"type": "input_text", "text": "target request"}]}});
    task.write(
        &target,
        &lines(&[
            json!({"type": "session_meta", "payload": {"id": "target-session"}}),
            message,
        ]),
    );
    let resolved = task.run(&["resolve", "--session", "target-session", "--max-files", "1"]);
    assert_eq!(resolved["sources"], json!([target.to_str().unwrap()]));
    assert_eq!(resolved["coverage"]["scanLimitReached"], true);
}

#[test]
fn reports_coverage_for_zero_search_results() {
    let task = Task::new();
    let output = task.raw(&["search", "--query", "absent"]);
    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!([])
    );
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(error["coverage"]["discovered"].is_number());
}

#[test]
fn excludes_inherited_fork_turns_from_human_effort_but_resolves_them() {
    let task = Task::new();
    let source = task.file("fork.jsonl");
    let file = source.to_str().unwrap();
    let message = |ordinal: u32, text: &str| {
        json!({"ordinal": ordinal, "type": "response_item", "payload": {"type": "message",
            "role": "user", "content": [{"type": "input_text", "text": text}]}})
    };
    task.write(
        &source,
        &lines(&[
            json!({"type": "session_meta", "payload": {"id": "fork-id", "forked_from_id": "parent-id", "subagent_history_start_ordinal": 2}}),
            message(1, "inherited request"),
            message(2, "new fork request"),
        ]),
    );
    let stats = task.run(&["index", "--file", file]);
    assert_eq!(stats["humanTurns"], 1);
    assert_eq!(stats["inheritedTurns"], 1);
    assert_eq!(
        task.run(&["resolve", "--session", "fork-id", "--file", file])["turns"],
        2
    );
    let hidden = task.run(&["search", "--query", "inherited", "--file", file]);
    assert_eq!(hidden.as_array().unwrap().len(), 0);
    let explicit = task.run(&["search", "--query", "inherited", "--session", "fork-id"]);
    assert_eq!(explicit.as_array().unwrap().len(), 1);
    assert_eq!(explicit[0]["inherited"], true);
}

#[test]
fn advances_through_an_oversized_line_under_an_explicit_byte_budget() {
    let task = Task::new();
    let source = task.file("oversized.jsonl");
    let file = source.to_str().unwrap();
    task.write(
        &source,
        &lines(&[
            user("large", &"x".repeat(1_100_000)),
            user("after", "after needle"),
        ]),
    );
    let first = task.run(&["index", "--file", file, "--byte-limit", "600000"]);
    assert_eq!(first["byteLimitReached"], true);
    let second = task.run(&["index", "--file", file, "--byte-limit", "600000"]);
    assert_eq!(second["oversizedLines"], 1);
    let found = task.run(&["search", "--query", "after needle", "--file", file]);
    assert_eq!(found.as_array().unwrap().len(), 1);
}

#[test]
fn help_lists_the_task_commands_and_a_bare_run_is_a_usage_error() {
    let task = Task::new();
    for flag in ["--help", "help"] {
        let output = task.raw(&[flag]);
        assert!(output.status.success());
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(help.contains("create") && help.contains("check"), "{help}");
    }
    assert_eq!(task.raw(&[]).status.code(), Some(2));
}

#[test]
fn refuses_a_symbolic_link_session_index_directory() {
    let task = Task::new();
    let target = task.file("index-target");
    fs::create_dir_all(task.tasks()).unwrap();
    fs::create_dir(&target).unwrap();
    symlink(&target, task.tasks().join("session-index")).unwrap();
    let error = task.fail(&["index"]);
    assert!(error.contains("must not be a symbolic link"), "{error}");
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
fn valid_eof_records_stay_visible_and_partial_utf8_appends_do_not_duplicate_them() {
    let task = Task::new();
    let source = task.file("eof.jsonl");
    let file = source.to_str().unwrap();
    let record = |text: &str| user("eof", text).to_string();
    let count = || {
        task.run(&["search", "--query", "needle", "--file", file])
            .as_array()
            .unwrap()
            .len()
    };
    fs::write(&source, record("needle caf\u{e9}")).unwrap();
    assert_eq!(count(), 1);
    assert_eq!(task.run(&["index", "--file", file])["scannedBytes"], 0);
    let second = record("needle second");
    let mut text = fs::read_to_string(&source).unwrap();
    text.push('\n');
    text.push_str(&second[..second.len() - 2]);
    fs::write(&source, &text).unwrap();
    assert_eq!(count(), 1);
    text.push_str("}}");
    fs::write(&source, &text).unwrap();
    assert_eq!(count(), 2);
    let first = record("needle caf\u{e9}");
    fs::write(&source, format!("{first}\n{second}")).unwrap();
    let complete = task.run(&["search", "--query", "needle", "--file", file]);
    assert_eq!(complete.as_array().unwrap().len(), 2);
    assert_eq!(complete[0]["offset"], first.len() + 1);
    fs::write(&source, format!("{first}\n{second}\n")).unwrap();
    assert_eq!(count(), 2);
}

#[test]
fn deleting_one_duplicate_removes_only_its_provenance() {
    let task = Task::new();
    let directory = task.home().join(".claude/projects/test");
    let (a, b) = (directory.join("a.jsonl"), directory.join("b.jsonl"));
    let row = lines(&[
        json!({"uuid": "shared", "type": "user", "sessionId": "dedup",
        "message": {"role": "user", "content": "needle duplicate"}}),
    ]);
    task.write(&a, &row);
    task.write(&b, &row);
    assert_eq!(
        task.run(&["search", "--query", "needle"])[0]["sources"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    fs::remove_file(&b).unwrap();
    assert_eq!(
        task.run(&["search", "--query", "needle"])[0]["sources"],
        json!([a.to_str().unwrap()])
    );
}

#[test]
fn a_growing_rewrite_near_the_old_tail_rebuilds_the_source() {
    let task = Task::new();
    let source = task.file("rewrite.jsonl");
    let file = source.to_str().unwrap();
    let prelude = format!("{}\n", json!({"type": "meta", "padding": "x".repeat(5000)}));
    let row = |text: &str| format!("{}\n", user("rewrite", text));
    task.write(&source, &format!("{prelude}{}", row("old needle")));
    task.run(&["index", "--file", file]);
    fs::write(
        &source,
        format!("{prelude}{}", row("replacement needle that is longer")),
    )
    .unwrap();
    let found = task.run(&["search", "--query", "needle", "--file", file]);
    assert_eq!(texts(&found), ["replacement needle that is longer"]);
    assert_eq!(
        task.run(&["index", "--rebuild", "--file", file])["humanTurns"],
        1
    );
}

#[test]
fn history_duplicates_use_ordered_transcript_context_and_show_the_matched_text() {
    let task = Task::new();
    let text = format!("{}needle prototype", "x".repeat(700));
    task.write(
        &task.home().join(".claude/history.jsonl"),
        &lines(&[json!({"display": text, "sessionId": "context", "timestamp": 1000})]),
    );
    let transcript = task.home().join(".claude/projects/test/context.jsonl");
    let turn = |kind: &str, at: u32, content: &str| {
        json!({"type": kind, "sessionId": "context", "timestamp": at,
            "message": {"role": kind, "content": content}})
    };
    task.write(
        &transcript,
        &lines(&[
            turn("assistant", 999, "before response"),
            turn("user", 1000, &text),
            turn("assistant", 1001, "after response"),
        ]),
    );
    let result = &task.run(&["search", "--query", "needle"])[0];
    assert_eq!(result["source"], transcript.to_str().unwrap());
    assert!(
        result["text"]
            .as_str()
            .unwrap()
            .contains("needle prototype")
    );
    let roles: Vec<&str> = result["context"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["assistant", "user", "assistant"]);
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
    usage(&["search"], "--query <QUERY>");
    assert!(
        task.fail(&["resolve", "nope-id"])
            .contains("session nope-id was not found")
    );
}

#[test]
fn the_index_file_keeps_its_format() {
    let task = Task::new();
    let source = task.file("format.jsonl");
    task.write(&source, &lines(&[user("fmt", "needle")]));
    task.run(&["index", "--file", source.to_str().unwrap()]);
    let index: Value = serde_json::from_str(
        &fs::read_to_string(task.tasks().join("session-index/index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(index["version"], 4);
    let entry = &index["files"][source.to_str().unwrap()];
    for key in [
        "dev",
        "ino",
        "size",
        "offset",
        "sessionId",
        "forkedFrom",
        "historyStart",
        "malformed",
        "oversized",
        "skipLine",
        "partial",
        "prefix",
        "tailHash",
        "modified",
        "changed",
        "turns",
    ] {
        assert!(entry.get(key).is_some(), "missing {key}");
    }
    let turn = &entry["turns"][0];
    for key in [
        "source",
        "sourceType",
        "host",
        "sessionId",
        "timestamp",
        "role",
        "text",
        "offset",
        "lineBytes",
        "eventId",
        "inherited",
    ] {
        assert!(turn.get(key).is_some(), "missing turn {key}");
    }
    assert!(index["coverage"]["indexedTurns"].is_number());
    assert!(index["updatedAt"].is_string());
}
