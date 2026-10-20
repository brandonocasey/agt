//! `create`, `list`, `show` and `update` (which also records a check): one pretty-printed JSON file per task in
//! `agents/tasks`.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::core::clock;
use crate::core::error::{Error, Result, ctx, fail};
use crate::core::local::{hostname, lock_file_or_busy, write_json};
use crate::core::output::Output;
use crate::git::Repo;
use crate::task::args::{Create, Target, Update};
use crate::task::files;
use crate::task::text::limited;

/// `[a-z0-9][a-z0-9._-]{0,63}`
pub(super) fn is_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && id.len() <= 64
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn task_path(id: &str) -> Result<PathBuf> {
    Ok(files::root()?.join(format!("{id}.json")))
}

pub(super) fn read_task(id: &str) -> Result<Value> {
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
    if task.get("id").and_then(Value::as_str) != Some(id)
        || task.get("revision").and_then(Value::as_i64).is_none()
    {
        return fail(format!("task {id} is invalid"));
    }
    Ok(task)
}

fn with_lock<T>(id: &str, callback: impl FnOnce() -> Result<T>) -> Result<T> {
    files::ensure_root()?;
    let _lock = lock_file_or_busy(
        &files::lock_path(id)?,
        None,
        &format!("task {id} is being updated; retry"),
    )?;
    callback()
}

fn write_task(task: &Value) -> Result<()> {
    let id = task.get("id").and_then(Value::as_str).unwrap_or_default();
    let file = task_path(id)?;
    write_json(&file, task, true)
}

fn context() -> Value {
    let cwd = std::env::current_dir().unwrap_or_default();
    let repo = Repo::at(&cwd).toplevel().ok();
    let head = repo.as_deref().and_then(|repo| Repo::at(repo).head().ok());
    json!({
        "host": hostname(),
        "repo": repo,
        "worktree": repo.as_ref().map(|_| cwd.to_string_lossy()),
        "head": head,
    })
}

fn optional(value: Option<&String>, name: &str) -> Result<Value> {
    value.map_or(Ok(Value::Null), |value| {
        limited(Some(value), name).map(Value::String)
    })
}

pub fn create(args: &Create) -> Result<Output> {
    let id = args.target.id()?;
    let objective = limited(Some(&args.objective), "objective")?;
    with_lock(&id, || {
        if task_path(&id)?.exists() {
            return fail(format!("task {id} already exists"));
        }
        let now = clock::now_iso_millis()?;
        let parent_pr = optional(args.fields.parent_pr.as_ref(), "parent-pr")?;
        let target_branch = optional(args.fields.target_branch.as_ref(), "target-branch")?;
        let task_boundary = optional(args.fields.task_boundary.as_ref(), "task-boundary")?;
        let context = context();
        let authorization = match &args.fields.authorization {
            Some(provenance) => json!({
                "provenance": limited(Some(provenance), "authorization")?,
                "scope": limited(Some(args.fields.scope.as_deref().unwrap_or("unspecified")), "scope")?,
            }),
            None => Value::Null,
        };
        let next_step = optional(args.fields.next_step.as_ref(), "next-step")?;
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
        .and_then(Value::as_i64)
        .unwrap_or_default();
    if current != expected {
        return fail(format!(
            "stale revision: expected {expected}, current {current}"
        ));
    }
    Ok(current)
}

pub fn update(args: &Update) -> Result<Output> {
    let id = args.target.id()?;
    with_lock(&id, || {
        let mut task = read_task(&id)?;
        let revision = check_revision(&task, args.revision)?;
        for (option, value, field) in [
            ("objective", &args.objective, "objective"),
            ("parent-pr", &args.fields.parent_pr, "parentPr"),
            ("target-branch", &args.fields.target_branch, "targetBranch"),
            ("task-boundary", &args.fields.task_boundary, "taskBoundary"),
            ("next-step", &args.fields.next_step, "nextStep"),
        ] {
            if let Some(value) = value {
                set(&mut task, field, json!(limited(Some(value), option)?));
            }
        }
        if let Some(text) = &args.correction {
            let entry = json!({
                "text": limited(Some(text), "correction")?,
                "at": clock::now_iso_millis()?,
            });
            match task.get_mut("corrections").and_then(Value::as_array_mut) {
                Some(list) => list.push(entry),
                None => set(&mut task, "corrections", json!([entry])),
            }
        }
        if let Some(status) = args.status {
            set(&mut task, "status", json!(status.as_str()));
        }
        if args.fields.authorization.is_some() || args.fields.scope.is_some() {
            let stored = |field: &str| {
                task.pointer(&format!("/authorization/{field}"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            };
            let provenance = args
                .fields
                .authorization
                .clone()
                .or_else(|| stored("provenance"));
            let scope = args.fields.scope.clone().or_else(|| stored("scope"));
            let authorization = json!({
                "provenance": limited(provenance.as_deref(), "authorization")?,
                "scope": limited(scope.as_deref(), "scope")?,
            });
            set(&mut task, "authorization", authorization);
        }
        set(&mut task, "context", context());
        let at = clock::now_iso_millis()?;
        if let Some(name) = &args.check_name {
            add_check(&mut task, args, name, &at)?;
        }
        bump(&mut task, revision, &at);
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
    format!("{os} {arch} {}", hostname())
}

/// Appends the check `update --check-name` names, recorded at `at`.
fn add_check(task: &mut Value, args: &Update, name: &str, at: &str) -> Result<()> {
    let name = limited(Some(name), "check-name")?;
    let result = limited(args.check_result.as_deref(), "check-result")?;
    let code_revision = match &args.check_code_revision {
        Some(value) => json!(limited(Some(value), "check-code-revision")?),
        None => task
            .pointer("/context/head")
            .cloned()
            .unwrap_or(Value::Null),
    };
    let inputs = optional(args.check_inputs.as_ref(), "check-inputs")?;
    let environment = match &args.check_environment {
        Some(value) => limited(Some(value), "check-environment")?,
        None => platform(),
    };
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
        None => set(task, "checks", json!([item])),
    }
    Ok(())
}

fn show_task(id: &str) -> Result<Output> {
    Output::pretty(&read_task(id)?)
}

pub fn show(target: &Target) -> Result<Output> {
    show_task(&target.id()?)
}

pub fn list() -> Result<Output> {
    let root = files::ensure_root()?;
    let mut tasks = Vec::new();
    let mut entries = fs::read_dir(&root)
        .and_then(|entries| entries.collect::<std::io::Result<Vec<_>>>())
        .map_err(ctx(format!("read {}", root.display())))?;
    // Name order settles ties between equal timestamps.
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
    Output::pretty(&Value::Array(summaries))
}
