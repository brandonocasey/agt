//! The root `--offline` flag against commands that would reach a remote: a `git` that records
//! `fetch`, `ls-remote` and `push` and refuses them, and the fake `gh` and `glab` call log, show
//! that no call happens.

use std::fs;
use std::process::Output;

use crate::support::{Env, executable, path_with};

struct Site {
    env: Env,
    path: String,
}

impl Site {
    /// A repository with `main` pushed to a bare origin that reads as a GitHub URL, and a `git`
    /// in front of the real one that records and refuses network commands.
    fn new() -> Self {
        let env = Env::new();
        let bare = env.path("origin.git");
        let url = "git@github.com:acme/app.git";
        env.git(&["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()]);
        env.write("base.txt", "base\n");
        env.commit_all("base");
        env.git(&["remote", "add", "origin", url]);
        env.git(&["config", &format!("url.{}.insteadOf", bare.display()), url]);
        env.git(&["push", "-q", "origin", "main"]);
        env.git(&["remote", "set-head", "origin", "main"]);
        let log = env.path("network.log");
        let path = env.git_wrapper(
            "wrapper",
            &format!(
                "for word in \"$@\"; do case \"$word\" in fetch|ls-remote|push|pull|clone) echo \"git $*\" >> '{}'; exit 97;; esac; done\n",
                log.display()
            ),
        );
        Self { env, path }
    }

    fn agt(&self, args: &[&str]) -> Output {
        self.agt_in(&self.env.repo, args)
    }

    fn agt_in(&self, dir: &std::path::Path, args: &[&str]) -> Output {
        self.env
            .bin(dir)
            .arg("--offline")
            .args(args)
            .env("PATH", &self.path)
            .output()
            .unwrap()
    }

    /// Every recorded network call of git, gh or glab.
    fn network(&self) -> Vec<String> {
        let mut calls: Vec<String> = fs::read_to_string(self.env.path("network.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        // `doctor` asks each tool for its version, which reaches no host.
        calls.extend(
            self.env
                .forge_calls()
                .into_iter()
                .filter(|call| call.trim() != "--version"),
        );
        calls
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.agt(args);
        assert!(
            output.status.success(),
            "agt --offline {args:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(self.network(), Vec::<String>::new(), "{args:?}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn refused(&self, args: &[&str], reason: &str) {
        let output = self.agt(args);
        let error = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {error}");
        assert!(error.contains(reason), "{args:?}: {error}");
        assert_eq!(self.network(), Vec::<String>::new(), "{args:?}");
    }
}

#[test]
fn the_network_log_sees_a_fetch_without_the_flag() {
    let site = Site::new();
    site.env.git(&["checkout", "-q", "-b", "topic"]);
    let output = site
        .env
        .bin(&site.env.repo)
        .args(["branch", "check", "--checks", "landable"])
        .env("PATH", &site.path)
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(0));
    assert!(!site.network().is_empty());
}

#[test]
fn branch_check_landable_uses_the_local_default_branch() {
    let site = Site::new();
    site.env.git(&["checkout", "-q", "-b", "topic"]);
    let out = site.ok(&["branch", "check", "--checks", "landable"]);
    assert!(out.contains("\"target\":\"main\""), "{out}");
}

#[test]
fn branch_check_pushed_needs_the_remote_and_refuses() {
    let site = Site::new();
    site.env.git(&["checkout", "-q", "-b", "topic"]);
    let reason = "offline mode forbids agt branch check --checks pushed";
    site.refused(&["branch", "check", "--checks", "pushed"], reason);
    site.refused(&["branch", "check"], reason);
}

#[test]
fn worktree_create_with_a_base_asks_no_remote() {
    let site = Site::new();
    let out = site.ok(&["worktree", "create", "feature", "--base", "main"]);
    assert!(out.contains("\"branch\":\"feature\""), "{out}");
}

#[test]
fn worktree_create_cannot_tell_whether_origin_has_the_branch() {
    let site = Site::new();
    site.refused(
        &["worktree", "create", "feature"],
        "offline mode forbids asking origin whether feature exists",
    );
}

#[test]
fn worktree_create_of_a_known_remote_branch_asks_no_remote() {
    let site = Site::new();
    site.env.git(&["branch", "other"]);
    site.env.git(&["push", "-q", "origin", "other"]);
    site.env.git(&["branch", "-D", "other"]);
    site.env.git(&["fetch", "-q", "origin"]);
    let out = site.ok(&["worktree", "create", "other"]);
    assert!(out.contains("\"tracking\":\"origin/other\""), "{out}");
}

#[test]
fn worktree_prepare_installs_are_refused() {
    let site = Site::new();
    let bin = site.env.path("tools");
    fs::create_dir_all(&bin).unwrap();
    let log = site.env.path("npm.log");
    executable(
        &bin.join("npm"),
        format!("#!/bin/sh\necho \"$@\" >> '{}'\n", log.display()),
    );
    site.env.write("package-lock.json", "{}\n");
    let path = format!("{}:{}", bin.display(), site.path);
    let output = site
        .env
        .bin(&site.env.repo)
        .args(["--offline", "worktree", "prepare"])
        .env(
            "PATH",
            path_with(&bin).replace(&bin.display().to_string(), &path),
        )
        .output()
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{error}");
    assert!(
        error.contains("offline mode forbids agt worktree prepare installs"),
        "{error}"
    );
    assert!(!log.exists(), "npm ran");
}

#[test]
fn forge_commands_refuse_without_calling_the_forge() {
    let site = Site::new();
    site.env.git(&["checkout", "-q", "-b", "topic"]);
    for (args, what) in [
        (vec!["pr", "list"], "agt pr"),
        (vec!["review", "sync"], "review sync and publish"),
        (
            vec!["review", "publish", "--dry-run"],
            "review sync and publish",
        ),
        (vec!["issue", "sync"], "issue sync and push"),
        (vec!["issue", "push"], "issue sync and push"),
    ] {
        site.refused(&args, &format!("offline mode forbids {what}"));
    }
}

#[test]
fn local_commands_make_no_network_call() {
    let site = Site::new();
    site.env.git(&["checkout", "-q", "-b", "topic"]);
    let doctor = site.agt(&["doctor", "--json"]);
    assert!(doctor.status.code().is_some_and(|code| code < 3));
    assert_eq!(site.network(), Vec::<String>::new());
    site.ok(&["issue", "list"]);
    site.ok(&["review", "list"]);
    site.ok(&["cleanup", "--only", "worktrees"]);
    site.ok(&["worktree", "list"]);
}
