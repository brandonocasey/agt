use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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
    env: Vec<(OsString, OsString)>,
    _scratch: Option<tempfile::TempDir>,
}

impl Repo {
    /// The repository whose work tree is `root`, which MUST be a directory git accepts.
    pub fn at(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            env: Vec::new(),
            _scratch: None,
        }
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
            .args(["-c", "core.quotepath=false"])
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

    /// Tree id of the work tree as `git add -A` would stage it, built in a copy of the index so
    /// the real index is untouched and tracked ignored files stay in the tree.
    pub fn worktree_tree(&self) -> Result<String> {
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
        let env = [("GIT_INDEX_FILE", temp_index.as_path())];
        self.checked(&["add", "-A"], None, &env)?;
        text(self.checked(&["write-tree"], None, &env)?)
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
