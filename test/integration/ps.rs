//! `agt ps` on fake session records, transcripts and processes in a temporary HOME. Live
//! processes are children of the test. The user's own `codex` processes are listed too, so
//! every check picks its rows by pid.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::{Value, json};

use crate::support::Env;

const BIN: &str = env!("CARGO_BIN_EXE_agt");

/// A child process that runs until it is dropped.
struct Proc(Child);

impl Proc {
    fn sleep(program: &str, dir: &Path) -> Self {
        let child = Command::new(program)
            .arg("60")
            .current_dir(dir)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        Self(child)
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The `procStart` Claude Code records for `pid`.
fn proc_start(pid: u32) -> String {
    if cfg!(target_os = "linux") {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let after = &stat[stat.rfind(')').unwrap() + 2..];
        after.split(' ').nth(19).unwrap().to_owned()
    } else {
        let output = Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

fn reused_start() -> &'static str {
    if cfg!(target_os = "linux") {
        "1"
    } else {
        "Mon Jan  1 00:00:00 2024"
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// A session record in the temporary home's `.claude/sessions`.
fn record(env: &Env, pid: u32, start: &str, extra: Value) {
    let dir = env.path("home/.claude/sessions");
    fs::create_dir_all(&dir).unwrap();
    let mut record = json!({
        "pid": pid,
        "procStart": start,
        "kind": "interactive",
        "sessionId": format!("sess-{pid}"),
        "status": "idle",
        "statusUpdatedAt": now_millis() - 120_000,
    });
    record
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    fs::write(dir.join(format!("{pid}.json")), record.to_string()).unwrap();
}

fn call(id: &str, model: &str, input: u64, read: u64, output: u64) -> String {
    json!({
        "type": "assistant",
        "timestamp": "2026-03-01T10:00:05.000Z",
        "message": {
            "id": id,
            "model": model,
            "usage": {
                "input_tokens": input,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": read,
                "output_tokens": output,
            },
            "content": [],
        },
    })
    .to_string()
}

fn transcript(env: &Env, session: &str, lines: &[String]) {
    let dir = env.path("home/.claude/projects/-repo");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{session}.jsonl")),
        lines.join("\n") + "\n",
    )
    .unwrap();
}

fn ps(env: &Env, args: &[&str]) -> Command {
    let mut command = env.command(BIN, &env.repo);
    command
        .env(
            "AGT_PRICES",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("test/fixtures/sessions/prices.json"),
        )
        .arg("ps")
        .args(args);
    command
}

fn rows(env: &Env) -> Vec<Value> {
    let output = ps(env, &["--json"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: Value = serde_json::from_slice(&output.stdout).unwrap();
    parsed["agents"].as_array().unwrap().clone()
}

fn row(rows: &[Value], pid: u32) -> &Value {
    rows.iter()
        .find(|row| row["pid"] == pid)
        .unwrap_or_else(|| panic!("no row for pid {pid}"))
}

/// A main checkout with a commit and a linked worktree `feat`.
fn repo_with_worktree() -> (Env, PathBuf) {
    let env = Env::new();
    env.write("a.txt", "a\n");
    env.commit_all("base");
    let tree = env.path("trees/feat");
    env.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "feat",
        &tree.to_string_lossy(),
    ]);
    (env, tree)
}

#[test]
fn a_session_shows_its_place_model_context_and_cost() {
    let (env, tree) = repo_with_worktree();
    let busy = Proc::sleep("sleep", &tree);
    record(
        &env,
        busy.pid(),
        &proc_start(busy.pid()),
        json!({ "name": "builder", "cwd": tree, "status": "busy", "statusUpdatedAt": now_millis() - 5_000 }),
    );
    transcript(
        &env,
        &format!("sess-{}", busy.pid()),
        &[
            call("m1", "claude-opus-4-8", 1000, 20000, 500),
            call("m2", "claude-opus-4-8", 2000, 30000, 1000),
        ],
    );

    let found = rows(&env);
    let found = row(&found, busy.pid());
    assert_eq!(found["harness"], "claude");
    assert_eq!(found["name"], "builder");
    assert_eq!(found["kind"], "interactive");
    assert_eq!(found["status"], "busy");
    assert!((5..60).contains(&found["status_secs"].as_i64().unwrap()));
    assert_eq!(found["session"], format!("sess-{}", busy.pid()));
    assert_eq!(found["model"], "claude-opus-4-8");
    assert_eq!(found["context_tokens"], 32000);
    assert_eq!(found["usd"], 0.0775);
    assert_eq!(found["unpriced_calls"], 0);
    assert_eq!(found["branch"], "feat");
    let main = fs::canonicalize(&env.repo).unwrap();
    assert_eq!(found["repository"], main.to_string_lossy().as_ref());
    let linked = fs::canonicalize(&tree).unwrap();
    assert_eq!(found["worktree"], linked.to_string_lossy().as_ref());

    let table = String::from_utf8(ps(&env, &[]).output().unwrap().stdout).unwrap();
    assert!(table.contains("builder"), "{table}");
    assert!(table.contains("32.0k"), "{table}");
}

#[test]
fn only_records_of_running_processes_count_and_busy_sessions_sort_first() {
    let env = Env::new();
    let idle = Proc::sleep("sleep", &env.repo);
    let busy = Proc::sleep("sleep", &env.repo);
    let unpriced = Proc::sleep("sleep", &env.repo);
    let reused = Proc::sleep("sleep", &env.repo);
    let mut dead = Command::new("true").spawn().unwrap();
    dead.wait().unwrap();
    let cwd = json!({ "cwd": env.repo });
    record(
        &env,
        idle.pid(),
        &proc_start(idle.pid()),
        json!({ "cwd": env.repo, "status": "waiting", "waitingFor": "dialog open" }),
    );
    record(
        &env,
        busy.pid(),
        &proc_start(busy.pid()),
        json!({ "cwd": env.repo, "status": "busy" }),
    );
    record(
        &env,
        unpriced.pid(),
        &proc_start(unpriced.pid()),
        cwd.clone(),
    );
    record(&env, reused.pid(), reused_start(), cwd.clone());
    record(&env, dead.id(), "Mon Jan  1 00:00:00 2024", cwd);
    transcript(
        &env,
        &format!("sess-{}", unpriced.pid()),
        &[call("m1", "mystery-model", 10, 20, 30)],
    );

    let found = rows(&env);
    for gone in [reused.pid(), dead.id()] {
        assert!(found.iter().all(|row| row["pid"] != gone), "{gone} listed");
    }
    let waiting = row(&found, idle.pid());
    assert_eq!(waiting["status"], "waiting");
    assert_eq!(waiting["waiting_for"], "dialog open");
    assert_eq!(waiting["branch"], "main");
    assert!(waiting["worktree"].is_null());
    assert!(waiting["model"].is_null());
    assert!(waiting["usd"].is_null());
    assert!((120..180).contains(&waiting["status_secs"].as_i64().unwrap()));
    let unpriced_row = row(&found, unpriced.pid());
    assert_eq!(unpriced_row["model"], "mystery-model");
    assert!(unpriced_row["usd"].is_null());
    assert_eq!(unpriced_row["unpriced_calls"], 1);
    let first = found.iter().find(|row| row["harness"] == "claude").unwrap();
    assert_eq!(first["pid"], busy.pid());

    let table = String::from_utf8(ps(&env, &[]).output().unwrap().stdout).unwrap();
    assert!(table.contains("unpriced"), "{table}");
    assert!(table.contains("waits for: dialog open"), "{table}");
}

/// A `codex` process: a copy of `sleep` under that name, run in `dir`.
fn codex(env: &Env, name: &str, dir: &Path) -> Proc {
    let bin: PathBuf = env.path(name);
    fs::create_dir_all(&bin).unwrap();
    let program = bin.join("codex");
    fs::copy(which("sleep"), &program).unwrap();
    Proc::sleep(&program.to_string_lossy(), dir)
}

fn which(name: &str) -> PathBuf {
    ["/bin", "/usr/bin"]
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.exists())
        .unwrap()
}

fn rollout(env: &Env, file: &str, id: &str, cwd: &Path, subagent: bool) {
    let dir = env.path("home/.codex/sessions/2026/10/17");
    fs::create_dir_all(&dir).unwrap();
    let mut payload = json!({ "id": id, "cwd": cwd });
    if subagent {
        payload["source"] = json!({ "subagent": { "thread_spawn": { "parent_thread_id": "x" } } });
    }
    let meta = json!({ "type": "session_meta", "payload": payload });
    fs::write(dir.join(file), format!("{meta}\n")).unwrap();
}

#[test]
fn a_codex_process_gets_its_session_only_when_the_rollout_is_unambiguous() {
    let env = Env::new();
    let matched_dir = env.path("work/matched");
    let shared_dir = env.path("work/shared");
    let missing_dir = env.path("work/missing");
    for dir in [&matched_dir, &shared_dir, &missing_dir] {
        fs::create_dir_all(dir).unwrap();
    }
    let matched = codex(&env, "bin1", &matched_dir);
    let first = codex(&env, "bin2", &shared_dir);
    let second = codex(&env, "bin3", &shared_dir);
    let missing = codex(&env, "bin4", &missing_dir);
    // A rollout from before the process started does not count; a subagent's is skipped.
    let stale = env.path("home/.codex/sessions/2026/10/17/rollout-old.jsonl");
    rollout(&env, "rollout-old.jsonl", "old-id", &missing_dir, false);
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(old)
        .unwrap();
    rollout(&env, "rollout-sub.jsonl", "sub-id", &missing_dir, true);
    rollout(&env, "rollout-a.jsonl", "match-id", &matched_dir, false);
    rollout(&env, "rollout-b.jsonl", "shared-id", &shared_dir, false);

    let found = rows(&env);
    let hit = row(&found, matched.pid());
    assert_eq!(hit["harness"], "codex");
    assert_eq!(hit["session"], "match-id");
    assert!(hit["note"].is_null());
    for pid in [first.pid(), second.pid()] {
        let row = row(&found, pid);
        assert!(row["session"].is_null());
        assert!(row["note"].as_str().unwrap().contains("share"), "{row}");
    }
    let none = row(&found, missing.pid());
    assert!(none["session"].is_null());
    assert!(
        none["note"].as_str().unwrap().contains("no rollout"),
        "{none}"
    );
}
