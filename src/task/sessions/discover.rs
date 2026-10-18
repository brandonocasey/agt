//! Finding the transcript files of Codex and Claude profiles under the home directory.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;

use serde::Serialize;

use crate::core::error::Result;
use crate::core::local::{error_code, home};
use crate::core::transcripts::{Step, Walker, all_roots, belongs_to, history_files};

#[derive(Clone, Serialize)]
pub struct Unreadable {
    pub source: String,
    pub error: String,
}

/// What a run looked at and what it could not, printed so a caller can judge a missing result.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Coverage {
    pub roots: Vec<String>,
    pub discovered: usize,
    pub indexed: usize,
    pub malformed_lines: u64,
    pub oversized_lines: u64,
    pub unreadable: Vec<Unreadable>,
    pub removed_files: usize,
    pub skipped_files: usize,
    pub scan_limit: Option<u64>,
    pub scan_limit_reached: bool,
    pub unscanned_directories: usize,
    pub byte_limit: Option<u64>,
    pub byte_limit_reached: bool,
    pub scanned_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_turns: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub human_turns: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inherited_turns: Option<usize>,
}

pub struct Discovered {
    pub files: Vec<String>,
    pub coverage: Coverage,
}

/// The items in first-seen order without repeats.
pub fn unique(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect()
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Lists transcript files, stopping after `maximum` unless `session` asks for an exact match,
/// which is found even beyond the limit.
pub fn discover(maximum: u64, session: Option<&str>) -> Result<Discovered> {
    let home = home()?;
    let roots = all_roots(&home, true);
    let mut files: Vec<String> = history_files(&home)
        .iter()
        .map(|file| lossy(file))
        .collect();
    let mut preferred: Vec<String> = Vec::new();
    let mut coverage = Coverage {
        roots: roots.iter().map(|root| lossy(root)).collect(),
        discovered: 0,
        indexed: 0,
        malformed_lines: 0,
        oversized_lines: 0,
        unreadable: Vec::new(),
        removed_files: 0,
        skipped_files: 0,
        scan_limit: (maximum != u64::MAX).then_some(maximum),
        scan_limit_reached: false,
        unscanned_directories: 0,
        byte_limit: None,
        byte_limit_reached: false,
        scanned_bytes: 0,
        indexed_turns: None,
        human_turns: None,
        inherited_turns: None,
    };
    let count = |files: &[String], preferred: &[String]| (files.len() + preferred.len()) as u64;
    let mut walker = Walker::new(roots);
    while session.is_some() || count(&files, &preferred) < maximum {
        let Some(step) = walker.next() else {
            break;
        };
        match step {
            Step::Unreadable(directory, error) => {
                if error.kind() != ErrorKind::NotFound {
                    coverage.unreadable.push(Unreadable {
                        source: lossy(&directory),
                        error: error_code(&error),
                    });
                }
            }
            Step::File(path) if session.is_some_and(|id| belongs_to(&path, id)) => {
                preferred.push(lossy(&path));
            }
            Step::File(path) => {
                if count(&files, &preferred) < maximum {
                    files.push(lossy(&path));
                }
            }
        }
    }
    coverage.discovered = files.len() + preferred.len();
    coverage.scan_limit_reached = maximum != u64::MAX && count(&files, &preferred) >= maximum;
    coverage.unscanned_directories = walker.unscanned();
    Ok(Discovered {
        files: unique(preferred.into_iter().chain(files)),
        coverage,
    })
}
