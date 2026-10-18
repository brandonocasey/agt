use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use assert_fs::TempDir;
use serde_json::Value;

pub mod forge;
pub mod issue;
pub mod scenario;

const BIN: &str = env!("CARGO_BIN_EXE_agt");
pub const FAKE_FORGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixtures/fake-forge.sh");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/test/fixtures");

/// A temporary HOME, XDG dirs, git config and repository; nothing reads the user's real state.
pub struct Env {
    _temp: TempDir,
    root: PathBuf,
    pub repo: PathBuf,
    offline: bool,
}

impl Env {
    pub fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for dir in ["home", "state", "config", "repo", "forge"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(
            root.join("gitconfig"),
            format!(
                "[user]\n\tname = Tester\n\temail = tester@example.com\n[init]\n\tdefaultBranch = main\n[core]\n\texcludesFile = {}\n",
                root.join("gitignore").display()
            ),
        )
        .unwrap();
        fs::write(root.join("gitignore"), "/.agt/\n").unwrap();
        let env = Self {
            repo: root.join("repo"),
            root,
            _temp: temp,
            offline: false,
        };
        env.git(&["init", "-q", "-b", "main"]);
        env
    }

    /// Every command of this environment runs with `AGT_OFFLINE=1`, so reads stay local.
    pub fn offline(mut self) -> Self {
        self.offline = true;
        self
    }

    pub fn state(&self) -> PathBuf {
        self.repo.join(".agt").join("reviews")
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// `agt review` in `dir`, in the isolated environment.
    pub fn agt(&self, dir: &Path) -> Command {
        let mut command = self.command(BIN, dir);
        command.arg("review");
        command
    }

    pub fn command(&self, program: &str, dir: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(dir)
            .env("HOME", self.root.join("home"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("GIT_CONFIG_GLOBAL", self.root.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_EDITOR", "true")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env("AGT_GH_BIN", FAKE_FORGE)
            .env("AGT_GLAB_BIN", FAKE_FORGE)
            .env("FAKE_FORGE_DIR", self.root.join("forge"))
            .env_remove("GITLAB_HOST")
            .env_remove("AGT_NOW")
            .env_remove("AGT_OFFLINE")
            .env_remove("AGT_SYNC_CAP_MS")
            // `agt cleanup` must see only this environment: its cache, its tmp, no processes.
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("AGT_CLEANUP_TMP", self.root.join("tmp"))
            .env("AGT_CLEANUP_PID", "0")
            .env_remove("CLAUDE_CONFIG_DIR")
            // A test run inside a Claude Code session would make every process it spawns
            // that session's own, which `agt worktree` ignores.
            .env_remove("CLAUDE_PID")
            .env_remove("PLAYWRIGHT_BROWSERS_PATH");
        if self.offline {
            command.env("AGT_OFFLINE", "1");
        }
        command
    }

    /// Registers a canned fake gh/glab response for exactly these arguments, with a GraphQL
    /// query given as `query=<operation name>`.
    pub fn respond(&self, args: &[&str], body: &str) {
        fs::write(self.forge_file(args, "json"), body).unwrap();
    }

    /// Like [`Env::respond`], for glab only (a call with `GITLAB_HOST` set), such as an
    /// `auth status` that gh must not see.
    pub fn respond_glab(&self, args: &[&str], body: &str) {
        fs::write(self.forge_file(args, "glab.json"), body).unwrap();
    }

    /// Like [`Env::respond`], for the `call`-th call with these arguments only (from 1, counted
    /// since the last [`Env::reset_calls`]).
    pub fn respond_nth(&self, args: &[&str], call: usize, body: &str) {
        fs::write(self.forge_file(args, &format!("{call}.json")), body).unwrap();
    }

    /// Makes calls with these arguments fail after their input is read, as when a write
    /// reaches the host but its response is lost.
    pub fn fail_call(&self, args: &[&str]) {
        fs::write(self.forge_file(args, "fail"), "").unwrap();
    }

    /// Like [`Env::fail_call`], with the error text the tool prints, such as one naming an
    /// HTTP status.
    pub fn fail_call_with(&self, args: &[&str], stderr: &str) {
        fs::write(self.forge_file(args, "fail"), stderr).unwrap();
    }

    /// Undoes [`Env::fail_call`].
    pub fn pass_call(&self, args: &[&str]) {
        fs::remove_file(self.forge_file(args, "fail")).unwrap();
    }

    /// Restarts the per-arguments call counts that [`Env::respond_nth`] uses.
    pub fn reset_calls(&self) {
        for entry in fs::read_dir(self.root.join("forge")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "count") {
                fs::remove_file(path).unwrap();
            }
        }
    }

    fn forge_file(&self, args: &[&str], suffix: &str) -> PathBuf {
        let key = self.git_stdin(
            &["hash-object", "--stdin"],
            &format!("{}\n", args.join("\n")),
        );
        self.root.join("forge").join(format!("{key}.{suffix}"))
    }

    /// Every fake gh/glab call that sent a body, as `(<arguments>, <body>)`.
    pub fn forge_inputs(&self) -> Vec<(String, String)> {
        self.forge_calls()
            .iter()
            .enumerate()
            .filter_map(|(index, call)| {
                let input = fs::read_to_string(
                    self.root.join("forge").join(format!("input-{}", index + 1)),
                )
                .ok()?;
                Some((
                    call.split('\t').nth(1).unwrap_or_default().to_owned(),
                    input,
                ))
            })
            .collect()
    }

    /// Every fake gh/glab call as `<GITLAB_HOST>\t<arguments>`.
    pub fn forge_calls(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("forge").join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// A fixture of `test/fixtures` (`github/threads-1.json`) with `{{NAME}}` placeholders replaced.
pub fn fixture(name: &str, vars: &[(&str, &str)]) -> String {
    let mut text = fs::read_to_string(Path::new(FIXTURES).join(name)).unwrap();
    for (name, value) in vars {
        text = text.replace(&format!("{{{{{name}}}}}"), value);
    }
    assert!(!text.contains("{{"), "unfilled placeholder in {name}");
    text
}

impl Env {
    pub fn git(&self, args: &[&str]) -> String {
        self.git_env(args, &[])
    }

    pub fn git_env(&self, args: &[&str], env: &[(&str, &str)]) -> String {
        let output = self
            .command("git", &self.repo)
            .args(args)
            .envs(env.iter().copied())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    pub fn git_stdin(&self, args: &[&str], input: &str) -> String {
        let mut child = self
            .command("git", &self.repo)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    pub fn git_status(&self, args: &[&str]) -> bool {
        self.command("git", &self.repo)
            .args(args)
            .output()
            .unwrap()
            .status
            .success()
    }

    pub fn write(&self, path: &str, content: &str) {
        let file = self.repo.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, content).unwrap();
    }

    pub fn commit_all(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }

    pub fn exec(&self, args: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> Output {
        let mut command = self.agt(&self.repo);
        command
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(stdin.unwrap_or("").as_bytes()).unwrap();
        drop(input);
        child.wait_with_output().unwrap()
    }

    pub fn run_with(&self, args: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> Value {
        let output = self.exec(args, stdin, env);
        assert!(
            output.status.success(),
            "agt review {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    pub fn run(&self, args: &[&str]) -> Value {
        self.run_with(args, None, &[])
    }

    /// Runs agt review from another directory, such as a linked worktree.
    pub fn run_in(&self, dir: &Path, args: &[&str]) -> Value {
        let output = self.agt(dir).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "agt review {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    pub fn run_stdin(&self, args: &[&str], stdin: &str) -> Value {
        self.run_with(args, Some(stdin), &[])
    }

    pub fn text(&self, args: &[&str]) -> String {
        let output = self.exec(args, None, &[]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    /// Runs a command that must fail and returns its stderr.
    pub fn fail(&self, args: &[&str], stdin: Option<&str>) -> String {
        let output = self.exec(args, stdin, &[]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "agt review {args:?} should fail"
        );
        assert!(output.stdout.is_empty());
        String::from_utf8(output.stderr).unwrap()
    }

    /// Starts a run with one reviewer, runs one full pass and returns the run id.
    pub fn review(&self, kind: &str) -> String {
        let run = self.run(&["run", "start"])["run"]
            .as_str()
            .unwrap()
            .to_owned();
        self.reviewed_pass(&run, kind);
        self.run(&["run", "end", "--run", &run]);
        run
    }

    pub fn reviewed_pass(&self, run: &str, kind: &str) -> String {
        let reviewers = r#"[{"model":"claude-opus-5-5","effort":"xhigh","harness":"claude-code","harness_version":"2.1.296"}]"#;
        let pass = self.run_stdin(
            &[
                "pass",
                "start",
                "--run",
                run,
                "--kind",
                kind,
                "--reviewers",
                "-",
            ],
            reviewers,
        );
        let id = pass["pass"]["id"].as_str().unwrap().to_owned();
        self.run(&["pass", "end", "--run", run, "--pass", &id]);
        id
    }

    /// Base repo: `a.txt` with ten numbered lines on `main`, then a `feat` branch.
    pub fn with_branch() -> Self {
        let env = Self::new();
        env.write("a.txt", &numbered(10, &[]));
        env.commit_all("base");
        env.git(&["switch", "-q", "-c", "feat"]);
        env
    }
}

/// Lines `1..=count`, with `(line, text)` replacements.
pub fn numbered(count: usize, changes: &[(usize, &str)]) -> String {
    (1..=count)
        .map(|line| {
            let text = changes
                .iter()
                .find(|(at, _)| *at == line)
                .map_or_else(|| line.to_string(), |(_, text)| (*text).to_owned());
            format!("{text}\n")
        })
        .collect()
}

/// Every file under `dir` with its contents and modification time.
pub fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                pending.push(entry.path());
                found.push((entry.path(), Vec::new(), meta.modified().unwrap()));
            } else {
                found.push((
                    entry.path(),
                    fs::read(entry.path()).unwrap(),
                    meta.modified().unwrap(),
                ));
            }
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}
