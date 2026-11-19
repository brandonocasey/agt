//! Running the repository's own git hooks, which libgit2 never does.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::core::process;

/// The executable hook file `name`, from `core.hooksPath` when set (relative to the work tree).
/// Otherwise it is in the `hooks` directory of the common git directory.
fn find(repo: &git2::Repository, name: &str) -> Option<PathBuf> {
    let configured = repo
        .config()
        .ok()
        .and_then(|config| config.get_path("core.hooksPath").ok());
    let dir = match configured {
        Some(path) if path.is_relative() => repo.workdir()?.join(path),
        Some(path) => path,
        None => repo.commondir().join("hooks"),
    };
    let file = dir.join(name);
    let mode = file.metadata().ok()?.permissions().mode();
    (file.is_file() && mode & 0o111 != 0).then_some(file)
}

/// Whether the repository has an executable hook `name`.
pub(super) fn exists(repo: &git2::Repository, name: &str) -> bool {
    find(repo, name).is_some()
}

/// Runs hook `name` with `args` and `stdin` in the work tree; a missing hook succeeds.
/// On failure the error is the hook's output on one line.
pub(super) fn run(
    repo: &git2::Repository,
    name: &str,
    args: &[&str],
    stdin: &str,
) -> Result<(), String> {
    let Some(file) = find(repo, name) else {
        return Ok(());
    };
    let mut command = Command::new(&file);
    command
        .args(args)
        .env("GIT_DIR", repo.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process::keep_user_prompts(&mut command);
    if let Some(workdir) = repo.workdir() {
        command.current_dir(workdir);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("run the {name} hook: {error}"))?;
    if let Some(mut pipe) = child.stdin.take() {
        // A hook that ignores its input closes the pipe; that is not a failure.
        let _ = std::io::Write::write_all(&mut pipe, stdin.as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("run the {name} hook: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let said = [output.stderr, output.stdout]
        .iter()
        .map(|bytes| {
            String::from_utf8_lossy(bytes)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .find(|text| !text.is_empty());
    Err(said.unwrap_or_else(|| format!("the {name} hook exited with {}", output.status)))
}
