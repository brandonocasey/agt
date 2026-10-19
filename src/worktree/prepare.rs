//! `agt worktree prepare`: install a worktree's dependencies from its lockfiles, logging each
//! command's output under `$XDG_CACHE_HOME/agents/output/`. Dependency directories are never
//! symlinked from another checkout; `--clone-deps` copies them copy-on-write instead.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::Args;
use serde_json::{Value, json};

use super::emit;
use super::info::{main_checkout, toplevel};
use super::setup::{self, Step};
use crate::core::error::{Error, Result, ctx, fail};
use crate::core::output::Output;
use crate::git::Repo;

const MISE_FILES: [&str; 3] = ["mise.toml", ".mise.toml", ".tool-versions"];

#[derive(Args)]
pub struct Cli {
    /// The worktree; default: the current one.
    path: Option<PathBuf>,
    /// Copy the main checkout's `node_modules` directories copy-on-write instead of installing.
    #[arg(long)]
    clone_deps: bool,
    /// Wait for the background setup `create --background` started, then print its report; exit
    /// 1 when it failed and 3 when `--max` passes first.
    #[arg(long, conflicts_with = "clone_deps")]
    wait: bool,
    /// Run only the `[worktree.hooks] create` commands, no install steps (to rerun hooks that failed).
    #[arg(long, conflicts_with_all = ["wait", "clone_deps"])]
    hooks: bool,
    /// Give up waiting after this many seconds.
    #[arg(long, default_value_t = 270, requires = "wait", value_parser = clap::value_parser!(u64).range(1..))]
    max: u64,
    /// Internal: run these steps (`prepare`, `hooks`) as the background setup child.
    #[arg(
        long,
        hide = true,
        value_delimiter = ',',
        value_parser = ["prepare", "hooks"],
        conflicts_with_all = ["clone_deps", "wait", "hooks"]
    )]
    setup: Vec<String>,
}

pub fn run(cli: Cli) -> Result<Output> {
    let dir = match cli.path {
        Some(path) => path,
        None => std::env::current_dir().map_err(ctx("read the current directory"))?,
    };
    if cli.wait {
        return setup::wait(&dir, cli.max);
    }
    if !cli.setup.is_empty() {
        let steps: Vec<_> = cli
            .setup
            .iter()
            .filter_map(|name| Step::parse(name))
            .collect();
        return setup::child(&dir, &steps);
    }
    setup::refuse_while_running(&toplevel(&dir)?)?;
    if cli.hooks {
        let root = toplevel(&dir)?;
        let (report, ok) = super::hooks::run(&root, super::hooks::Kind::Create)?;
        return emit(json!({"path": root, "hooks": report, "ok": ok}), ok);
    }
    let (value, ok) = prepare(&dir, cli.clone_deps)?;
    emit(value, ok)
}

/// Runs the steps for the worktree containing `dir`; the flag says every step succeeded.
pub(super) fn prepare(dir: &Path, clone_deps: bool) -> Result<(Value, bool)> {
    let root = toplevel(dir)?;
    let main = main_checkout(&Repo::at(&root))?.path;
    let same = fs::canonicalize(&main).ok() == fs::canonicalize(&root).ok();
    let mut steps = Vec::new();
    let mut commands: Vec<(Vec<String>, Option<&str>)> = Vec::new();
    if MISE_FILES.iter().any(|name| root.join(name).exists()) && on_path("mise") {
        commands.push((words(&["mise", "install"]), None));
    }
    let mut cloned = Vec::new();
    if clone_deps {
        if same {
            return fail("this is the main checkout; there is nothing to clone from");
        }
        cloned = clone(&main, &root)?;
        if root.join("package-lock.json").exists() {
            commands.push((words(&["npm", "run", "prepare", "--if-present"]), None));
        }
    }
    // A clone replaces only the node installs; every other install still runs.
    commands.extend(
        installs(&root)
            .into_iter()
            .filter(|(file, _)| !clone_deps || !is_node_lockfile(file))
            .map(|(file, cmd)| (cmd, Some(file))),
    );
    let mut clone_available = false;
    for (index, (command, lockfile)) in commands.iter().enumerate() {
        let mut step = run_step(&root, command, index, &[])?;
        let failed = step["ok"] != true;
        // A clone copies `node_modules` only, so it is offered for a failed node install whose
        // lockfile the main checkout shares and whose `node_modules` it has.
        if failed
            && lockfile.is_some_and(|file| {
                !same
                    && is_node_lockfile(file)
                    && identical(&main, &root, file)
                    && has_node_modules(&main)
            })
        {
            step["clone_available"] = json!(true);
            clone_available = true;
        }
        steps.push(step);
    }
    let ok = steps.iter().all(|step| step["ok"] == true);
    let value = json!({
        "path": root,
        "deps_from": if clone_deps { "clone" } else { "install" },
        "cloned": cloned,
        "steps": steps,
        "clone_available": clone_available,
        "ok": ok,
    });
    Ok((value, ok))
}

fn words(command: &[&str]) -> Vec<String> {
    command.iter().map(|word| (*word).to_owned()).collect()
}

/// The install for each lockfile in `root`, as `(lockfile, command)`.
fn installs(root: &Path) -> Vec<(&'static str, Vec<String>)> {
    let table: [(&str, &[&str]); 9] = [
        ("package-lock.json", &["npm", "ci"]),
        ("pnpm-lock.yaml", &["pnpm", "install", "--frozen-lockfile"]),
        ("yarn.lock", &[]),
        ("bun.lock", &["bun", "install", "--frozen-lockfile"]),
        ("bun.lockb", &["bun", "install", "--frozen-lockfile"]),
        ("uv.lock", &["uv", "sync", "--frozen"]),
        ("poetry.lock", &["poetry", "install"]),
        ("Cargo.lock", &["cargo", "fetch", "--locked"]),
        ("go.sum", &["go", "mod", "download"]),
    ];
    let mut found: Vec<(&'static str, Vec<String>)> = Vec::new();
    for (file, command) in table {
        if !root.join(file).exists() {
            continue;
        }
        let command = match file {
            "yarn.lock" if modern_yarn(root) => words(&["yarn", "install", "--immutable"]),
            "yarn.lock" => words(&["yarn", "install", "--frozen-lockfile"]),
            _ => words(command),
        };
        // `bun.lock` and `bun.lockb` together run one install.
        if !found.iter().any(|(_, existing)| *existing == command) {
            found.push((file, command));
        }
    }
    found
}

/// Yarn 2 and later: a `.yarnrc.yml`, or a `packageManager` field naming yarn 2 or newer.
fn modern_yarn(root: &Path) -> bool {
    if root.join(".yarnrc.yml").exists() {
        return true;
    }
    let manager = fs::read(root.join("package.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|manifest| manifest["packageManager"].as_str().map(str::to_owned));
    manager
        .and_then(|name| {
            let version = name.strip_prefix("yarn@")?;
            version.split('.').next()?.parse::<u32>().ok()
        })
        .is_some_and(|major| major >= 2)
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// The lockfiles of the installs that fill a `node_modules`.
fn is_node_lockfile(file: &str) -> bool {
    matches!(
        file,
        "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock" | "bun.lock" | "bun.lockb"
    )
}

fn has_node_modules(main: &Path) -> bool {
    let mut found = Vec::new();
    find_modules(main, Path::new(""), &mut found).is_ok() && !found.is_empty()
}

fn identical(main: &Path, root: &Path, file: &str) -> bool {
    match (fs::read(main.join(file)), fs::read(root.join(file))) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `$XDG_CACHE_HOME/agents/output`, creating it.
pub(super) fn output_dir() -> Result<PathBuf> {
    let cache = match std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        _ => {
            let home = std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .ok_or_else(|| Error("HOME is not set".into()))?;
            PathBuf::from(home).join(".cache")
        }
    };
    let dir = cache.join("agents").join("output");
    fs::create_dir_all(&dir).map_err(ctx(format!("create {}", dir.display())))?;
    Ok(dir)
}

/// Runs `command` in `root` with `env` added, logging its output; the report says how it went.
pub(super) fn run_step(
    root: &Path,
    command: &[String],
    index: usize,
    env: &[(&str, String)],
) -> Result<Value> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let name = format!(
        "worktree-prepare-{}-{nanos}-{index}.log",
        std::process::id()
    );
    let log = output_dir()?.join(name);
    let sink = File::create(&log).map_err(ctx(format!("create {}", log.display())))?;
    let again = sink.try_clone().map_err(ctx("duplicate the log handle"))?;
    let started = Instant::now();
    let status = Command::new(&command[0])
        .args(&command[1..])
        .current_dir(root)
        // An inherited PORT must not leak into a step; the caller's `env` may set it.
        .env_remove("PORT")
        .envs(env.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(sink)
        .stderr(again)
        .status();
    let mut step = json!({
        "command": command.join(" "),
        "exit": status.as_ref().ok().and_then(|status| status.code()),
        "ok": status.as_ref().is_ok_and(|status| status.success()),
        "duration_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "log": log,
    });
    if let Err(error) = status {
        step["error"] = json!(format!("run {}: {error}", command[0]));
    }
    Ok(step)
}

/// Copies every `node_modules` of `main` that `root` lacks, relative path preserved, and returns
/// the relative paths.
fn clone(main: &Path, root: &Path) -> Result<Vec<String>> {
    let mut found = Vec::new();
    find_modules(main, Path::new(""), &mut found)?;
    if found.is_empty() {
        return fail(format!("no node_modules in {}", main.display()));
    }
    let mut copied = Vec::new();
    for relative in found {
        let to = root.join(&relative);
        if to.exists() {
            continue;
        }
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(ctx(format!("create {}", parent.display())))?;
        }
        let mut copy = Command::new("cp");
        copy.arg(if cfg!(target_os = "macos") {
            "-cR"
        } else {
            "--reflink=auto"
        });
        if !cfg!(target_os = "macos") {
            copy.arg("-R");
        }
        let output = copy
            .arg(main.join(&relative))
            .arg(&to)
            .output()
            .map_err(ctx("run cp"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return fail(format!(
                "cp {} failed: {}",
                relative.display(),
                stderr.trim()
            ));
        }
        copied.push(relative.to_string_lossy().into_owned());
    }
    Ok(copied)
}

/// Finds `node_modules` directories under `main/relative`, without entering one or `.git`.
fn find_modules(main: &Path, relative: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    let dir = main.join(relative);
    let mut names: Vec<_> = fs::read_dir(&dir)
        .map_err(ctx(format!("read {}", dir.display())))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.file_name())
        .collect();
    names.sort();
    for name in names {
        let child = relative.join(&name);
        if name == "node_modules" {
            found.push(child);
        } else if name != ".git" && name != ".worktrees" {
            find_modules(main, &child, found)?;
        }
    }
    Ok(())
}
