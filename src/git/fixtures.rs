//! Repositories for the unit tests of the git modules, built with the git command line.
//! The tests can then ask git what it makes of them.

use std::fs;
use std::path::Path;

use crate::core::testing::Sandbox;

/// Writes `files` (path and content) in `dir`, and commits them with `message`.
pub(super) fn commit(sandbox: &Sandbox, dir: &Path, files: &[(&str, &str)], message: &str) {
    for (path, content) in files {
        let target = dir.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, content).unwrap();
    }
    sandbox.git(dir, &["add", "-A"]);
    sandbox.git(dir, &["commit", "-q", "-m", message]);
}

/// The commit `rev` names in `dir`.
pub(super) fn rev(sandbox: &Sandbox, dir: &Path, rev: &str) -> String {
    sandbox
        .git_output(dir, &["rev-parse", rev])
        .trim()
        .to_owned()
}
