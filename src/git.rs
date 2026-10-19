use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;

pub mod base;

use crate::core::error::{Error, Result, ctx, fail};

// Fixed diff options so patch-ids and anchors do not depend on user diff config.
const DIFF_FLAGS: [&str; 5] = [
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

pub struct CommitInfo {
    pub sha: String,
    pub tree: String,
    pub subject: String,
    pub patch_id: Option<String>,
}

pub struct LogEntry {
    pub sha: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub subject: String,
}

pub struct Repo {
    root: PathBuf,
    main_checkout: OnceLock<PathBuf>,
    env: Vec<(OsString, OsString)>,
    _scratch: Option<tempfile::TempDir>,
}

impl Repo {
    /// The repository whose work tree is `root`, which MUST be a directory git accepts; it need
    /// not be the current directory.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            main_checkout: OnceLock::new(),
            env: Vec::new(),
            _scratch: None,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn discover() -> Result<Self> {
        let output = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .stdin(Stdio::null())
            .output()
            .map_err(ctx("run git"))?;
        if !output.status.success() {
            return fail("not inside a git work tree");
        }
        let root = text(output.stdout)?;
        Ok(Self {
            root: PathBuf::from(root),
            main_checkout: OnceLock::new(),
            env: Vec::new(),
            _scratch: None,
        })
    }

    /// Sends object writes to a throwaway directory while existing objects stay readable, so a
    /// read-only command never modifies the repository.
    pub fn read_only(mut self) -> Result<Self> {
        let objects = self.git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ])?;
        let scratch = tempfile::tempdir().map_err(ctx("create temporary object directory"))?;
        self.env.push((
            "GIT_OBJECT_DIRECTORY".into(),
            scratch.path().as_os_str().into(),
        ));
        self.env
            .push(("GIT_ALTERNATE_OBJECT_DIRECTORIES".into(), objects.into()));
        self.env.push(("GIT_OPTIONAL_LOCKS".into(), "0".into()));
        self._scratch = Some(scratch);
        Ok(self)
    }

    fn exec(&self, args: &[&str], input: Option<Vec<u8>>, env: &[(&str, &Path)]) -> Result<Output> {
        let mut command = Command::new("git");
        command
            .current_dir(&self.root)
            // `git status` and friends would otherwise refresh and rewrite the index of a
            // worktree that is only being read.
            .args(["--no-optional-locks", "-c", "core.quotepath=false"])
            .args(args)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .envs(env.iter().copied())
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(ctx("run git"))?;
        // Input is written from another thread so a large output cannot fill the pipe and deadlock.
        let writer = match (input, child.stdin.take()) {
            (Some(data), Some(mut stdin)) => {
                Some(std::thread::spawn(move || stdin.write_all(&data)))
            }
            _ => None,
        };
        let output = child
            .wait_with_output()
            .map_err(ctx(format!("run git {}", args.join(" "))))?;
        if let Some(writer) = writer {
            match writer.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) if error.kind() == ErrorKind::BrokenPipe => {}
                Ok(Err(error)) => {
                    return fail(format!("write to git {}: {error}", args.join(" ")));
                }
                Err(_) => return fail("git input writer panicked"),
            }
        }
        Ok(output)
    }

    pub fn checked(
        &self,
        args: &[&str],
        input: Option<Vec<u8>>,
        env: &[(&str, &Path)],
    ) -> Result<Vec<u8>> {
        let output = self.exec(args, input, env)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return fail(format!("git {} failed: {}", args.join(" "), stderr.trim()));
        }
        Ok(output.stdout)
    }

    /// Runs `git args` and returns its output, giving up after `seconds`. For commands that
    /// talk to a remote, which must not hang or ask anything: no credential prompt of any kind
    /// (git, ssh, askpass helpers, credential managers), ssh in batch mode with a connect
    /// timeout, and git in its own process group so that a timeout or a signal to agt kills ssh with it.
    pub fn git_within(&self, args: &[&str], seconds: u64) -> Result<Vec<u8>> {
        let scratch = tempfile::tempdir().map_err(ctx("create temporary output directory"))?;
        let path = scratch.path().join("out");
        let out = fs::File::create(&path).map_err(ctx("create temporary output"))?;
        let ssh = self.batch_ssh_command(args);
        let mut child = Command::new("git")
            .current_dir(&self.root)
            .args(["--no-optional-locks"])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "false")
            .env("SSH_ASKPASS", "false")
            .env("SSH_ASKPASS_REQUIRE", "never")
            .env("GCM_INTERACTIVE", "never")
            .env("GIT_SSH_COMMAND", ssh)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(Stdio::null())
            .spawn()
            .map_err(ctx("run git"))?;
        // Held until return: a signal to agt kills the group too, not just git.
        let _watch = crate::core::group::Watch::new(child.id());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
        loop {
            match child.try_wait().map_err(ctx("wait for git"))? {
                Some(status) if status.success() => {
                    return fs::read(&path).map_err(ctx("read git output"));
                }
                Some(_) => return fail(format!("git {} failed", args.join(" "))),
                None if std::time::Instant::now() >= deadline => {
                    // The group, not just git: ssh would otherwise outlive agt.
                    if let Ok(pid) = i32::try_from(child.id()) {
                        // SAFETY: kill(2) with a negative pid signals only that process group.
                        unsafe { libc::kill(-pid, libc::SIGKILL) };
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    return fail(format!(
                        "git {} took longer than {seconds}s",
                        args.join(" ")
                    ));
                }
                None => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
    }

    /// The ssh command a remote check uses: the user's own (`GIT_SSH_COMMAND`, `core.sshCommand`,
    /// `GIT_SSH`, in git's order of precedence) with batch mode and a connect timeout added, or
    /// plain ssh with them. `GIT_SSH` is a program path git runs without a shell, so it is quoted.
    /// `core.sshCommand` is read with the `--git-dir` and `--work-tree` of `args`, as the command
    /// itself resolves the repository: a stray `core.worktree` would otherwise fail the read.
    fn batch_ssh_command(&self, args: &[&str]) -> String {
        let mut config: Vec<&str> = Vec::new();
        let mut rest = args.iter();
        while let Some(&flag) = rest.next() {
            match (flag, rest.clone().next()) {
                ("--git-dir" | "--work-tree", Some(&value)) => {
                    config.extend([flag, value]);
                    rest.next();
                }
                _ => break,
            }
        }
        config.extend(["config", "core.sshCommand"]);
        let nonempty = |value: Option<String>| value.filter(|command| !command.is_empty());
        let own = nonempty(std::env::var("GIT_SSH_COMMAND").ok())
            .or_else(|| nonempty(self.git_opt(&config).ok().flatten()))
            .or_else(|| {
                nonempty(std::env::var("GIT_SSH").ok())
                    .map(|program| format!("'{}'", program.replace('\'', "'\\''")))
            });
        format!(
            "{} -o BatchMode=yes -o ConnectTimeout=5",
            own.as_deref().unwrap_or("ssh")
        )
    }

    pub fn git(&self, args: &[&str]) -> Result<String> {
        text(self.checked(args, None, &[])?)
    }

    pub fn git_opt(&self, args: &[&str]) -> Result<Option<String>> {
        let output = self.exec(args, None, &[])?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(Some(text(output.stdout)?).filter(|value| !value.is_empty()))
    }

    /// The exit code of `git <args>` (`None` when a signal ended it) and its stderr, for the
    /// commands whose codes mean more than failure.
    pub fn probe(&self, args: &[&str]) -> Result<(Option<i32>, String)> {
        let output = self.exec(args, None, &[])?;
        Ok((
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }

    pub fn succeeds(&self, args: &[&str]) -> Result<bool> {
        Ok(self.exec(args, None, &[])?.status.success())
    }

    pub fn head(&self) -> Result<String> {
        self.git_opt(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])?
            .ok_or_else(|| Error("repository has no commits".into()))
    }

    pub fn commit(&self, rev: &str) -> Result<String> {
        let spec = format!("{rev}^{{commit}}");
        self.git_opt(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ])?
        .ok_or_else(|| Error(format!("unknown commit: {rev}")))
    }

    /// The first line of the message of `commit`.
    pub fn subject(&self, commit: &str) -> Result<String> {
        let spec = format!("{commit}^{{commit}}");
        self.git(&["log", "-1", "--format=%s", "--end-of-options", &spec, "--"])
            .map(|text| text.trim_end().to_owned())
    }

    pub fn parent(&self, commit: &str) -> Result<Option<String>> {
        let spec = format!("{commit}^1");
        self.git_opt(&[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ])
    }

    pub fn tree_of(&self, commit: &str) -> Result<String> {
        let spec = format!("{commit}^{{tree}}");
        self.git(&["rev-parse", "--verify", "--end-of-options", &spec])
    }

    pub fn current_branch(&self) -> Result<Option<String>> {
        self.git_opt(&["symbolic-ref", "--quiet", "--short", "HEAD"])
    }

    /// `<unix time> <sha>` of the oldest reflog entry of a local branch, which stays the same
    /// across rebases but changes when the branch is deleted and created again. Reflogs of
    /// branches live in the common dir, so linked worktrees agree. `None` without a reflog.
    pub fn branch_birth(&self, branch: &str) -> Result<Option<String>> {
        let reference = format!("refs/heads/{branch}");
        let Some(log) = self.git_opt(&[
            "reflog",
            "show",
            "--date=unix",
            "--format=%H %gd",
            &reference,
            "--",
        ])?
        else {
            return Ok(None);
        };
        let Some((sha, selector)) = log.lines().last().and_then(|line| line.split_once(' ')) else {
            return Ok(None);
        };
        let time = selector
            .rsplit_once("@{")
            .and_then(|(_, rest)| rest.strip_suffix('}'))
            .filter(|time| !time.is_empty() && time.bytes().all(|b| b.is_ascii_digit()));
        Ok(time.map(|time| format!("{time} {sha}")))
    }

    /// Returns the full ref of the default branch: `--default-branch`, then `origin/HEAD`, then a
    /// local `main`, `master` or `init.defaultBranch`.
    pub fn default_branch(&self, explicit: Option<&str>) -> Result<String> {
        if let Some(name) = explicit {
            let local = format!("refs/heads/{name}");
            if self.succeeds(&["show-ref", "--verify", "--quiet", &local])? {
                return Ok(local);
            }
            self.commit(name)?;
            return Ok(name.to_owned());
        }
        if let Some(remote) =
            self.git_opt(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])?
        {
            return Ok(remote);
        }
        let configured = self.git_opt(&["config", "init.defaultBranch"])?;
        for name in ["main", "master"].into_iter().chain(configured.as_deref()) {
            let local = format!("refs/heads/{name}");
            if self.succeeds(&["show-ref", "--verify", "--quiet", &local])? {
                return Ok(local);
            }
        }
        fail("cannot find the default branch; pass --default-branch <ref>")
    }

    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        self.git_opt(&["merge-base", "--end-of-options", a, b])
    }

    pub fn root_commit(&self, head: &str) -> Result<String> {
        let roots = self.git(&["rev-list", "--max-parents=0", "--end-of-options", head])?;
        let mut roots: Vec<&str> = roots.lines().collect();
        roots.sort_unstable();
        roots
            .first()
            .map(|root| (*root).to_owned())
            .ok_or_else(|| Error(format!("no root commit for {head}")))
    }

    /// Missing objects count as "not an ancestor", since stored commits may have been collected.
    pub fn is_ancestor(&self, ancestor: &str, commit: &str) -> Result<bool> {
        self.succeeds(&[
            "merge-base",
            "--is-ancestor",
            "--end-of-options",
            ancestor,
            commit,
        ])
    }

    /// Commits in `to` that `from` lacks (`git rev-list --count from..to`).
    pub fn distance(&self, from: &str, to: &str) -> Result<u64> {
        let range = format!("{from}..{to}");
        let count = self.git(&["rev-list", "--count", "--end-of-options", &range])?;
        count
            .parse()
            .map_err(|_| Error(format!("git rev-list --count printed {count}")))
    }

    pub fn object_exists(&self, spec: &str) -> Result<bool> {
        self.succeeds(&["cat-file", "-e", "--end-of-options", spec])
    }

    pub fn user_name(&self) -> Result<Option<String>> {
        self.git_opt(&["config", "user.name"])
    }

    /// A copy of the index in a scratch directory, to run index-changing commands against.
    fn scratch_index(&self) -> Result<(tempfile::TempDir, PathBuf)> {
        let index = self.git(&["rev-parse", "--path-format=absolute", "--git-path", "index"])?;
        let scratch = tempfile::tempdir().map_err(ctx("create temporary index directory"))?;
        let temp_index = scratch.path().join("index");
        // Keep the index's mtime on the copy: git trusts cached stat data only for entries
        // older than the index file, so a newer copy would hide files changed in the same
        // second as the last `git add` (racy git). macOS copies keep it; Linux copies do not.
        // Read the time before copying: if git replaces the index meanwhile, an older time
        // only makes git recheck more files.
        let modified = fs::metadata(&index).and_then(|meta| meta.modified());
        match fs::copy(&index, &temp_index) {
            Ok(_) => {
                let modified = modified.map_err(ctx(format!("read git index time {index}")))?;
                fs::File::options()
                    .write(true)
                    .open(&temp_index)
                    .and_then(|file| file.set_modified(modified))
                    .map_err(ctx("set temporary index time"))?;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return fail(format!("copy git index {index}: {error}")),
        }
        Ok((scratch, temp_index))
    }

    /// Tree id of the work tree as `git add -A` would stage it, built in a copy of the index so
    /// the real index is untouched and tracked ignored files stay in the tree.
    pub fn worktree_tree(&self) -> Result<String> {
        let (_scratch, temp_index) = self.scratch_index()?;
        let env = [("GIT_INDEX_FILE", temp_index.as_path())];
        self.checked(&["add", "-A"], None, &env)?;
        text(self.checked(&["write-tree"], None, &env)?)
    }

    /// Like [`Self::worktree_tree`], but a tracked file marked skip-worktree or assume-unchanged
    /// that exists on disk is taken as it is, not as the index has it. A file that is gone (a
    /// sparse checkout) keeps its entry, or the tree would record it as deleted.
    pub fn worktree_tree_with_hidden_edits(&self) -> Result<String> {
        let (_scratch, temp_index) = self.scratch_index()?;
        self.unhide(&temp_index)?;
        let env = [("GIT_INDEX_FILE", temp_index.as_path())];
        // `--sparse` (git 2.35) makes add take files outside the sparse-checkout patterns too.
        let sparse = self.exec(&["add", "--sparse", "-A"], None, &env)?;
        if !sparse.status.success() {
            let stderr = String::from_utf8_lossy(&sparse.stderr);
            if !(stderr.contains("unknown option") || stderr.contains("usage:")) {
                return fail(format!("git add --sparse -A failed: {}", stderr.trim()));
            }
            // An older git cannot add paths outside the patterns: take what it can, then
            // refuse if the tree is not the working tree, rather than lose an edit.
            self.checked(&["add", "-A"], None, &env)?;
            let tree = text(self.checked(&["write-tree"], None, &env)?)?;
            let lost = self.checked(&["diff-files", "--name-only", "-z"], None, &env)?;
            if let Some(name) = lost.split(|byte| *byte == 0).find(|name| !name.is_empty()) {
                return fail(format!(
                    "{} is outside the sparse-checkout patterns and this git ({}) cannot back it up; widen the patterns with `git sparse-checkout add` or upgrade to git 2.35",
                    String::from_utf8_lossy(name),
                    self.git(&["--version"])?
                ));
            }
            return Ok(tree);
        }
        text(self.checked(&["write-tree"], None, &env)?)
    }

    /// The tracked files marked skip-worktree or assume-unchanged that exist on disk and differ
    /// from the index: edits `git status` does not show.
    pub fn hidden_edits(&self) -> Result<Vec<String>> {
        let (_scratch, temp_index) = self.scratch_index()?;
        let flagged = self.unhide(&temp_index)?;
        if flagged.is_empty() {
            return Ok(Vec::new());
        }
        let env = [("GIT_INDEX_FILE", temp_index.as_path())];
        let changed = self.checked(&["diff", "--name-only", "-z", "--no-renames"], None, &env)?;
        Ok(changed
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
            .map(|name| String::from_utf8_lossy(name).into_owned())
            .filter(|name| flagged.contains(name))
            .collect())
    }

    /// Clears skip-worktree and assume-unchanged in the scratch index for each flagged file that
    /// exists on disk; returns those paths.
    fn unhide(&self, temp_index: &Path) -> Result<Vec<String>> {
        let env = [("GIT_INDEX_FILE", temp_index)];
        let listing = self.checked(&["ls-files", "-v", "-z"], None, &env)?;
        let mut flagged = Vec::new();
        let mut input = Vec::new();
        for entry in listing.split(|byte| *byte == 0).filter(|e| e.len() > 2) {
            let (tag, path) = (entry[0], &entry[2..]);
            if tag != b'S' && !tag.is_ascii_lowercase() {
                continue;
            }
            let name = String::from_utf8_lossy(path).into_owned();
            if fs::symlink_metadata(self.root.join(&name)).is_ok() {
                input.extend_from_slice(path);
                input.push(0);
                flagged.push(name);
            }
        }
        if !input.is_empty() {
            // One flag per run: with both, git clears only one of them.
            for flag in ["--no-skip-worktree", "--no-assume-unchanged"] {
                self.checked(
                    &["update-index", flag, "-z", "--stdin"],
                    Some(input.clone()),
                    &env,
                )?;
            }
        }
        Ok(flagged)
    }

    /// Non-merge commits in `from..to` (or all of `to` when `from` is `None`), oldest first.
    pub fn commits(&self, from: Option<&str>, to: &str) -> Result<Vec<CommitInfo>> {
        let range = match from {
            Some(from) => format!("{from}..{to}"),
            None => to.to_owned(),
        };
        let listing = self.git(&[
            "log",
            "--reverse",
            "--no-merges",
            "--format=%H%x00%T%x00%s",
            "--end-of-options",
            &range,
            "--",
        ])?;
        let mut args = vec![
            "-c",
            "log.showRoot=true",
            "-c",
            "log.showSignature=false",
            "log",
            "--reverse",
            "--no-merges",
            "-p",
            "--no-renames",
            "--format=commit %H",
        ];
        args.extend(DIFF_FLAGS);
        args.extend(["--end-of-options", &range, "--"]);
        let patch = self.checked(&args, None, &[])?;
        let ids: HashMap<String, String> = self
            .patch_ids(patch)?
            .into_iter()
            .map(|(patch_id, sha)| (sha, patch_id))
            .collect();
        listing
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                let mut parts = line.splitn(3, '\0');
                let (Some(sha), Some(tree), Some(subject)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return fail(format!("unexpected git log line: {line}"));
                };
                Ok(CommitInfo {
                    sha: sha.to_owned(),
                    tree: tree.to_owned(),
                    subject: subject.to_owned(),
                    patch_id: ids.get(sha).cloned(),
                })
            })
            .collect()
    }

    pub fn commit_patch_id(&self, commit: &str) -> Result<Option<String>> {
        let mut args = vec!["diff-tree", "-p", "--root", "--no-renames"];
        args.extend(DIFF_FLAGS);
        args.extend(["--end-of-options", commit, "--"]);
        let patch = self.checked(&args, None, &[])?;
        Ok(self.patch_ids(patch)?.into_iter().next().map(|(id, _)| id))
    }

    /// Patch-id of the whole change from `from` (empty tree when `None`) to a commit or tree.
    /// Every commit in `from..to`, merges included, oldest first.
    pub fn log(&self, from: Option<&str>, to: &str) -> Result<Vec<LogEntry>> {
        let range = match from {
            Some(from) => format!("{from}..{to}"),
            None => to.to_owned(),
        };
        let listing = self.git(&[
            "log",
            "--reverse",
            "--format=%H%x00%T%x00%P%x00%s",
            "--end-of-options",
            &range,
            "--",
        ])?;
        listing
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                let mut parts = line.splitn(4, '\0');
                let (Some(sha), Some(tree), Some(parents), Some(subject)) =
                    (parts.next(), parts.next(), parts.next(), parts.next())
                else {
                    return fail(format!("unexpected git log line: {line}"));
                };
                Ok(LogEntry {
                    sha: sha.to_owned(),
                    tree: tree.to_owned(),
                    parents: parents.split_whitespace().map(str::to_owned).collect(),
                    subject: subject.to_owned(),
                })
            })
            .collect()
    }

    /// The tree of git's automatic merge of `ours` and `theirs`, conflict markers included;
    /// `None` when git cannot tell (`merge-tree --write-tree` needs git 2.38). It writes
    /// objects, so it only runs on a read-only repository.
    pub fn merge_tree(&self, ours: &str, theirs: &str) -> Result<Option<String>> {
        if self._scratch.is_none() {
            return fail("merge-tree needs a read-only repository");
        }
        let output = self.exec(
            &[
                "merge-tree",
                "--write-tree",
                "--end-of-options",
                ours,
                theirs,
            ],
            None,
            &[],
        )?;
        // Exit 1 means conflicts; the first line is still the merged tree.
        if !matches!(output.status.code(), Some(0 | 1)) {
            return Ok(None);
        }
        let printed = text(output.stdout)?;
        Ok(printed.lines().next().map(str::to_owned))
    }

    /// The paths git cannot merge when it merges `theirs` into `ours`; empty for a clean merge.
    /// It writes objects, so it only runs on a read-only repository.
    pub fn merge_conflicts(&self, ours: &str, theirs: &str) -> Result<Vec<String>> {
        if self._scratch.is_none() {
            return fail("merge-tree needs a read-only repository");
        }
        let args = [
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "--end-of-options",
            ours,
            theirs,
        ];
        let output = self.exec(&args, None, &[])?;
        match output.status.code() {
            Some(0) => Ok(Vec::new()),
            // Exit 1 means conflicts: the merged tree, then one line per conflicted path.
            Some(1) => {
                let mut paths: Vec<String> = text(output.stdout)?
                    .lines()
                    .skip(1)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect();
                paths.dedup();
                Ok(paths)
            }
            _ => fail(format!(
                "git merge-tree failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )),
        }
    }

    pub fn range_patch_id(&self, from: Option<&str>, to: &str) -> Result<Option<String>> {
        let from = match from {
            Some(from) => from.to_owned(),
            None => text(self.checked(
                &["hash-object", "-t", "tree", "--stdin"],
                Some(Vec::new()),
                &[],
            )?)?,
        };
        let mut args = vec!["diff", "--no-renames"];
        args.extend(DIFF_FLAGS);
        args.extend(["--end-of-options", &from, to, "--"]);
        let patch = self.checked(&args, None, &[])?;
        Ok(self.patch_ids(patch)?.into_iter().next().map(|(id, _)| id))
    }

    fn patch_ids(&self, patch: Vec<u8>) -> Result<Vec<(String, String)>> {
        if patch.is_empty() {
            return Ok(Vec::new());
        }
        let output = text(self.checked(&["patch-id", "--stable"], Some(patch), &[])?)?;
        Ok(output
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(id, sha)| (id.to_owned(), sha.to_owned()))
            .collect())
    }

    /// `git diff -U3 -M`, the context forges show around review comments.
    pub fn diff_context(&self, from: &str, to: &str) -> Result<String> {
        let mut args = vec!["diff", "-U3", "-M"];
        args.extend(DIFF_FLAGS);
        args.extend(["--end-of-options", from, to, "--"]);
        let diff = self.checked(&args, None, &[])?;
        Ok(String::from_utf8_lossy(&diff).into_owned())
    }

    pub fn diff_u0(&self, from: &str, to: &str) -> Result<String> {
        let mut args = vec!["diff", "-U0", "-M"];
        args.extend(DIFF_FLAGS);
        args.extend(["--end-of-options", from, to, "--"]);
        let diff = self.checked(&args, None, &[])?;
        Ok(String::from_utf8_lossy(&diff).into_owned())
    }

    /// `<host>/<owner>/<repo>` from `origin`, else `local/<name>-<hash of the git common dir>`.
    pub fn origin_url(&self) -> Result<Option<String>> {
        self.git_opt(&["remote", "get-url", "origin"])
    }

    /// The absolute git common dir, which names a `local/` repository.
    pub fn common_dir(&self) -> Result<String> {
        self.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
    }

    pub fn main_checkout_root(&self) -> Result<PathBuf> {
        if let Some(root) = self.main_checkout.get() {
            return Ok(root.clone());
        }
        if self.git(&["rev-parse", "--is-bare-repository"])? == "true" {
            return fail("repository-local state requires a non-bare main checkout");
        }
        let listing = self.checked(&["worktree", "list", "--porcelain", "-z"], None, &[])?;
        let first = listing.split(|byte| *byte == 0).next().unwrap_or_default();
        let path = first
            .strip_prefix(b"worktree ")
            .ok_or_else(|| Error("git did not report its main checkout".into()))?;
        use std::os::unix::ffi::OsStrExt;
        let root = fs::canonicalize(Path::new(std::ffi::OsStr::from_bytes(path)))
            .map_err(ctx("main checkout is unavailable"))?;
        let main = Self::at(&root);
        if main.git(&["rev-parse", "--is-bare-repository"])? == "true"
            || fs::canonicalize(main.common_dir()?).map_err(ctx("resolve main git directory"))?
                != fs::canonicalize(self.common_dir()?).map_err(ctx("resolve git directory"))?
        {
            return fail("main checkout does not belong to this repository");
        }
        let _ = self.main_checkout.set(root.clone());
        Ok(root)
    }

    pub fn repo_key(&self) -> Result<String> {
        if let Some(url) = self.origin_url()?
            && let Some(key) = remote_key(&url)
        {
            return Ok(key);
        }
        let common = self.common_dir()?;
        let path = Path::new(&common);
        let named = if path.file_name().is_some_and(|name| name == ".git") {
            path.parent()
        } else {
            Some(path)
        };
        let name = named
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().trim_end_matches(".git").to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "repo".to_owned());
        let hash = self.hash_text(&common)?;
        let short = hash.get(..12).unwrap_or(&hash);
        Ok(format!("local/{name}-{short}"))
    }

    /// A stable id for `value`: git hash-object stays the same across Rust versions, unlike std
    /// hashers. It writes no object.
    pub fn hash_text(&self, value: &str) -> Result<String> {
        text(self.checked(
            &["hash-object", "--stdin"],
            Some(value.as_bytes().to_vec()),
            &[],
        )?)
    }
}

fn text(bytes: Vec<u8>) -> Result<String> {
    let text = String::from_utf8(bytes).map_err(ctx("git printed invalid UTF-8"))?;
    Ok(text.trim_end_matches('\n').to_owned())
}

/// Splits a remote URL into its lowercase host and its `owner/repo` path (GitLab subgroups kept).
pub fn split_remote(url: &str) -> Option<(String, String)> {
    let (authority, path) = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?,
        None => {
            let (authority, path) = url.split_once(':')?;
            if authority.contains('/') {
                return None;
            }
            (authority, path)
        }
    };
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host.split(':').next().unwrap_or_default();
    if host.is_empty() {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if segments.len() < 2 {
        return None;
    }
    Some((host.to_ascii_lowercase(), segments.join("/")))
}

fn remote_key(url: &str) -> Option<String> {
    split_remote(url).map(|(host, path)| format!("{host}/{path}"))
}

#[cfg(test)]
mod tests {
    use super::remote_key;

    #[test]
    fn parses_remote_urls() {
        let cases = [
            (
                "git@github.com:owner/repo.git",
                Some("github.com/owner/repo"),
            ),
            (
                "https://GitHub.com/owner/repo",
                Some("github.com/owner/repo"),
            ),
            (
                "ssh://git@gitlab.example.com:2222/group/sub/repo.git",
                Some("gitlab.example.com/group/sub/repo"),
            ),
            ("https://user:token@host.io/o/r.git/", Some("host.io/o/r")),
            ("/srv/git/repo.git", None),
            ("file:///srv/git/repo.git", None),
        ];
        for (url, expected) in cases {
            assert_eq!(remote_key(url).as_deref(), expected, "{url}");
        }
    }
}
