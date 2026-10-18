//! `agt worktree`: list, remove with a backup, restore.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{Value, json};

use crate::support::Env;

mod cleanup;

const BIN: &str = env!("CARGO_BIN_EXE_agt");

struct Fixture {
    env: Env,
    wt: PathBuf,
}

impl Fixture {
    /// A main checkout with a commit and a linked worktree `feat` that has one more commit.
    fn new() -> Self {
        let env = Env::new();
        env.write(".gitignore", ".env\nnode_modules/\nbuild/\n");
        env.write("a.txt", "a\n");
        env.write("b.txt", "b\n");
        env.write("c.txt", "c\n");
        env.commit_all("base");
        let wt = env.path("trees").join("feat");
        env.git(&["worktree", "add", "-q", "-b", "feat", &wt.to_string_lossy()]);
        fs::write(wt.join("feature.txt"), "feature\n").unwrap();
        wgit(&env, &wt, &["add", "-A"]);
        wgit(&env, &wt, &["commit", "-q", "-m", "feature"]);
        Self { env, wt }
    }

    /// Dirties the worktree: tracked, staged, deleted and untracked files plus ignored ones.
    fn dirty(&self) {
        let wt = &self.wt;
        fs::write(wt.join("a.txt"), "a changed\n").unwrap();
        fs::write(wt.join("b.txt"), "b staged\n").unwrap();
        wgit(&self.env, wt, &["add", "b.txt"]);
        fs::remove_file(wt.join("c.txt")).unwrap();
        fs::write(wt.join("new.bin"), [0u8, 159, 146, 150, 10]).unwrap();
        fs::set_permissions(wt.join("new.bin"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(wt.join(".env"), "SECRET=1\n").unwrap();
        fs::set_permissions(wt.join(".env"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::create_dir_all(wt.join("node_modules/pkg")).unwrap();
        fs::write(wt.join("node_modules/pkg/index.js"), "x").unwrap();
        fs::create_dir_all(wt.join("build")).unwrap();
        fs::write(wt.join("build/out.o"), "o").unwrap();
    }

    fn agt(&self, dir: &Path, args: &[&str]) -> Output {
        self.env
            .command(BIN, dir)
            .arg("worktree")
            .args(args)
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.agt(&self.env.repo, args);
        assert!(
            output.status.success(),
            "agt worktree {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn refuses(&self, dir: &Path, args: &[&str], message: &str) {
        let output = self.agt(dir, args);
        assert_eq!(output.status.code(), Some(1), "agt worktree {args:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(message), "{error}");
    }

    fn remove(&self) -> Value {
        self.json(&["remove", &self.wt.to_string_lossy(), "--yes"])
    }
}

fn wgit(env: &Env, dir: &Path, args: &[&str]) -> String {
    let output = env.command("git", dir).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end_matches('\n')
        .to_owned()
}

#[test]
fn remove_then_restore_returns_every_file_byte_for_byte() {
    let f = Fixture::new();
    f.dirty();
    let feature = f.env.git(&["rev-parse", "feat"]);
    let removed = f.remove();

    assert_backup(&f, &removed, &feature);

    // The branch is deleted, yet the backup ref keeps its commits.
    f.env.git(&["branch", "-D", "feat"]);
    f.env.git(&["reflog", "expire", "--expire=now", "--all"]);
    f.env.git(&["gc", "-q", "--prune=now"]);
    let reference = removed["ref"].as_str().unwrap();
    f.env
        .git(&["merge-base", "--is-ancestor", &feature, reference]);
    assert_eq!(f.env.git(&["rev-parse", &format!("{reference}^")]), feature);

    let restored = f.json(&["restore", removed["id"].as_str().unwrap()]);
    assert_eq!(restored["warnings"], json!([]));
    assert_restored(&f, &feature);
}

fn assert_backup(f: &Fixture, removed: &Value, feature: &str) {
    assert!(!f.wt.exists());
    let id = removed["id"].as_str().unwrap();
    assert_eq!(removed["ref"], format!("refs/agt/backups/{id}"));
    assert_eq!(removed["copied_ignored"], 1);
    assert_eq!(removed["snapshot_files"], 4);
    assert_eq!(removed["head_on_local_branch"], true);
    assert_eq!(removed["head_on_remote_branch"], false);
    let dir = PathBuf::from(removed["backup_dir"].as_str().unwrap());
    assert!(dir.starts_with(f.env.path("state/agents/backups/repo")));
    assert!(id.ends_with("-worktree-feat"), "{id}");
    let record: Value =
        serde_json::from_slice(&fs::read(dir.join("backup.json")).unwrap()).unwrap();
    assert_eq!(record["copied"], json!([".env"]));
    let skipped = record["skipped"].as_array().unwrap();
    assert!(skipped.contains(&json!("node_modules/")), "{skipped:?}");
    assert!(skipped.contains(&json!("build/")), "{skipped:?}");
    assert_eq!(record["branch"], "feat");
    assert_eq!(record["head"], feature);
    let copy = dir.join("files/.env");
    assert_eq!(fs::read_to_string(&copy).unwrap(), "SECRET=1\n");
    assert_eq!(
        fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!dir.join("files/node_modules").exists());
    assert_eq!(
        fs::metadata(dir.join("files"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

fn assert_restored(f: &Fixture, feature: &str) {
    let wt = &f.wt;
    assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "a changed\n");
    assert_eq!(fs::read_to_string(wt.join("b.txt")).unwrap(), "b staged\n");
    assert!(!wt.join("c.txt").exists());
    assert_eq!(
        fs::read(wt.join("new.bin")).unwrap(),
        [0u8, 159, 146, 150, 10]
    );
    assert_eq!(
        fs::metadata(wt.join("new.bin"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(fs::read_to_string(wt.join(".env")).unwrap(), "SECRET=1\n");
    assert_eq!(
        fs::metadata(wt.join(".env")).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!wt.join("node_modules").exists());
    assert_eq!(
        wgit(&f.env, wt, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feat"
    );
    assert_eq!(wgit(&f.env, wt, &["rev-parse", "HEAD"]), feature);
    let status = wgit(&f.env, wt, &["status", "--porcelain"]);
    let mut lines: Vec<&str> = status.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, [" D c.txt", " M a.txt", " M b.txt", "?? new.bin"]);
}

#[test]
fn plan_mode_changes_nothing() {
    let f = Fixture::new();
    f.dirty();
    let plan = f.json(&["remove", &f.wt.to_string_lossy()]);
    assert_eq!(plan["plan"], true);
    assert_eq!(plan["copied_ignored"], json!([".env"]));
    assert!(f.wt.join("a.txt").exists());
    assert_eq!(
        fs::read_to_string(f.wt.join("a.txt")).unwrap(),
        "a changed\n"
    );
    assert!(!f.env.path("state/agents/backups").exists());
    assert_eq!(f.env.git(&["for-each-ref", "refs/agt"]), "");
    assert!(f.env.git(&["worktree", "list"]).contains("feat"));
}

#[test]
fn refuses_what_it_must_not_remove() {
    let f = Fixture::new();
    let wt = f.wt.to_string_lossy().into_owned();
    let repo = f.env.repo.to_string_lossy().into_owned();
    f.refuses(&f.env.repo, &["remove", &repo, "--yes"], "main checkout");
    f.refuses(
        &f.env.repo,
        &[
            "remove",
            &f.env.repo.join(".gitignore").to_string_lossy(),
            "--yes",
        ],
        "not a linked worktree",
    );
    f.env.write("sub/x.txt", "x");
    f.refuses(
        &f.env.repo,
        &["remove", &f.env.repo.join("sub").to_string_lossy(), "--yes"],
        "not a linked worktree",
    );
    f.refuses(
        &f.env.repo,
        &["remove", "/nonexistent/dir", "--yes"],
        "not a linked worktree",
    );
    f.refuses(&f.wt, &["remove", &wt, "--yes"], "current directory");
    fs::create_dir(f.wt.join("deep")).unwrap();
    f.refuses(
        &f.wt.join("deep"),
        &["remove", &wt, "--yes"],
        "current directory",
    );

    // A live Claude Code session whose cwd is inside the worktree, matched by process start.
    let me = std::process::id();
    let sessions = f.env.path("home/.claude/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let start = proc_start(me);
    fs::write(
        sessions.join(format!("{me}.json")),
        json!({ "pid": me, "cwd": f.wt.join("deep"), "procStart": start }).to_string(),
    )
    .unwrap();
    f.refuses(&f.env.repo, &["remove", &wt, "--yes"], "session");
    // A record for a reused pid does not count.
    let stale = if cfg!(target_os = "linux") {
        "1"
    } else {
        "Mon Jan  1 00:00:00 2024"
    };
    fs::write(
        sessions.join(format!("{me}.json")),
        json!({ "pid": me, "cwd": f.wt, "procStart": stale }).to_string(),
    )
    .unwrap();
    fs::remove_dir(f.wt.join("deep")).unwrap();

    f.env.git(&["worktree", "lock", &wt]);
    f.refuses(&f.env.repo, &["remove", &wt, "--yes"], "locked");
    f.env.git(&["worktree", "unlock", &wt]);

    assert!(f.wt.join("feature.txt").exists());
    assert!(!f.env.path("state/agents/backups").exists());
    f.remove();
    assert!(!f.wt.exists());
}

/// The `procStart` Claude Code records for `pid`.
fn proc_start(pid: u32) -> String {
    if cfg!(target_os = "linux") {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let after = &stat[stat.rfind(')').unwrap() + 2..];
        after.split(' ').nth(19).unwrap().to_owned()
    } else {
        let output = std::process::Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

#[test]
fn a_submodule_with_unpublished_work_blocks_removal() {
    let f = Fixture::new();
    let source = f.env.path("subsrc");
    fs::create_dir_all(&source).unwrap();
    wgit(&f.env, &source, &["init", "-q", "-b", "main"]);
    fs::write(source.join("lib.txt"), "lib\n").unwrap();
    wgit(&f.env, &source, &["add", "-A"]);
    wgit(&f.env, &source, &["commit", "-q", "-m", "lib"]);
    let allow = ["-c", "protocol.file.allow=always"];
    let add = |dir: &Path, args: &[&str]| {
        let mut full = allow.to_vec();
        full.extend_from_slice(args);
        wgit(&f.env, dir, &full)
    };
    add(
        &f.env.repo,
        &["submodule", "add", "-q", &source.to_string_lossy(), "lib"],
    );
    f.env.git(&["commit", "-q", "-m", "add submodule"]);
    let wt2 = f.env.path("trees").join("sub");
    f.env
        .git(&["worktree", "add", "-q", "-b", "sub", &wt2.to_string_lossy()]);
    add(&wt2, &["submodule", "update", "--init"]);
    let wt2_text = wt2.to_string_lossy().into_owned();

    // Uncommitted change inside the submodule.
    fs::write(wt2.join("lib/scratch.txt"), "x").unwrap();
    f.refuses(
        &f.env.repo,
        &["remove", &wt2_text, "--yes"],
        "submodule lib",
    );
    fs::remove_file(wt2.join("lib/scratch.txt")).unwrap();

    // A commit in the submodule that the worktree does not record.
    fs::write(wt2.join("lib/lib.txt"), "changed\n").unwrap();
    wgit(&f.env, &wt2.join("lib"), &["commit", "-q", "-am", "local"]);
    f.refuses(
        &f.env.repo,
        &["remove", &wt2_text, "--yes"],
        "submodule lib",
    );
    assert!(wt2.exists());

    // Back on the recorded commit, which the remote-tracking branch holds, removal proceeds.
    wgit(
        &f.env,
        &wt2.join("lib"),
        &["checkout", "-q", "--detach", "HEAD~1"],
    );
    f.json(&["remove", &wt2_text, "--yes"]);
    assert!(!wt2.exists());
}

#[test]
fn restore_warns_when_the_branch_moved_and_refuses_an_existing_path() {
    let f = Fixture::new();
    f.dirty();
    let id = f.remove()["id"].as_str().unwrap().to_owned();
    let feature = f.env.git(&["rev-parse", "feat"]);
    f.env.git(&["switch", "-q", "feat"]);
    f.env.write("more.txt", "m");
    f.env.commit_all("moved");
    f.env.git(&["switch", "-q", "main"]);

    let restored = f.json(&["restore", &id]);
    let warning = restored["warnings"][0].as_str().unwrap();
    assert!(
        warning.contains("moved") && warning.contains(&feature),
        "{warning}"
    );
    assert_eq!(
        fs::read_to_string(f.wt.join("a.txt")).unwrap(),
        "a changed\n"
    );
    f.refuses(&f.env.repo, &["restore", &id], "already exists");

    let elsewhere = f.env.path("trees").join("again");
    // The branch is now checked out in the first restore, so a second one is detached.
    let second = f.json(&["restore", &id, "--path", &elsewhere.to_string_lossy()]);
    let warning = second["warnings"][0].as_str().unwrap();
    assert!(warning.contains("another worktree"), "{warning}");
    assert_eq!(wgit(&f.env, &elsewhere, &["rev-parse", "HEAD"]), feature);
    assert_eq!(
        fs::read_to_string(elsewhere.join("a.txt")).unwrap(),
        "a changed\n"
    );
}

#[test]
fn list_reports_counts_ahead_and_merged_for_this_repo_and_all_roots() {
    let f = Fixture::new();
    f.dirty();
    let merged = f.env.path("trees").join("merged");
    f.env.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "merged",
        &merged.to_string_lossy(),
    ]);
    let listed = f.json(&["list"]);
    let rows = listed["worktrees"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let feat = rows.iter().find(|row| row["branch"] == "feat").unwrap();
    let real = fs::canonicalize(&f.wt).unwrap();
    assert_eq!(feat["path"], real.to_string_lossy().as_ref());
    assert_eq!(feat["modified"], 3);
    assert_eq!(feat["untracked"], 1);
    assert_eq!(feat["ignored"], 1);
    assert_eq!(feat["ahead"], 1);
    assert_eq!(feat["behind"], 0);
    assert_eq!(feat["merged"], false);
    assert_eq!(feat["default_branch"], "main");
    assert_eq!(feat["locked"], false);
    let done = rows.iter().find(|row| row["branch"] == "merged").unwrap();
    assert_eq!(done["merged"], true);
    assert_eq!(done["ahead"], 0);

    // `--all` scans the configured roots, not the current repository.
    let other = f.env.path("home/code/proj");
    fs::create_dir_all(&other).unwrap();
    wgit(&f.env, &other, &["init", "-q", "-b", "trunk"]);
    fs::write(other.join("x"), "x").unwrap();
    wgit(&f.env, &other, &["add", "-A"]);
    wgit(&f.env, &other, &["commit", "-q", "-m", "x"]);
    let tree = f.env.path("trees").join("other");
    wgit(
        &f.env,
        &other,
        &["worktree", "add", "-q", "-b", "o", &tree.to_string_lossy()],
    );
    fs::create_dir_all(f.env.path("config/agt")).unwrap();
    fs::write(
        f.env.path("config/agt/config.toml"),
        "[worktree]\nroots = [\"~/code/*\"]\n",
    )
    .unwrap();
    let all = f.json(&["list", "--all"]);
    let rows = all["worktrees"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{all}");
    assert_eq!(rows[0]["branch"], "o");
    assert_eq!(rows[0]["default_branch"], Value::Null);
    assert_eq!(
        rows[0]["repository"],
        fs::canonicalize(&other).unwrap().to_string_lossy().as_ref()
    );
}

#[test]
fn backups_list_show_and_rm() {
    let f = Fixture::new();
    f.dirty();
    let removed = f.remove();
    let id = removed["id"].as_str().unwrap();
    let dir = removed["backup_dir"].as_str().unwrap();

    let listed = f.json(&["backups"]);
    assert_eq!(listed["backups"][0]["id"], id);
    let shown = f.json(&["backups", "show", id]);
    assert_eq!(shown["dir"], dir);
    assert_eq!(
        shown["worktree"],
        fs::canonicalize(f.env.path("trees"))
            .unwrap()
            .join("feat")
            .to_string_lossy()
            .as_ref()
    );

    let plan = f.json(&["backups", "rm", id]);
    assert_eq!(plan["plan"], true);
    assert!(Path::new(dir).exists());
    f.json(&["backups", "rm", id, "--yes"]);
    assert!(!Path::new(dir).exists());
    assert_eq!(f.env.git(&["for-each-ref", "refs/agt"]), "");
    assert_eq!(f.json(&["backups"])["backups"], json!([]));
    f.refuses(&f.env.repo, &["backups", "show", id], "no worktree backup");
}

#[test]
fn list_names_the_live_sessions_inside_each_worktree() {
    let f = Fixture::new();
    let me = std::process::id();
    let sessions = f.env.path("home/.claude/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::create_dir(f.wt.join("deep")).unwrap();
    fs::write(
        sessions.join(format!("{me}.json")),
        json!({
            "pid": me,
            "cwd": f.wt.join("deep"),
            "procStart": proc_start(me),
            "name": "builder",
            "status": "busy",
            "sessionId": "abc",
        })
        .to_string(),
    )
    .unwrap();
    let listed = f.json(&["list"]);
    let feat = &listed["worktrees"][0];
    assert_eq!(
        feat["agents"],
        json!([{ "pid": me, "name": "builder", "status": "busy", "session": "abc" }])
    );

    // A record for a reused pid is not an agent.
    fs::write(
        sessions.join(format!("{me}.json")),
        json!({ "pid": me, "cwd": f.wt, "procStart": "Mon Jan  1 00:00:00 2024" }).to_string(),
    )
    .unwrap();
    let listed = f.json(&["list"]);
    assert_eq!(listed["worktrees"][0]["agents"], json!([]));
}

/// Makes `dir` a git repository of its own (an untracked nested repository for its parent).
fn nest(env: &Env, dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("lib.txt"), "lib\n").unwrap();
    wgit(env, dir, &["init", "-q"]);
    wgit(env, dir, &["add", "-A"]);
    wgit(env, dir, &["commit", "-q", "-m", "lib"]);
}

#[test]
fn a_nested_repository_blocks_removal_in_plan_and_yes_alike() {
    let f = Fixture::new();
    nest(&f.env, &f.wt.join("vendor/lib"));
    nest(&f.env, &f.wt.join("tools/deep/clone"));
    nest(&f.env, &f.wt.join("node_modules/pkg"));
    let path = f.wt.to_string_lossy().into_owned();
    for args in [vec!["remove", &path], vec!["remove", &path, "--yes"]] {
        let output = f.agt(&f.env.repo, &args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        for nested in ["vendor/lib", "tools/deep/clone", "node_modules/pkg"] {
            assert!(error.contains(nested), "{nested}: {error}");
        }
    }
    assert!(f.wt.join("vendor/lib/.git").exists());
    assert!(f.wt.join("node_modules/pkg/.git").exists());
    assert_eq!(f.env.git(&["for-each-ref", "refs/agt/backups"]), "");
}

#[test]
fn a_registered_submodule_is_not_a_nested_repository() {
    let f = Fixture::new();
    let sub = f.env.path("sub-origin");
    nest(&f.env, &sub);
    wgit(
        &f.env,
        &f.wt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &sub.to_string_lossy(),
            "mod",
        ],
    );
    wgit(&f.env, &f.wt, &["commit", "-q", "-m", "add submodule"]);
    let plan = f.json(&["remove", &f.wt.to_string_lossy()]);
    assert_eq!(plan["plan"], true);
}

#[test]
fn listing_never_rewrites_a_worktree_index() {
    let f = Fixture::new();
    let index = PathBuf::from(wgit(
        &f.env,
        &f.wt,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    ));
    // Same content, new mtime: a plain `git status` refreshes and rewrites the index.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fs::write(f.wt.join("a.txt"), "a\n").unwrap();
    let before = fs::read(&index).unwrap();
    let listed = f.json(&["list"]);
    assert_eq!(listed["worktrees"].as_array().unwrap().len(), 1);
    let plan = f.json(&["remove", &f.wt.to_string_lossy()]);
    assert_eq!(plan["plan"], true);
    assert_eq!(fs::read(&index).unwrap(), before);
}

#[test]
fn list_all_warns_about_an_unreadable_repository_and_lists_the_rest() {
    let f = Fixture::new();
    let broken = f.env.path("home/code/broken");
    fs::create_dir_all(broken.join(".git")).unwrap();
    let good = f.env.path("home/code/good");
    fs::create_dir_all(good.parent().unwrap()).unwrap();
    f.env.git(&[
        "clone",
        "-q",
        &f.env.repo.to_string_lossy(),
        &good.to_string_lossy(),
    ]);
    wgit(
        &f.env,
        &good,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "o",
            &f.env.path("trees/o").to_string_lossy(),
        ],
    );
    fs::create_dir_all(f.env.path("config/agt")).unwrap();
    fs::write(
        f.env.path("config/agt/config.toml"),
        "[worktree]\nroots = [\"~/code/*\"]\n",
    )
    .unwrap();
    let all = f.json(&["list", "--all"]);
    let rows = all["worktrees"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{all}");
    let warnings = all["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{all}");
    assert!(warnings[0].as_str().unwrap().contains("broken"), "{all}");
}

#[test]
fn a_failed_backup_leaves_no_ref_and_no_directory() {
    let f = Fixture::new();
    f.dirty();
    let secret = f.wt.join(".env");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
    let path = f.wt.to_string_lossy().into_owned();
    let output = f.agt(&f.env.repo, &["remove", &path, "--yes"]);
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(f.wt.join("feature.txt").exists());
    assert_eq!(f.env.git(&["for-each-ref", "refs/agt/backups"]), "");
    let backups = f.env.path("state/agents/backups");
    let left = fs::read_dir(&backups)
        .map(|dirs| {
            dirs.flatten()
                .flat_map(|repo| fs::read_dir(repo.path()).unwrap().flatten())
                .count()
        })
        .unwrap_or(0);
    assert_eq!(left, 0);
}

#[test]
fn remove_and_restore_leave_other_missing_worktrees_registered() {
    let f = Fixture::new();
    let other = f.env.path("trees/other");
    f.env.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "other",
        &other.to_string_lossy(),
    ]);
    fs::remove_dir_all(&other).unwrap();
    let admin = |name: &str| f.env.repo.join(".git/worktrees").join(name);
    assert!(admin("other").exists());

    let removed = f.remove();
    assert!(admin("other").exists(), "remove pruned another worktree");

    // A stale registration of the restore target itself is cleared; the other one stays.
    let stale = f.env.path("trees/again");
    f.env.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "again",
        &stale.to_string_lossy(),
    ]);
    fs::remove_dir_all(&stale).unwrap();
    let id = removed["id"].as_str().unwrap();
    f.json(&["restore", id, "--path", &stale.to_string_lossy()]);
    assert!(stale.join("feature.txt").exists());
    assert!(admin("other").exists(), "restore pruned another worktree");
}
