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
            .current_dir(self.repo());
        command
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
