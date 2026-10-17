//! `create`, `list`, `show`, `update` and `check`: one pretty-printed JSON file per task in
//! `agents/tasks`, written exactly as `agent-task` writes it.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::core::clock;
use crate::core::error::{Error, Result, ctx, fail};
use crate::core::output::Output;
use crate::task::args::Options;
use crate::task::files::{self, Lock};
use crate::task::text::{js_number, limited, safe_integer};

const STATUSES: [&str; 4] = ["active", "paused", "blocked", "complete"];

pub fn pretty(value: &Value) -> Result<Output> {
    let mut text = serde_json::to_string_pretty(value).map_err(ctx("serialize output"))?;
    text.push('\n');
    Ok(Output::Text(text))
}

/// `[a-z0-9][a-z0-9._-]{0,63}`
fn is_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && id.len() <= 64
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn validate_id(id: Option<&str>) -> Result<String> {
    match id {
        Some(id) if is_id(id) => Ok(id.to_owned()),
        _ => fail("id must match [a-z0-9][a-z0-9._-]{0,63}"),
    }
}

fn task_path(id: &str) -> Result<PathBuf> {
    Ok(files::root()?.join(format!("{id}.json")))
}

fn read_task(id: &str) -> Result<Value> {
    let file = task_path(id)?;
    let missing = |error: &std::io::Error| error.kind() == ErrorKind::NotFound;
    let metadata = fs::symlink_metadata(&file).map_err(|error| {
        if missing(&error) {
            Error(format!("task {id} does not exist"))
        } else {
            Error(format!("read {}: {error}", file.display()))
        }
    })?;
    if metadata.file_type().is_symlink() {
        return fail(format!("task {id} must not be a symbolic link"));
    }
    let mut raw = String::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&file)
        .and_then(|mut handle| handle.read_to_string(&mut raw))
        .map_err(ctx(format!("read {}", file.display())))?;
    let task: Value =
        serde_json::from_str(&raw).map_err(|_| Error(format!("task {id} is invalid")))?;
    let revision = task.get("revision").and_then(Value::as_f64);
    if task.get("id").and_then(Value::as_str) != Some(id)
        || revision.and_then(safe_integer).is_none()
    {
        return fail(format!("task {id} is invalid"));
    }
    Ok(task)
}

fn with_lock<T>(id: &str, callback: impl FnOnce() -> Result<T>) -> Result<T> {
    let root = files::ensure_root()?;
    let _lock = Lock::acquire(
        root.join(format!(".{id}.lock")),
        None,
        &format!("task {id} is being updated; retry"),
    )?;
    callback()
}

fn write_task(task: &Value) -> Result<()> {
    let id = task.get("id").and_then(Value::as_str).unwrap_or_default();
    let file = task_path(id)?;
    let temporary = files::root()?.join(format!(".{id}.{}.json", std::process::id()));
    let mut data = serde_json::to_string_pretty(task).map_err(ctx("serialize task"))?;
    data.push('\n');
    files::write_private(&file, &temporary, data.as_bytes(), true)
}

fn git(args: &[&str], dir: &std::path::Path) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn context() -> Value {
    let cwd = std::env::current_dir().unwrap_or_default();
    let repo = git(&["rev-parse", "--show-toplevel"], &cwd);
    let head = repo
        .as_deref()
        .and_then(|repo| git(&["rev-parse", "HEAD"], std::path::Path::new(repo)));
    json!({
        "host": files::hostname(),
        "repo": repo,
        "worktree": repo.as_ref().map(|_| cwd.to_string_lossy()),
        "head": head,
    })
}

fn revision_argument(options: &Options) -> Result<i64> {
    let expected = options.get("revision").map_or(f64::NAN, js_number);
    match safe_integer(expected) {
        Some(revision) if revision >= 1 => Ok(revision),
        _ => fail("--revision must be a positive integer"),
    }
}

fn optional(options: &Options, key: &str) -> Result<Value> {
    options.get(key).map_or(Ok(Value::Null), |value| {
        limited(Some(value), key).map(Value::String)
    })
}

pub fn create(options: &Options) -> Result<Output> {
    let id = validate_id(options.id_argument())?;
    let objective = limited(options.get("objective"), "objective")?;
    with_lock(&id, || {
        if task_path(&id)?.exists() {
            return fail(format!("task {id} already exists"));
        }
        let now = clock::now_iso_millis()?;
        let parent_pr = optional(options, "parent-pr")?;
        let target_branch = optional(options, "target-branch")?;
        let task_boundary = optional(options, "task-boundary")?;
        let context = context();
        let authorization = match options.get("authorization") {
            Some(provenance) => json!({
                "provenance": limited(Some(provenance), "authorization")?,
                "scope": limited(Some(options.get("scope").unwrap_or("unspecified")), "scope")?,
            }),
            None => Value::Null,
        };
        let next_step = optional(options, "next-step")?;
        write_task(&json!({
            "version": 1,
            "id": id,
            "revision": 1,
            "objective": objective,
            "parentPr": parent_pr,
            "targetBranch": target_branch,
            "taskBoundary": task_boundary,
            "corrections": [],
            "context": context,
            "checks": [],
            "authorization": authorization,
            "nextStep": next_step,
            "status": "active",
            "createdAt": now,
            "updatedAt": now,
        }))
    })?;
    show_task(&id)
}

fn set(task: &mut Value, key: &str, value: Value) {
    if let Some(map) = task.as_object_mut() {
        map.insert(key.to_owned(), value);
    }
}

fn bump(task: &mut Value, revision: i64, at: &str) {
    set(task, "revision", json!(revision + 1));
    set(task, "updatedAt", json!(at));
}

fn check_revision(task: &Value, expected: i64) -> Result<i64> {
    let current = task
        .get("revision")
        .and_then(Value::as_f64)
        .and_then(safe_integer)
        .unwrap_or_default();
    if current != expected {
        return fail(format!(
            "stale revision: expected {expected}, current {current}"
        ));
    }
    Ok(current)
}

pub fn update(options: &Options) -> Result<Output> {
    let id = validate_id(options.id_argument())?;
    let expected = revision_argument(options)?;
    with_lock(&id, || {
        let mut task = read_task(&id)?;
        let revision = check_revision(&task, expected)?;
        for (option, field) in [
            ("objective", "objective"),
            ("parent-pr", "parentPr"),
            ("target-branch", "targetBranch"),
            ("task-boundary", "taskBoundary"),
        ] {
            if let Some(value) = options.get(option) {
                set(&mut task, field, json!(limited(Some(value), option)?));
            }
        }
        if let Some(text) = options.get("correction") {
            let entry = json!({
                "text": limited(Some(text), "correction")?,
                "at": clock::now_iso_millis()?,
            });
            match task.get_mut("corrections").and_then(Value::as_array_mut) {
                Some(list) => list.push(entry),
                None => set(&mut task, "corrections", json!([entry])),
            }
        }
        if let Some(value) = options.get("next-step") {
            set(
                &mut task,
                "nextStep",
                json!(limited(Some(value), "next-step")?),
            );
        }
        if let Some(status) = options.get("status") {
            if !STATUSES.contains(&status) {
                return fail("invalid status");
            }
            set(&mut task, "status", json!(status));
        }
        if options.get("authorization").is_some() || options.get("scope").is_some() {
            let stored = |field: &str| {
                task.pointer(&format!("/authorization/{field}"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            };
            let provenance = options
                .get("authorization")
                .map(str::to_owned)
                .or_else(|| stored("provenance"));
            let scope = options
                .get("scope")
                .map(str::to_owned)
                .or_else(|| stored("scope"));
            let authorization = json!({
                "provenance": limited(provenance.as_deref(), "authorization")?,
                "scope": limited(scope.as_deref(), "scope")?,
            });
            set(&mut task, "authorization", authorization);
        }
        set(&mut task, "context", context());
        bump(&mut task, revision, &clock::now_iso_millis()?);
        write_task(&task)
    })?;
    show_task(&id)
}

fn platform() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{os} {arch} {}", files::hostname())
}

pub fn check(options: &Options) -> Result<Output> {
    let id = validate_id(options.id_argument())?;
    let expected = revision_argument(options)?;
    with_lock(&id, || {
        let mut task = read_task(&id)?;
        let revision = check_revision(&task, expected)?;
        let name = limited(options.get("name"), "name")?;
        let result = limited(options.get("result"), "result")?;
        let code_revision = match options.get("code-revision") {
            Some(value) => json!(limited(Some(value), "code-revision")?),
            None => task
                .pointer("/context/head")
                .cloned()
                .unwrap_or(Value::Null),
        };
        let inputs = optional(options, "inputs")?;
        let environment = match options.get("environment") {
            Some(value) => limited(Some(value), "environment")?,
            None => platform(),
        };
        let at = clock::now_iso_millis()?;
        let item = json!({
            "name": name,
            "result": result,
            "revision": code_revision,
            "inputs": inputs,
            "environment": environment,
            "at": at,
        });
        match task.get_mut("checks").and_then(Value::as_array_mut) {
            Some(list) => list.push(item),
            None => set(&mut task, "checks", json!([item])),
        }
        bump(&mut task, revision, &at);
        write_task(&task)
    })?;
    show_task(&id)
}

fn show_task(id: &str) -> Result<Output> {
    pretty(&read_task(id)?)
}

pub fn show(options: &Options) -> Result<Output> {
    show_task(&validate_id(options.id_argument())?)
}

pub fn list() -> Result<Output> {
    let root = files::ensure_root()?;
    let mut tasks = Vec::new();
    let mut entries = fs::read_dir(&root)
        .and_then(|entries| entries.collect::<std::io::Result<Vec<_>>>())
        .map_err(ctx(format!("read {}", root.display())))?;
    // Node lists a directory sorted by name, which settles ties between equal timestamps.
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if !is_id(id) || entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
            continue;
        }
        tasks.push(read_task(id)?);
    }
    let updated = |task: &Value| {
        task.get("updatedAt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    tasks.sort_by_key(|task| std::cmp::Reverse(updated(task)));
    let summaries: Vec<Value> = tasks
        .iter()
        .map(|task| {
            let field = |name: &str| task.get(name).cloned().unwrap_or(Value::Null);
            json!({
                "id": field("id"),
                "revision": field("revision"),
                "status": field("status"),
                "updatedAt": field("updatedAt"),
                "objective": field("objective"),
            })
        })
        .collect();
    pretty(&Value::Array(summaries))
}
