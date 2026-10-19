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

/// Adapted records shared by every selected report in one invocation.
pub struct Loaded {
    transcript: Transcript,
    records: Vec<serde_json::Value>,
}

impl std::ops::Deref for Loaded {
    type Target = Transcript;

    fn deref(&self) -> &Transcript {
        &self.transcript
    }
}

impl Loaded {
    pub fn records(&self, each: impl FnMut(&serde_json::Value)) {
        self.records.iter().for_each(each);
    }

    pub fn api_calls(
        &self,
        seen: &mut std::collections::HashSet<String>,
    ) -> Vec<crate::core::transcripts::Call> {
        Transcript::calls_from_records(&self.records, seen)
    }
}

pub fn load(scope: &Scope) -> impl Iterator<Item = Loaded> + '_ {
    transcripts(scope).into_iter().map(|transcript| {
        let mut records = Vec::new();
        transcript.records(&scope.since_iso, |record| records.push(record.clone()));
        Loaded {
            transcript,
            records,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_reuse_records_after_the_source_disappears() {
        let dir = tempfile::tempdir().expect("temporary transcript root");
        let path = dir.path().join("session.jsonl");
        fs::write(&path, concat!(
            r#"{"type":"assistant","timestamp":"2026-03-01","message":{"id":"response","model":"test","usage":{"input_tokens":10},"content":[]}}"#,
            "\n",
        )).expect("write transcript");
        let scope = Scope {
            since: "2026-01-01".into(),
            since_iso: "2026-01-01".into(),
            cutoff: 0.0,
            project: None,
            scratch: false,
            roots: Vec::new(),
            files: vec![path.to_string_lossy().into_owned()],
            top: 25,
            harness: Harness::All,
        };
        let found: Vec<_> = load(&scope).collect();
        fs::remove_file(path).expect("remove original transcript");
        for _ in 0..2 {
            let mut count = 0;
            found[0].records(|_| count += 1);
            assert_eq!(count, 1);
            let mut seen = std::collections::HashSet::new();
            let calls = found[0].api_calls(&mut seen);
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].usage["input_tokens"], 10);
            assert!(found[0].api_calls(&mut seen).is_empty());
        }
    }
}
