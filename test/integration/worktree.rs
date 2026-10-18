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

    // Another live Claude Code session whose cwd is inside the worktree, matched by process
    // start. The test process itself is agt's parent, so it would be the calling session.
    let other = std::process::Command::new("sleep")
        .arg("60")
        .current_dir(&f.env.repo)
        .spawn()
        .unwrap();
    let _other = Reaped::new(other);
    let me = _other.0.id();
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
    let tip = f.env.git(&["rev-parse", "feat"]);
    f.env.git(&["switch", "-q", "main"]);

    let restored = f.json(&["restore", &id]);
    let warning = restored["warnings"][0].as_str().unwrap();
    assert!(
        warning.contains("moved") && warning.contains(&feature) && warning.contains(&tip),
        "{warning}"
    );
    // Detached at the snapshot's parent: the newer commit is not shown as reverted changes.
    assert_eq!(wgit(&f.env, &f.wt, &["rev-parse", "HEAD"]), feature);
    assert_eq!(wgit(&f.env, &f.wt, &["branch", "--show-current"]), "");
    assert!(!f.wt.join("more.txt").exists());
    assert!(!wgit(&f.env, &f.wt, &["status", "--porcelain"]).contains("more.txt"));
    assert_eq!(f.env.git(&["rev-parse", "feat"]), tip);
    assert_eq!(
        fs::read_to_string(f.wt.join("a.txt")).unwrap(),
        "a changed\n"
    );
    f.refuses(&f.env.repo, &["restore", &id], "already exists");

    let elsewhere = f.env.path("trees").join("again");
    let second = f.json(&["restore", &id, "--path", &elsewhere.to_string_lossy()]);
    let warning = second["warnings"][0].as_str().unwrap();
    assert!(warning.contains("moved"), "{warning}");
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

/// Registers a submodule `name` at `path` in the worktree and commits it.
fn add_submodule(f: &Fixture, name: &str, path: &str) {
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
            "--name",
            name,
            &sub.to_string_lossy(),
            path,
        ],
    );
    wgit(&f.env, &f.wt, &["commit", "-q", "-m", "add submodule"]);
}

#[test]
fn a_registered_submodule_is_not_a_nested_repository() {
    let f = Fixture::new();
    add_submodule(&f, "mod", "mod");
    let plan = f.json(&["remove", &f.wt.to_string_lossy()]);
    assert_eq!(plan["plan"], true);
}

#[test]
fn a_submodule_whose_name_has_a_space_is_registered() {
    let f = Fixture::new();
    add_submodule(&f, "my lib", "libdir");
    let plan = f.json(&["remove", &f.wt.to_string_lossy()]);
    assert_eq!(plan["plan"], true);
}

#[test]
fn a_submodule_whose_path_has_a_space_can_be_checked() {
    let f = Fixture::new();
    add_submodule(&f, "spaced", "lib dir");
    let plan = f.json(&["remove", &f.wt.to_string_lossy()]);
    assert_eq!(plan["plan"], true);
    f.remove();
    assert!(!f.wt.exists());
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

/// A child process killed and reaped when dropped, with its whole process group when it leads
/// one, so nothing it forked outlives the test.
struct Reaped(std::process::Child, bool);

impl Reaped {
    fn new(child: std::process::Child) -> Self {
        Self(child, false)
    }

    /// Runs `command` as the leader of its own process group.
    fn group(command: &mut std::process::Command) -> Self {
        use std::os::unix::process::CommandExt;
        Self(command.process_group(0).spawn().unwrap(), true)
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        if self.1 {
            let group = i32::try_from(self.0.id()).unwrap();
            // SAFETY: killpg only sends a signal to the group this guard's child leads.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn record_session(f: &Fixture, pid: u32, cwd: &Path) {
    let sessions = f.env.path("home/.claude/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join(format!("{pid}.json")),
        json!({ "pid": pid, "cwd": cwd, "procStart": proc_start(pid) }).to_string(),
    )
    .unwrap();
}

#[test]
fn the_calling_session_does_not_block_its_own_removal_but_another_does() {
    let f = Fixture::new();
    let wt = f.wt.to_string_lossy().into_owned();
    // The test process is agt's parent: the calling session.
    record_session(&f, std::process::id(), &f.wt);
    let plan = f.json(&["remove", &wt]);
    assert_eq!(plan["plan"], true);

    // CLAUDE_PID names the calling session too.
    let caller = Reaped::new(
        std::process::Command::new("sleep")
            .arg("60")
            .current_dir(&f.env.repo)
            .spawn()
            .unwrap(),
    );
    record_session(&f, caller.0.id(), &f.wt);
    let output = f
        .env
        .command(BIN, &f.env.repo)
        .env("CLAUDE_PID", caller.0.id().to_string())
        .args(["worktree", "remove", &wt])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Without CLAUDE_PID it is another live session.
    f.refuses(&f.env.repo, &["remove", &wt, "--yes"], "session");
}

#[test]
fn a_process_with_its_directory_inside_the_worktree_blocks_removal() {
    let f = Fixture::new();
    let wt = f.wt.to_string_lossy().into_owned();
    let child = Reaped::new(
        std::process::Command::new("sleep")
            .arg("60")
            .current_dir(&f.wt)
            .spawn()
            .unwrap(),
    );
    // Give lsof or /proc time to see the child's directory.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let pid = child.0.id().to_string();
    for args in [vec!["remove", &wt], vec!["remove", &wt, "--yes"]] {
        let output = f.agt(&f.env.repo, &args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(&pid) && error.contains("sleep"), "{error}");
    }
    assert!(f.wt.join("feature.txt").exists());
}

#[test]
fn restore_detaches_when_the_branch_is_checked_out_in_another_worktree() {
    let f = Fixture::new();
    f.dirty();
    let id = f.remove()["id"].as_str().unwrap().to_owned();
    let feature = f.env.git(&["rev-parse", "feat"]);
    // The branch has not moved, but another worktree holds it now.
    let other = f.env.path("trees/holder");
    f.env
        .git(&["worktree", "add", "-q", &other.to_string_lossy(), "feat"]);

    let restored = f.json(&["restore", &id]);
    let warning = restored["warnings"][0].as_str().unwrap();
    assert!(
        warning.contains("checked out in another worktree") && warning.contains(&feature),
        "{warning}"
    );
    assert_eq!(wgit(&f.env, &f.wt, &["rev-parse", "HEAD"]), feature);
    assert_eq!(wgit(&f.env, &f.wt, &["branch", "--show-current"]), "");
    assert_eq!(
        fs::read_to_string(f.wt.join("a.txt")).unwrap(),
        "a changed\n"
    );
}

#[test]
fn a_relative_restore_path_means_the_callers_directory() {
    let f = Fixture::new();
    let id = f.remove()["id"].as_str().unwrap().to_owned();
    let cwd = f.env.path("elsewhere");
    fs::create_dir_all(&cwd).unwrap();

    let output = f.agt(&cwd, &["restore", &id, "--path", "back"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(cwd.join("back/feature.txt").exists());
    assert!(!f.env.repo.join("back").exists());
}

#[test]
fn the_calling_sessions_own_processes_do_not_block_removal_but_others_do() {
    let f = Fixture::new();
    let wt = f.wt.to_string_lossy().into_owned();
    // A fake session: a shell outside the worktree whose child has its directory inside it,
    // as a task server or a tool shell of a Claude Code session does.
    let session = Reaped::group(
        std::process::Command::new("sh")
            .args(["-c", "cd \"$1\" && sleep 30 & wait", "sh", &wt])
            .current_dir(&f.env.repo),
    );
    record_session(&f, session.0.id(), &f.env.repo);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let remove = |args: &[&str]| {
        f.env
            .command(BIN, &f.env.repo)
            .env("CLAUDE_PID", session.0.id().to_string())
            .args(["worktree", "remove"])
            .args(args)
            .output()
            .unwrap()
    };

    let output = remove(&[&wt]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let warnings = plan["warnings"].as_array().unwrap();
    // The shell forks a subshell that holds the directory too.
    assert!(
        warnings
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("(sleep)")),
        "{plan}"
    );
    assert!(
        warnings
            .iter()
            .all(|warning| warning.as_str().unwrap().contains("calling session")),
        "{plan}"
    );

    // A process of nobody's session still blocks.
    let stranger = Reaped::new(
        std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&f.wt)
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    let output = remove(&[&wt, "--yes"]);
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(&stranger.0.id().to_string()), "{error}");
    assert!(f.wt.exists());
}

/// A worktree with an initialized submodule `mod`, at the commit the worktree records.
fn with_submodule() -> (Fixture, PathBuf) {
    let f = Fixture::new();
    add_submodule(&f, "mod", "mod");
    let sub = f.wt.join("mod");
    (f, sub)
}

/// Ignores `pattern` inside the submodule, whose git directory lives under the worktree's.
fn ignore_in(f: &Fixture, sub: &Path, pattern: &str) {
    let exclude = PathBuf::from(wgit(
        &f.env,
        sub,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ],
    ));
    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    fs::write(exclude, format!("{pattern}\n")).unwrap();
}

fn refuses_both_ways(f: &Fixture, message: &str) {
    let wt = f.wt.to_string_lossy().into_owned();
    f.refuses(&f.env.repo, &["remove", &wt], message);
    f.refuses(&f.env.repo, &["remove", &wt, "--yes"], message);
    assert!(f.wt.join("mod").exists());
}

#[test]
fn a_submodule_stash_blocks_removal() {
    let (f, sub) = with_submodule();
    fs::write(sub.join("lib.txt"), "changed\n").unwrap();
    wgit(&f.env, &sub, &["stash", "push", "-q"]);
    refuses_both_ways(&f, "stash");
}

#[test]
fn a_submodule_branch_that_nothing_else_holds_blocks_removal() {
    let (f, sub) = with_submodule();
    wgit(&f.env, &sub, &["switch", "-q", "-c", "unpushed"]);
    fs::write(sub.join("new.txt"), "n\n").unwrap();
    wgit(&f.env, &sub, &["add", "-A"]);
    wgit(&f.env, &sub, &["commit", "-q", "-m", "unpushed work"]);
    // Back on the recorded commit, so only the branch holds the work.
    wgit(&f.env, &sub, &["checkout", "-q", "--detach", "HEAD~1"]);
    refuses_both_ways(&f, "unpushed");
}

#[test]
fn ignored_files_in_a_submodule_block_removal_unless_disposable() {
    let (f, sub) = with_submodule();
    ignore_in(&f, &sub, ".env\nnode_modules/\nbuild/");
    fs::create_dir_all(sub.join("node_modules/pkg")).unwrap();
    fs::write(sub.join("node_modules/pkg/index.js"), "x").unwrap();
    fs::create_dir_all(sub.join("build")).unwrap();
    fs::write(sub.join("build/out.o"), "o").unwrap();
    let wt = f.wt.to_string_lossy().into_owned();
    // Dependencies and build output are recreated, so they do not block.
    assert_eq!(f.json(&["remove", &wt])["plan"], true);

    fs::write(sub.join(".env"), "SECRET=1\n").unwrap();
    refuses_both_ways(&f, ".env");
}

#[test]
fn a_populated_submodule_stays_protected_after_it_is_deinitialized_in_the_main_checkout() {
    let (f, sub) = with_submodule();
    // The main checkout has the submodule too, as it does when the worktree is made from it.
    let allow = ["-c", "protocol.file.allow=always"];
    f.env.git(&["merge", "-q", "feat"]);
    let mut update = allow.to_vec();
    update.extend(["submodule", "update", "--init"]);
    f.env.git(&update);
    fs::write(sub.join("lib.txt"), "uncommitted edit\n").unwrap();
    let wt = f.wt.to_string_lossy().into_owned();
    f.refuses(&f.env.repo, &["remove", &wt, "--yes"], "uncommitted");

    // The shared config forgets the submodule, so the worktree's copy now reads as `-`.
    f.env.git(&["submodule", "deinit", "-f", "mod"]);
    assert!(sub.join(".git").exists());
    refuses_both_ways(&f, "uncommitted");
    assert_eq!(
        fs::read_to_string(sub.join("lib.txt")).unwrap(),
        "uncommitted edit\n"
    );
}

fn worktree_at(f: &Fixture, branch: &str, path: &Path) {
    f.env.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        branch,
        &path.to_string_lossy(),
    ]);
}

#[test]
fn a_worktree_name_that_is_no_ref_name_still_backs_up_and_restores() {
    let f = Fixture::new();
    let odd = f.env.path("trees").join("my tree~1^x:y..z.lock");
    worktree_at(&f, "odd", &odd);
    fs::write(odd.join("note.txt"), "note\n").unwrap();
    let removed = f.json(&["remove", &odd.to_string_lossy(), "--yes"]);
    let id = removed["id"].as_str().unwrap();
    assert!(!id.contains([' ', '~', '^', ':']), "{id}");
    assert!(!odd.exists());

    let restored = f.json(&["restore", id]);
    assert_eq!(restored["warnings"], json!([]));
    assert_eq!(fs::read_to_string(odd.join("note.txt")).unwrap(), "note\n");
}

#[test]
fn backup_ids_are_unique_across_repositories() {
    let f = Fixture::new();
    let other = f.env.path("other-repo");
    fs::create_dir_all(&other).unwrap();
    wgit(&f.env, &other, &["init", "-q", "-b", "main"]);
    fs::write(other.join("x"), "x").unwrap();
    wgit(&f.env, &other, &["add", "-A"]);
    wgit(&f.env, &other, &["commit", "-q", "-m", "x"]);
    let tree = f.env.path("other-trees/feat");
    wgit(
        &f.env,
        &other,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            &tree.to_string_lossy(),
        ],
    );

    // The same minute and the same worktree name in two repositories.
    let run = |dir: &Path, path: &Path| -> Value {
        let output = f
            .env
            .command(BIN, dir)
            .env("AGT_NOW", "1790000000")
            .args(["worktree", "remove", &path.to_string_lossy(), "--yes"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let first = run(&f.env.repo, &f.wt);
    let second = run(&other, &tree);
    assert_ne!(first["id"], second["id"], "{first} {second}");
    for removed in [&first, &second] {
        let id = removed["id"].as_str().unwrap();
        assert_eq!(f.json(&["backups", "show", id])["id"], id);
    }
}

#[test]
fn a_worktree_on_an_unborn_branch_is_listed_and_not_removed() {
    let f = Fixture::new();
    let orphan = f.env.path("trees/orphan");
    let dir = orphan.to_string_lossy();
    // `worktree add --orphan` needs git 2.42; this builds the same unborn worktree on older git.
    f.env.git(&["worktree", "add", "-q", "--detach", &dir]);
    f.env
        .git(&["-C", &dir, "checkout", "-q", "--orphan", "orphan"]);
    f.env.git(&["-C", &dir, "rm", "-rfq", "."]);
    let listed = f.json(&["list"]);
    let row = listed["worktrees"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["branch"] == "orphan")
        .unwrap();
    assert_eq!(row["unborn"], true);
    assert!(row["ahead"].is_null() && row["behind"].is_null() && row["merged"].is_null());
    assert!(
        listed["worktrees"].as_array().unwrap().len() == 2,
        "{listed}"
    );

    let path = orphan.to_string_lossy().into_owned();
    f.refuses(&f.env.repo, &["remove", &path, "--yes"], "no commit yet");
    assert!(orphan.exists());
}

/// Commits in the submodule on a new branch and returns to the recorded commit, so only the
/// branch holds the work.
fn unpushed_branch(f: &Fixture, sub: &Path, branch: &str) {
    wgit(&f.env, sub, &["switch", "-q", "-c", branch]);
    fs::write(sub.join(format!("{branch}.txt")), "n\n").unwrap();
    wgit(&f.env, sub, &["add", "-A"]);
    wgit(&f.env, sub, &["commit", "-q", "-m", "unpushed work"]);
    wgit(&f.env, sub, &["checkout", "-q", "--detach", "HEAD~1"]);
}

/// The submodule's clone under the worktree's git directory.
fn clone_of(f: &Fixture, sub: &Path) -> PathBuf {
    PathBuf::from(wgit(&f.env, sub, &["rev-parse", "--absolute-git-dir"]))
}

#[test]
fn a_clone_left_behind_by_deinit_or_rm_in_the_worktree_still_blocks_removal() {
    for route in ["deinit", "rm", "rm-cached"] {
        let (f, sub) = with_submodule();
        unpushed_branch(&f, &sub, "keep");
        let clone = clone_of(&f, &sub);
        match route {
            "deinit" => wgit(&f.env, &f.wt, &["submodule", "deinit", "-f", "mod"]),
            "rm" => {
                wgit(&f.env, &f.wt, &["rm", "-q", "-f", "mod"]);
                wgit(&f.env, &f.wt, &["commit", "-q", "-m", "drop the submodule"])
            }
            _ => wgit(&f.env, &f.wt, &["rm", "-q", "--cached", "mod"]),
        };
        assert!(clone.exists(), "{route}");
        let wt = f.wt.to_string_lossy().into_owned();
        f.refuses(&f.env.repo, &["remove", &wt, "--yes"], "keep");
        assert!(clone.exists(), "{route}");
    }
}

#[test]
fn a_nested_repository_inside_a_submodule_disposable_directory_blocks_removal() {
    let (f, sub) = with_submodule();
    ignore_in(&f, &sub, "node_modules/");
    let nested = sub.join("node_modules/pkg");
    nest(&f.env, &nested);
    refuses_both_ways(&f, "mod/node_modules/pkg");
}

#[test]
fn untracked_files_in_a_submodule_block_removal_whatever_status_is_configured_to_show() {
    let (f, sub) = with_submodule();
    wgit(&f.env, &sub, &["config", "status.showUntrackedFiles", "no"]);
    fs::write(sub.join("notes.txt"), "notes\n").unwrap();
    refuses_both_ways(&f, "uncommitted");
}

#[test]
fn a_submodule_tag_a_linked_worktree_or_an_unfinished_operation_blocks_removal() {
    let (f, sub) = with_submodule();
    unpushed_branch(&f, &sub, "tmp");
    let tip = wgit(&f.env, &sub, &["rev-parse", "tmp"]);
    wgit(&f.env, &sub, &["branch", "-D", "tmp"]);
    wgit(&f.env, &sub, &["tag", "kept-tag", &tip]);
    refuses_both_ways(&f, "kept-tag");
    wgit(&f.env, &sub, &["tag", "-d", "kept-tag"]);

    let linked = f.env.path("sub-linked");
    wgit(
        &f.env,
        &sub,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &linked.to_string_lossy(),
        ],
    );
    refuses_both_ways(&f, "worktree");
    wgit(
        &f.env,
        &sub,
        &["worktree", "remove", "--force", &linked.to_string_lossy()],
    );

    let head = wgit(&f.env, &sub, &["rev-parse", "HEAD"]);
    fs::write(clone_of(&f, &sub).join("MERGE_HEAD"), format!("{head}\n")).unwrap();
    refuses_both_ways(&f, "in progress");
}

#[test]
fn per_worktree_refs_and_unfinished_operations_block_removal() {
    let f = Fixture::new();
    let wt = f.wt.to_string_lossy().into_owned();
    wgit(&f.env, &f.wt, &["update-ref", "refs/worktree/keep", "HEAD"]);
    f.refuses(&f.env.repo, &["remove", &wt, "--yes"], "refs/worktree/keep");
    wgit(&f.env, &f.wt, &["update-ref", "-d", "refs/worktree/keep"]);

    let git_dir = PathBuf::from(wgit(&f.env, &f.wt, &["rev-parse", "--absolute-git-dir"]));
    for marker in ["rebase-merge", "MERGE_HEAD", "CHERRY_PICK_HEAD"] {
        let path = git_dir.join(marker);
        if marker == "rebase-merge" {
            fs::create_dir(&path).unwrap();
        } else {
            fs::write(&path, "0000000000000000000000000000000000000000\n").unwrap();
        }
        f.refuses(&f.env.repo, &["remove", &wt, "--yes"], "in progress");
        if path.is_dir() {
            fs::remove_dir(&path).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
        }
    }
    assert_eq!(f.json(&["remove", &wt])["plan"], true);
}

#[test]
fn a_path_with_staged_and_unstaged_changes_blocks_removal() {
    let f = Fixture::new();
    let wt = f.wt.to_string_lossy().into_owned();
    fs::write(f.wt.join("a.txt"), "staged\n").unwrap();
    wgit(&f.env, &f.wt, &["add", "a.txt"]);
    // Staged alone is kept in the snapshot's working tree.
    assert_eq!(f.json(&["remove", &wt])["plan"], true);
    fs::write(f.wt.join("a.txt"), "worktree\n").unwrap();
    refuses_staged(&f, &wt);
}

fn refuses_staged(f: &Fixture, wt: &str) {
    f.refuses(&f.env.repo, &["remove", wt, "--yes"], "a.txt");
    f.refuses(&f.env.repo, &["remove", wt], "staged and unstaged");
    assert!(f.wt.join("a.txt").exists());
}

#[test]
fn files_in_an_uninitialized_submodule_directory_block_removal() {
    let (f, _sub) = with_submodule();
    let second = f.env.path("trees/second");
    f.env.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "second",
        &second.to_string_lossy(),
        "feat",
    ]);
    let wt = second.to_string_lossy().into_owned();
    // Nothing initialized and nothing written: removal goes ahead as a plan.
    assert_eq!(f.json(&["remove", &wt])["plan"], true);
    fs::write(second.join("mod/notes.md"), "mine\n").unwrap();
    f.refuses(&f.env.repo, &["remove", &wt, "--yes"], "notes.md");
    assert!(second.join("mod/notes.md").exists());
}

/// Removes the fixture's worktree, restores it, and returns the restored worktree's path.
fn remove_and_restore(f: &Fixture) -> Value {
    let removed = f.remove();
    f.json(&["restore", removed["id"].as_str().unwrap()]);
    removed
}

#[test]
fn edits_hidden_by_skip_worktree_or_assume_unchanged_are_backed_up_and_counted_as_changes() {
    for flag in ["--skip-worktree", "--assume-unchanged"] {
        let f = Fixture::new();
        wgit(&f.env, &f.wt, &["update-index", flag, "a.txt"]);
        fs::write(f.wt.join("a.txt"), "hidden edit\n").unwrap();
        assert_eq!(
            wgit(&f.env, &f.wt, &["status", "--porcelain"]),
            "",
            "{flag}"
        );
        let plan = f.json(&["remove", &f.wt.to_string_lossy()]);
        assert_eq!(plan["snapshot_files"], 1, "{flag}: {plan}");
        remove_and_restore(&f);
        assert_eq!(
            fs::read_to_string(f.wt.join("a.txt")).unwrap(),
            "hidden edit\n",
            "{flag}"
        );
    }
}

#[test]
fn sparse_checkout_files_that_are_absent_stay_in_the_backup() {
    let f = Fixture::new();
    wgit(
        &f.env,
        &f.wt,
        &["sparse-checkout", "set", "--no-cone", "/a.txt"],
    );
    assert!(!f.wt.join("b.txt").exists());
    fs::write(f.wt.join("a.txt"), "edited in sparse\n").unwrap();
    let removed = f.remove();
    assert_eq!(removed["snapshot_files"], 1, "{removed}");
    f.json(&["restore", removed["id"].as_str().unwrap()]);
    wgit(&f.env, &f.wt, &["sparse-checkout", "disable"]);
    assert_eq!(
        fs::read_to_string(f.wt.join("a.txt")).unwrap(),
        "edited in sparse\n"
    );
    assert_eq!(fs::read_to_string(f.wt.join("b.txt")).unwrap(), "b\n");
    assert_eq!(fs::read_to_string(f.wt.join("c.txt")).unwrap(), "c\n");
}

#[test]
fn regenerable_caches_never_block_removal_in_the_worktree_or_a_submodule() {
    let (f, sub) = with_submodule();
    ignore_in(&f, &sub, ".pytest_cache/\n__pycache__/");
    fs::write(
        f.wt.join(".gitignore"),
        ".env\n__pycache__/\n.tox/\n.next/\n",
    )
    .unwrap();
    wgit(&f.env, &f.wt, &["add", ".gitignore"]);
    wgit(&f.env, &f.wt, &["commit", "-q", "-m", "ignore caches"]);
    for dir in [
        "mod/.pytest_cache/v",
        "mod/pkg/__pycache__",
        ".tox/py",
        ".next/cache",
    ] {
        fs::create_dir_all(f.wt.join(dir)).unwrap();
        fs::write(f.wt.join(dir).join("x"), "x").unwrap();
    }
    let plan = f.json(&["remove", &f.wt.to_string_lossy()]);
    assert_eq!(plan["copied_ignored"], json!([]), "{plan}");
    let skipped = plan["skipped_disposable"].as_array().unwrap();
    assert!(skipped.iter().any(|s| s == ".tox/"), "{plan}");
    // A secret next to the caches still blocks.
    ignore_in(&f, &sub, ".env\n.pytest_cache/\n__pycache__/");
    fs::write(sub.join(".env"), "SECRET=1\n").unwrap();
    refuses_both_ways(&f, ".env");
}

#[test]
fn ignored_paths_are_listed_once() {
    let f = Fixture::new();
    fs::write(f.wt.join(".gitignore"), ".env\n__pycache__/\n*.local\n").unwrap();
    wgit(&f.env, &f.wt, &["add", ".gitignore"]);
    wgit(&f.env, &f.wt, &["commit", "-q", "-m", "ignore"]);
    // Untracked directories holding only ignored files: git lists the directory and its children.
    for dir in ["cfg/__pycache__", "cfg/deep"] {
        fs::create_dir_all(f.wt.join(dir)).unwrap();
    }
    fs::create_dir_all(f.wt.join("pkg/__pycache__")).unwrap();
    fs::write(f.wt.join("pkg/__pycache__/x.pyc"), "x").unwrap();
    fs::write(f.wt.join("cfg/__pycache__/x.local"), "x").unwrap();
    fs::write(f.wt.join("pkg/mod.py"), "m\n").unwrap();
    wgit(&f.env, &f.wt, &["add", "pkg/mod.py"]);
    fs::write(f.wt.join("cfg/a.local"), "a").unwrap();
    fs::write(f.wt.join("cfg/deep/b.local"), "b").unwrap();
    let plan = f.json(&["remove", &f.wt.to_string_lossy()]);
    for key in ["copied_ignored", "skipped_disposable"] {
        let mut listed: Vec<&str> = plan[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let count = listed.len();
        listed.sort_unstable();
        listed.dedup();
        assert_eq!(listed.len(), count, "{key}: {plan}");
    }
    assert!(
        !plan["copied_ignored"].as_array().unwrap().is_empty(),
        "{plan}"
    );
}

#[test]
fn a_worktree_named_with_repeated_lock_endings_backs_up_and_restores() {
    let f = Fixture::new();
    let odd = f.env.path("trees").join("x.lock.lock");
    worktree_at(&f, "odd", &odd);
    fs::write(odd.join("note.txt"), "note\n").unwrap();
    let removed = f.json(&["remove", &odd.to_string_lossy(), "--yes"]);
    let id = removed["id"].as_str().unwrap();
    assert!(!id.ends_with(".lock"), "{id}");
    let restored = f.json(&["restore", id]);
    assert_eq!(restored["warnings"], json!([]));
}
