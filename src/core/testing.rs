//! An isolated home, state, config and cache directory plus an empty repository for unit tests
//! that run subprocesses, with a git identity and a global git config of their own.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    pub(crate) fn new() -> Self {
        hide_global_git_config();
        let root = tempfile::tempdir().unwrap();
        for dir in ["home", "state", "config", "cache", "repo"] {
            fs::create_dir(root.path().join(dir)).unwrap();
        }
        fs::write(
            root.path().join("gitconfig"),
            "[user]\n\tname = Tester\n\temail = tester@example.com\n[init]\n\tdefaultBranch = main\n",
        )
        .unwrap();
        Self { root }
    }

    /// A sandbox whose repository is already `git init`ed.
    pub(crate) fn with_repo() -> Self {
        let sandbox = Self::new();
        sandbox.git(&sandbox.repo(), &["init", "-q"]);
        sandbox
    }

    pub(crate) fn path(&self) -> &Path {
        self.root.path()
    }

    pub(crate) fn repo(&self) -> PathBuf {
        self.root.path().join("repo")
    }

    /// `program` in the repository with only this sandbox's environment, and no forge.
    pub(crate) fn command(&self, program: &str) -> Command {
        let root = self.root.path();
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", root.join("home"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("GIT_CONFIG_GLOBAL", root.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("AGT_GH_BIN", root.join("no-forge"))
            .env("AGT_GLAB_BIN", root.join("no-forge"))
            .env("AGT_TEA_BIN", root.join("no-forge"))
            .current_dir(self.repo());
        command
    }

    /// What `git args` prints on stdout in `dir`, for tests that compare the library with the
    /// command line.
    pub(crate) fn git_output(&self, dir: &Path, args: &[&str]) -> String {
        let output = self
            .command("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    /// What the shell `script` prints on stdout in `dir`.
    pub(crate) fn shell_output(&self, dir: &Path, script: &str) -> String {
        let output = self
            .command("sh")
            .current_dir(dir)
            .args(["-c", script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{script}");
        String::from_utf8(output.stdout).unwrap()
    }

    pub(crate) fn git(&self, dir: &Path, args: &[&str]) {
        let status = self
            .command("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
}

/// Points libgit2's global, XDG and system config search at an empty directory, once per test
/// process, so a developer's own git config never reaches a unit test.
fn hide_global_git_config() {
    static HIDDEN: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    HIDDEN.get_or_init(|| {
        let empty = tempfile::tempdir().unwrap();
        for level in [
            git2::ConfigLevel::System,
            git2::ConfigLevel::XDG,
            git2::ConfigLevel::Global,
            git2::ConfigLevel::ProgramData,
        ] {
            // SAFETY: runs once, before any test of this process opens a repository.
            unsafe { git2::opts::set_search_path(level, empty.path()) }.unwrap();
        }
        empty
    });
}
