//! The transcripts a report reads: the roots or files its scope names, narrowed to its dates,
//! project and harness.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::core::transcripts::{
    Kind, Step, Transcript, Walker, claude_projects, codex_roots, is_scratch,
};
use crate::sessions::{Harness, Scope};

fn modified(path: &Path) -> Option<f64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(match modified.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_secs_f64(),
        Err(before) => -before.duration().as_secs_f64(),
    })
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn wanted(scope: &Scope, kind: Kind) -> bool {
    matches!(
        (scope.harness, kind),
        (Harness::All, _) | (Harness::Claude, Kind::Claude) | (Harness::Codex, Kind::Codex)
    )
}

/// `~` and `~/...` as the home directory.
fn expand(text: &str, home: &Path) -> PathBuf {
    match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            PathBuf::from(format!("{}{rest}", home.display()))
        }
        _ => PathBuf::from(text),
    }
}

/// The transcripts in scope. Prints how many scratch transcripts it skipped.
pub fn transcripts(scope: &Scope) -> Vec<Transcript> {
    if !scope.files.is_empty() {
        return scope
            .files
            .iter()
            .map(|file| Transcript::of_file(PathBuf::from(file)))
            .filter(|transcript| wanted(scope, transcript.kind))
            .collect();
    }
    let home = home();
    // A root named on the command line can hold either harness; each file says which.
    let roots: Vec<(PathBuf, Option<Kind>)> = if scope.roots.is_empty() {
        claude_projects(&home)
            .into_iter()
            .map(|root| (root, Some(Kind::Claude)))
            .chain(
                codex_roots(&home)
                    .into_iter()
                    .map(|root| (root, Some(Kind::Codex))),
            )
            .collect()
    } else {
        scope
            .roots
            .iter()
            .map(|root| (expand(root, &home), None))
            .collect()
    };
    let mut found = Vec::new();
    let mut skipped = 0;
    for (root, hint) in roots {
        if hint.is_some_and(|kind| !wanted(scope, kind)) {
            continue;
        }
        for step in Walker::new(vec![root.clone()]) {
            let Step::File(path) = step else {
                continue;
            };
            if modified(&path).is_none_or(|time| time < scope.cutoff) {
                continue;
            }
            let parts: Vec<String> = path
                .strip_prefix(&root)
                .iter()
                .flat_map(|relative| relative.components())
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            let transcript = Transcript::locate(path, &parts, hint);
            let outside = scope
                .project
                .as_ref()
                .is_some_and(|text| !transcript.project.contains(text.as_str()));
            if !wanted(scope, transcript.kind) || outside {
                continue;
            }
            if !scope.scratch && is_scratch(&transcript.project) {
                skipped += 1;
                continue;
            }
            found.push(transcript);
        }
    }
    if skipped > 0 {
        eprintln!("(skipped {skipped} scratch transcripts; --scratch includes them)");
    }
    found
}
