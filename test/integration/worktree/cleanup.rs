//! `agt cleanup --only worktrees` against a local bare repository as origin (reached through
//! `insteadOf`, so `origin` reads as a GitHub or GitLab URL) and the fake `gh`/`glab`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::{Value, json};

use super::{BIN, proc_start};
use crate::support::Env;

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Site {
    env: Env,
    bare: PathBuf,
    /// How many days past the real time the run's clock is; worktrees made by a test are
    /// quiet for 3 days only once this is past 3.
    days_on: i64,
}

impl Site {
    fn new() -> Self {
        Self::at("git@github.com:acme/app.git")
    }

    /// A repository with one commit on `main`, pushed to a bare origin that `url` names.
    fn at(url: &str) -> Self {
        let env = Env::new();
        let bare = env.path("origin.git");
        env.git(&["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()]);
        env.write("base.txt", "base\n");
        env.commit_all("base");
        env.git(&["remote", "add", "origin", url]);
        env.git(&["config", &format!("url.{}.insteadOf", bare.display()), url]);
        env.git(&["push", "-q", "origin", "main"]);
        env.git(&["remote", "set-head", "origin", "main"]);
        Self {
            env,
            bare,
            days_on: 4,
        }
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let mut all = vec!["-C", dir.to_str().unwrap()];
        all.extend_from_slice(args);
        self.env.git(&all)
    }

    /// A branch with a worktree under `trees/` and one commit; returns the worktree.
    fn branch(&self, name: &str) -> PathBuf {
        let wt = self.env.path("trees").join(name);
        self.env
            .git(&["worktree", "add", "-q", "-b", name, wt.to_str().unwrap()]);
        self.commit(&wt, name);
        wt
    }

    fn commit(&self, dir: &Path, file: &str) -> String {
        fs::write(dir.join(file), file).unwrap();
        self.git(dir, &["add", "-A"]);
        self.git(dir, &["commit", "-q", "-m", file]);
        self.git(dir, &["rev-parse", "HEAD"])
    }

    fn tip(&self, branch: &str) -> String {
        self.env.git(&["rev-parse", branch])
    }

    /// Puts the work of `branch` on `main` as one new commit, as a squash merge does.
    fn squash(&self, branch: &str) {
        self.env.git(&["merge", "-q", "--squash", branch]);
        self.env
            .git(&["commit", "-q", "-m", &format!("squash {branch}")]);
    }

    fn branches(&self) -> Vec<String> {
        self.env
            .git(&["for-each-ref", "--format=%(refname:short)", "refs/heads"])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn agt_in(&self, dir: &Path, args: &[&str]) -> Output {
        let real = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let now = i64::try_from(real).unwrap() + self.days_on * 86_400;
        self.env
            .command(BIN, dir)
            .env("AGT_NOW", now.to_string())
            .args(["cleanup", "--only", "worktrees"])
            .args(args)
            .output()
            .unwrap()
    }

    /// The report of the one repository, after `agt cleanup --only worktrees <args>` in the main checkout.
    fn run(&self, args: &[&str]) -> Value {
        self.run_in(&self.env.repo, args)
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> Value {
        let output = self.agt_in(dir, args);
        assert!(
            output.status.success(),
            "agt cleanup --only worktrees {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        report["worktrees"]["repositories"][0].clone()
    }

    /// Answers the fake `gh` for the PRs of `branch`.
    fn pulls(&self, branch: &str, nodes: &[Value]) {
        self.pages(branch, None, nodes, None);
    }

    fn pages(&self, branch: &str, after: Option<&str>, nodes: &[Value], next: Option<&str>) {
        let branch = format!("branch={branch}");
        let mut args = vec![
            "api",
            "graphql",
            "-f",
            "owner=acme",
            "-f",
            "name=app",
            "-f",
            &branch,
            "-f",
            "query=LandedPulls",
        ];
        let cursor = after.map(|after| format!("cursor={after}"));
        if let Some(cursor) = &cursor {
            args.extend(["-f", cursor]);
        }
        let body = json!({"data": {"repository": {"pullRequests": {
            "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next},
            "nodes": nodes,
        }}}});
        self.env.respond(&args, &body.to_string());
    }

    fn kept<'a>(&self, report: &'a Value, branch: &str) -> &'a Value {
        report["kept"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["branch"] == branch)
            .unwrap_or_else(|| panic!("{branch} is not kept: {report}"))
    }
}

fn pull(number: u64, state: &str, base: &str, branch: &str, head: &str, cross: bool) -> Value {
    json!({
        "number": number,
        "url": format!("https://github.com/acme/app/pull/{number}"),
        "title": format!("Title of {number}"),
        "state": state,
        "mergedAt": (state == "MERGED").then_some("2026-01-01T00:00:00Z"),
        "closedAt": (state == "CLOSED").then_some("2026-02-03T04:05:06Z"),
        "baseRefName": base,
        "headRefName": branch,
        "headRefOid": head,
        "isCrossRepository": cross,
    })
}

fn merged(number: u64, branch: &str, head: &str) -> Value {
    pull(number, "MERGED", "main", branch, head, false)
}

#[test]
fn the_default_cleanup_pass_never_removes_a_landed_worktree() {
    let site = Site::new();
    let wt = site.branch("done");
    site.env.git(&["merge", "-q", "done"]);
    let output = site
        .env
        .command(BIN, &site.env.repo)
        .args(["cleanup", "--yes"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["worktrees"], Value::Null);
    assert!(wt.exists());
    site.env
        .git(&["rev-parse", "--verify", "--quiet", "refs/heads/done"]);
    // Worktree flags alone do not opt in.
    let output = site
        .env
        .command(BIN, &site.env.repo)
        .args(["cleanup", "--yes", "--dirty"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--dirty needs --only worktrees"),
        "{stderr}"
    );
    assert!(wt.exists());
    let output = site
        .env
        .command(BIN, &site.env.repo)
        .args(["cleanup", "--count", "--worktree", "done"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn the_manual_list_leaves_out_what_the_sweep_removed() {
    let site = Site::new();
    let wt = site.branch("done");
    site.env.git(&["merge", "-q", "done"]);
    // The manual list scans the configured roots, so name the repository there.
    let config = site.env.path("config/agt");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        format!("[worktree]\nroots = [\"{}\"]\n", site.env.repo.display()),
    )
    .unwrap();
    let output = site.agt_in(
        &site.env.repo,
        &["--only", "manual", "--offline", "--recent", "--yes"],
    );
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["worktrees"]["repositories"][0]["removed"][0]["branch"],
        "done"
    );
    assert!(!wt.exists());
    // The path may differ by symlink (`/var` against `/private/var`), so match its last part.
    let listed = report["manual"]["worktrees"].to_string();
    let name = wt.file_name().unwrap().to_str().unwrap();
    assert!(!listed.contains(&format!("/{name} ")), "{listed}");
}

#[test]
fn an_ancestor_merged_branch_loses_its_clean_worktree_and_branch() {
    let site = Site::new();
    let wt = site.branch("done");
    let tip = site.tip("done");
    // A disposable untracked file does not make the worktree dirty.
    fs::write(wt.join("debug.log"), "noise").unwrap();
    site.env.git(&["merge", "-q", "done"]);

    let report = site.run(&["--yes"]);
    assert_eq!(report["removed"][0]["path"], wt.to_str().unwrap());
    assert_eq!(report["removed"][0]["branch"], "done");
    let id = report["removed"][0]["id"].as_str().unwrap();
    assert!(!wt.exists());
    // The backup ref pins the tip even though the branch is gone.
    assert_eq!(
        site.env
            .git(&["rev-parse", &format!("refs/agt/backups/{id}^")]),
        tip
    );
    let deleted = &report["deleted"][0];
    assert_eq!(deleted["branch"], "done");
    assert_eq!(deleted["tip"], tip.as_str());
    assert_eq!(deleted["verdict"], "merged");
    assert_eq!(deleted["evidence"], "tip is an ancestor of main");
    assert_eq!(deleted["restore"], format!("git branch done {tip}"));
    assert_eq!(site.branches(), ["main"]);
    // Local ancestry needs no forge.
    assert_eq!(site.env.forge_calls(), Vec::<String>::new());
}

#[test]
fn a_squash_merged_branch_is_proven_by_a_merged_pull_request() {
    let site = Site::new();
    let wt = site.branch("sq");
    let tip = site.tip("sq");
    site.squash("sq");
    site.pulls("sq", &[merged(7, "sq", &tip)]);

    let report = site.run(&["--yes"]);
    assert!(!wt.exists());
    assert_eq!(report["removed"][0]["verdict"], "squash-merged");
    let deleted = &report["deleted"][0];
    assert_eq!(deleted["verdict"], "squash-merged");
    assert_eq!(deleted["tip"], tip.as_str());
    assert!(
        deleted["evidence"]
            .as_str()
            .unwrap()
            .contains("merged PR #7 into main"),
        "{deleted}"
    );
    assert_eq!(deleted["restore"], format!("git branch sq {tip}"));
    assert_eq!(site.branches(), ["main"]);
}

#[test]
fn a_merged_head_missing_locally_is_fetched_through_the_pull_ref() {
    let site = Site::new();
    let wt = site.branch("far");
    let tip = site.tip("far");
    // The pull request's head is one commit past the local branch, and only origin has it.
    site.git(&wt, &["push", "-q", "origin", "far"]);
    let clone = site.env.path("clone");
    site.env.git(&[
        "clone",
        "-q",
        site.bare.to_str().unwrap(),
        clone.to_str().unwrap(),
    ]);
    site.git(&clone, &["checkout", "-q", "far"]);
    let head = site.commit(&clone, "more");
    site.git(&clone, &["push", "-q", "origin", "HEAD:refs/pull/3/head"]);
    site.squash("far");
    // A second branch whose recorded head is on no pull ref: the fetch cannot confirm it.
    site.branch("lost");
    site.pulls("far", &[merged(3, "far", &head)]);
    site.pulls("lost", &[merged(9, "lost", SHA)]);
    assert!(!site.env.git_status(&["cat-file", "-e", &head]));

    let report = site.run(&["--yes"]);
    assert!(!wt.exists());
    assert_eq!(report["deleted"][0]["branch"], "far");
    assert_eq!(report["deleted"][0]["tip"], tip.as_str());
    assert_eq!(site.branches(), ["lost", "main"]);
    let lost = site.kept(&report, "lost");
    assert_eq!(lost["verdict"], "unknown");
    assert!(lost["reason"].as_str().unwrap().contains("PR #9"), "{lost}");
}

#[test]
fn a_branch_past_the_merged_pull_request_head_is_diverged() {
    let site = Site::new();
    let wt = site.branch("past");
    let head = site.tip("past");
    site.squash("past");
    site.commit(&wt, "later");
    site.pulls("past", &[merged(4, "past", &head)]);

    let report = site.run(&["--yes"]);
    let row = site.kept(&report, "past");
    assert_eq!(row["verdict"], "diverged");
    assert_eq!(row["reason"], "diverged from merged PR #4");
    assert!(wt.exists());
    assert_eq!(site.branches(), ["main", "past"]);
}

#[test]
fn open_closed_and_cross_repository_pull_requests_keep_the_branch() {
    let site = Site::new();
    for name in ["open", "closed", "fork"] {
        site.branch(name);
    }
    let (open, closed, fork) = (site.tip("open"), site.tip("closed"), site.tip("fork"));
    // An older merged PR does not outweigh the open one.
    site.pulls(
        "open",
        &[
            merged(1, "open", &open),
            pull(2, "OPEN", "main", "open", &open, false),
        ],
    );
    site.pulls(
        "closed",
        &[pull(5, "CLOSED", "main", "closed", &closed, false)],
    );
    site.pulls("fork", &[pull(6, "MERGED", "main", "fork", &fork, true)]);

    let report = site.run(&["--yes"]);
    assert_eq!(report["removed"], json!([]));
    assert_eq!(report["deleted"], json!([]));
    assert_eq!(site.kept(&report, "open")["verdict"], "open");
    let closed = site.kept(&report, "closed");
    assert_eq!(closed["verdict"], "closed");
    assert_eq!(closed["head"], site.tip("closed").as_str());
    assert_eq!(closed["title"], "Title of 5");
    assert_eq!(closed["closed_at"], "2026-02-03T04:05:06Z");
    assert!(site.kept(&report, "open").get("title").is_none());
    let reason = closed["reason"].as_str().unwrap();
    assert!(
        reason.contains("#5") && reason.contains("/pull/5"),
        "{reason}"
    );
    let fork = site.kept(&report, "fork");
    assert_eq!(fork["verdict"], "unmerged");
    assert!(
        fork["reason"]
            .as_str()
            .unwrap()
            .contains("cross-repository"),
        "{fork}"
    );
    assert_eq!(site.branches(), ["closed", "fork", "main", "open"]);
}

#[test]
fn pull_requests_are_read_to_the_last_page() {
    let site = Site::new();
    site.branch("late");
    let tip = site.tip("late");
    site.squash("late");
    let old = pull(1, "CLOSED", "main", "late", SHA, false);
    site.pages("late", None, &[old], Some("c1"));
    site.pages("late", Some("c1"), &[merged(2, "late", &tip)], None);

    let report = site.run(&["--yes"]);
    assert_eq!(report["deleted"][0]["verdict"], "squash-merged");
}

#[test]
fn a_gitlab_merge_request_proves_a_squash_and_a_cross_project_one_does_not() {
    let site = Site::at("git@gitlab.com:acme/app.git");
    site.branch("sq");
    site.branch("fork");
    let (sq, fork) = (site.tip("sq"), site.tip("fork"));
    site.squash("sq");
    site.squash("fork");
    let mr = |iid: u64, branch: &str, head: &str, source: u64| {
        json!([{
            "iid": iid,
            "web_url": format!("https://gitlab.com/acme/app/-/merge_requests/{iid}"),
            "state": "merged",
            "merged_at": "2026-01-01T00:00:00Z",
            "target_branch": "main",
            "source_branch": branch,
            "sha": head,
            "source_project_id": source,
            "target_project_id": 10,
        }])
        .to_string()
    };
    let path = |branch: &str| {
        format!("projects/acme%2Fapp/merge_requests?source_branch={branch}&state=all&per_page=100")
    };
    site.env.respond(
        &["api", "-X", "GET", "--paginate", &path("sq")],
        &mr(11, "sq", &sq, 10),
    );
    site.env.respond(
        &["api", "-X", "GET", "--paginate", &path("fork")],
        &mr(12, "fork", &fork, 99),
    );

    let report = site.run(&["--yes"]);
    assert_eq!(report["deleted"][0]["branch"], "sq");
    assert_eq!(report["deleted"][0]["verdict"], "squash-merged");
    assert_eq!(site.kept(&report, "fork")["verdict"], "unmerged");
    assert_eq!(site.branches(), ["fork", "main"]);
}

#[test]
fn a_dirty_worktree_stays_unless_dirty_is_passed() {
    let site = Site::new();
    let wt = site.branch("wip");
    site.env.git(&["merge", "-q", "wip"]);
    fs::write(wt.join("wip"), "changed").unwrap();

    let report = site.run(&["--yes"]);
    let row = site.kept(&report, "wip");
    assert_eq!(row["verdict"], "merged");
    assert!(
        row["reason"].as_str().unwrap().starts_with("dirty"),
        "{row}"
    );
    assert!(wt.exists());
    assert_eq!(site.branches(), ["main", "wip"]);

    let report = site.run(&["--yes", "--dirty"]);
    assert!(!wt.exists());
    let id = report["removed"][0]["id"].as_str().unwrap();
    // The snapshot holds the uncommitted change.
    assert_eq!(
        site.env
            .git(&["show", &format!("refs/agt/backups/{id}:wip")]),
        "changed"
    );
    assert_eq!(site.branches(), ["main"]);
}

#[test]
fn a_live_agent_session_keeps_the_worktree() {
    let site = Site::new();
    let wt = site.branch("busy");
    site.env.git(&["merge", "-q", "busy"]);
    // Another session's process: the test process is agt's parent, so a record for it would be
    // the calling session, which never blocks.
    let other = super::Reaped::new(
        site.env
            .command("sleep", &site.env.repo)
            .arg("60")
            .spawn()
            .unwrap(),
    );
    let me = other.0.id();
    let sessions = site.env.path("home/.claude/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join(format!("{me}.json")),
        json!({"pid": me, "cwd": wt, "procStart": proc_start(me)}).to_string(),
    )
    .unwrap();

    let report = site.run(&["--yes"]);
    let row = site.kept(&report, "busy");
    assert!(row["reason"].as_str().unwrap().contains("session"), "{row}");
    assert!(wt.exists());
    assert_eq!(site.branches(), ["busy", "main"]);
}

#[test]
fn a_process_running_inside_the_worktree_keeps_it() {
    let site = Site::new();
    let wt = site.branch("running");
    site.env.git(&["merge", "-q", "running"]);
    let mut child = site.env.command("sleep", &wt).arg("30").spawn().unwrap();
    // Give lsof or /proc time to see the child's directory.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let report = site.run(&["--yes"]);
    child.kill().unwrap();
    child.wait().unwrap();
    let row = site.kept(&report, "running");
    assert!(row["reason"].as_str().unwrap().contains("runs in"), "{row}");
    assert!(wt.exists());
}

#[test]
fn offline_skips_the_forge() {
    let site = Site::new();
    let wt = site.branch("sq");
    let tip = site.tip("sq");
    site.squash("sq");
    site.pulls("sq", &[merged(7, "sq", &tip)]);

    let report = site.run(&["--yes", "--offline"]);
    assert_eq!(site.kept(&report, "sq")["verdict"], "unmerged");
    assert!(wt.exists());
    assert_eq!(site.env.forge_calls(), Vec::<String>::new());
}

#[test]
fn a_failed_forge_call_is_unknown_and_keeps_everything() {
    let site = Site::new();
    site.branch("sq");
    site.squash("sq");

    let report = site.run(&["--yes"]);
    let row = site.kept(&report, "sq");
    assert_eq!(row["verdict"], "unknown");
    assert_eq!(site.branches(), ["main", "sq"]);
}

#[test]
fn without_yes_the_plan_names_every_action_and_nothing_changes() {
    let site = Site::new();
    let wt = site.branch("done");
    site.env.git(&["merge", "-q", "done"]);
    let sq = site.branch("sq");
    let tip = site.tip("sq");
    site.squash("sq");
    site.pulls("sq", &[merged(7, "sq", &tip)]);

    let output = site.agt_in(&site.env.repo, &[]);
    assert!(output.status.success());
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["worktrees"]["plan"], true);
    let report = &plan["worktrees"]["repositories"][0];
    assert_eq!(report["remove"].as_array().unwrap().len(), 2);
    let actions: Vec<&str> = report["delete"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, ["git branch -d done", "git branch -D sq"]);
    assert!(
        report["remove"][0]["action"]
            .as_str()
            .unwrap()
            .contains("git worktree remove --force")
    );
    assert!(wt.exists() && sq.exists());
    assert_eq!(site.branches(), ["done", "main", "sq"]);
    assert_eq!(site.env.git(&["for-each-ref", "refs/agt"]), "");
    assert!(!site.env.path("state/agents/backups").exists());
}

#[test]
fn the_default_the_main_the_current_branch_and_its_worktree_are_never_touched() {
    let site = Site::new();
    let current = site.branch("current");
    site.branch("side");
    site.env.git(&["merge", "-q", "current"]);
    site.env.git(&["merge", "-q", "side"]);
    // The main checkout is on a merged branch of its own.
    site.env.git(&["checkout", "-q", "-b", "trunk-copy"]);

    let report = site.run_in(&current, &["--yes"]);
    assert!(current.exists());
    assert_eq!(report["deleted"][0]["branch"], "side");
    assert_eq!(report["deleted"].as_array().unwrap().len(), 1);
    assert_eq!(site.branches(), ["current", "main", "trunk-copy"]);
}

#[test]
fn all_sweeps_every_configured_repository() {
    let site = Site::new();
    let wt = site.branch("done");
    site.env.git(&["merge", "-q", "done"]);
    let config = site.env.path("config/agt");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        format!("[worktree]\nroots = [\"{}\"]\n", site.env.repo.display()),
    )
    .unwrap();

    let report = site.run_in(&site.env.path("home"), &["--all-repos", "--yes"]);
    assert_eq!(report["repository"], site.env.repo.to_str().unwrap());
    assert_eq!(report["removed"][0]["path"], wt.to_str().unwrap());
    assert_eq!(site.branches(), ["main"]);
}

/// A worktree on a new branch at the default tip, as `agt worktree create` leaves it.
fn fresh(site: &Site) -> PathBuf {
    let wt = site.env.path("trees/fresh");
    site.env
        .git(&["worktree", "add", "-q", "-b", "fresh", wt.to_str().unwrap()]);
    wt
}

#[test]
fn a_worktree_changed_in_the_last_3_days_is_recent_unless_recent_is_passed() {
    let mut site = Site::new();
    let wt = fresh(&site);
    site.days_on = 0;

    let report = site.run(&["--yes"]);
    let row = site.kept(&report, "fresh");
    assert_eq!(row["verdict"], "recent");
    assert!(wt.exists());
    assert_eq!(site.branches(), ["fresh", "main"]);

    let report = site.run(&["--yes", "--recent"]);
    assert!(!wt.exists());
    assert_eq!(report["removed"][0]["branch"], "fresh");
    assert_eq!(site.branches(), ["main"]);
}

#[test]
fn a_worktree_quiet_for_4_days_is_removed() {
    let site = Site::new();
    let wt = fresh(&site);
    let report = site.run(&["--yes"]);
    assert!(!wt.exists());
    assert_eq!(report["removed"][0]["branch"], "fresh");
}

#[test]
fn a_nested_repository_keeps_its_worktree_even_with_dirty() {
    let site = Site::new();
    let wt = site.branch("wip");
    site.env.git(&["merge", "-q", "wip"]);
    let nested = wt.join("vendor/lib");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("lib.txt"), "lib").unwrap();
    site.git(&nested, &["init", "-q"]);
    site.git(&nested, &["add", "-A"]);
    site.git(&nested, &["commit", "-q", "-m", "lib"]);

    for args in [&["--yes"][..], &["--yes", "--dirty"]] {
        let report = site.run(args);
        let row = site.kept(&report, "wip");
        let reason = row["reason"].as_str().unwrap();
        assert!(reason.contains("vendor/lib"), "{row}");
        assert!(reason.contains("nested"), "{row}");
        assert!(nested.join(".git").exists());
        assert_eq!(site.branches(), ["main", "wip"]);
    }
}

#[test]
fn all_warns_about_an_unreadable_repository_and_sweeps_the_rest() {
    let site = Site::new();
    let wt = site.branch("done");
    site.env.git(&["merge", "-q", "done"]);
    let broken = site.env.path("home/code/broken");
    fs::create_dir_all(broken.join(".git")).unwrap();
    let config = site.env.path("config/agt");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("config.toml"),
        format!(
            "[worktree]\nroots = [\"{}\", \"{}\"]\n",
            broken.display(),
            site.env.repo.display()
        ),
    )
    .unwrap();

    let output = site.agt_in(&site.env.path("home"), &["--all-repos", "--yes"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let rows = report["worktrees"]["repositories"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{report}");
    let bad = rows
        .iter()
        .find(|row| row["repository"].as_str().unwrap().ends_with("broken"))
        .unwrap();
    assert_eq!(bad["warnings"].as_array().unwrap().len(), 1, "{report}");
    assert!(!wt.exists());
    assert_eq!(site.branches(), ["main"]);
}

#[test]
fn main_master_and_develop_are_protected_by_name() {
    let site = Site::new();
    site.env.git(&["branch", "develop"]);
    site.env.git(&["branch", "master"]);
    site.env.git(&["branch", "other"]);

    let report = site.run(&["--yes"]);
    assert_eq!(report["deleted"][0]["branch"], "other");
    assert_eq!(report["deleted"].as_array().unwrap().len(), 1);
    assert_eq!(site.branches(), ["develop", "main", "master"]);
}

#[test]
fn a_closed_gitlab_merge_request_row_carries_head_title_and_closed_at() {
    let site = Site::at("git@gitlab.com:acme/app.git");
    site.branch("shelved");
    let head = site.tip("shelved");
    let mr = json!([{
        "iid": 7,
        "web_url": "https://gitlab.com/acme/app/-/merge_requests/7",
        "title": "Shelve it",
        "state": "closed",
        "merged_at": null,
        "closed_at": "2026-03-04T05:06:07Z",
        "target_branch": "main",
        "source_branch": "shelved",
        "sha": head,
        "source_project_id": 10,
        "target_project_id": 10,
    }]);
    site.env.respond(
        &[
            "api",
            "-X",
            "GET",
            "--paginate",
            "projects/acme%2Fapp/merge_requests?source_branch=shelved&state=all&per_page=100",
        ],
        &mr.to_string(),
    );
    let report = site.run(&["--yes"]);
    let row = site.kept(&report, "shelved");
    assert_eq!(row["verdict"], "closed");
    assert_eq!(row["head"], head.as_str());
    assert_eq!(row["title"], "Shelve it");
    assert_eq!(row["closed_at"], "2026-03-04T05:06:07Z");
}

fn full(site: &Site, args: &[&str]) -> Value {
    let output = site.agt_in(&site.env.repo, args);
    assert!(
        output.status.success(),
        "agt cleanup --only worktrees {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    report["worktrees"].clone()
}

#[test]
fn only_acts_on_the_named_rows_and_skips_the_rest_with_a_reason() {
    let site = Site::new();
    let a = site.branch("a");
    let b = site.branch("b");
    let dirty = site.branch("dirty");
    for name in ["a", "b", "dirty"] {
        site.env.git(&["merge", "-q", name]);
    }
    fs::write(dirty.join("dirty"), "changed").unwrap();

    // The plan lists the named row alone and changes nothing.
    let plan = full(&site, &["--worktree", "a"]);
    let row = &plan["repositories"][0];
    assert_eq!(row["remove"].as_array().unwrap().len(), 1, "{plan}");
    assert_eq!(row["remove"][0]["branch"], "a");
    assert_eq!(row["delete"].as_array().unwrap().len(), 1, "{plan}");
    assert!(a.exists() && b.exists());

    // A path names a row too; a named row that no longer qualifies is skipped, and a name that
    // matches no row is reported.
    let report = full(
        &site,
        &[
            "--yes",
            "--worktree",
            "a",
            "--worktree",
            "dirty",
            "--worktree",
            "no-such-branch",
        ],
    );
    let row = &report["repositories"][0];
    assert_eq!(row["removed"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(row["removed"][0]["branch"], "a");
    assert_eq!(row["deleted"][0]["branch"], "a");
    assert_eq!(row["skipped"][0]["branch"], "dirty");
    assert!(
        row["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("dirty"),
        "{report}"
    );
    assert!(row.get("kept").is_none());
    assert_eq!(report["skipped"][0]["name"], "no-such-branch");
    assert!(!a.exists());
    // Unnamed rows are never touched.
    assert!(b.exists() && dirty.exists());
    assert_eq!(site.branches(), ["b", "dirty", "main"]);

    let report = full(&site, &["--yes", "--worktree", b.to_str().unwrap()]);
    assert_eq!(report["repositories"][0]["removed"][0]["branch"], "b");
    assert_eq!(report["skipped"], json!([]));
    assert!(!b.exists() && dirty.exists());
    assert_eq!(site.branches(), ["dirty", "main"]);
}

/// A directory made unreadable, readable again when dropped so the temporary tree can go.
struct Sealed(PathBuf);

impl Sealed {
    fn new(dir: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir_all(dir).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
        Self(dir.to_owned())
    }
}

impl Drop for Sealed {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn an_unreadable_directory_keeps_its_worktree_and_the_pass_goes_on() {
    let site = Site::new();
    site.branch("a-first");
    let sealed = site.branch("z-sealed");
    for name in ["a-first", "z-sealed"] {
        site.env.git(&["merge", "-q", name]);
    }
    let _sealed = Sealed::new(&sealed.join("locked"));

    let report = site.run(&["--yes", "--dirty"]);
    assert_eq!(report["deleted"][0]["branch"], "a-first", "{report}");
    let row = site.kept(&report, "z-sealed");
    assert!(row["reason"].as_str().unwrap().contains("locked"), "{row}");
    assert!(sealed.exists());
    assert_eq!(site.branches(), ["main", "z-sealed"]);
}

#[test]
fn the_calling_sessions_own_process_in_a_landed_worktree_is_a_warning_not_a_block() {
    let site = Site::new();
    let wt = site.branch("done");
    site.env.git(&["merge", "-q", "done"]);
    // The test process is agt's parent: the calling session. Its child holds the worktree.
    let me = std::process::id();
    let sessions = site.env.path("home/.claude/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join(format!("{me}.json")),
        json!({"pid": me, "cwd": site.env.repo, "procStart": proc_start(me)}).to_string(),
    )
    .unwrap();
    let _child = super::Reaped::new(site.env.command("sleep", &wt).arg("60").spawn().unwrap());
    std::thread::sleep(std::time::Duration::from_millis(400));

    let report = site.run(&["--yes"]);
    assert_eq!(
        report["removed"][0]["path"],
        wt.to_str().unwrap(),
        "{report}"
    );
    assert!(!wt.exists());
    let warnings = report["warnings"].as_array().unwrap();
    assert!(
        warnings.iter().any(|warning| {
            let text = warning.as_str().unwrap();
            text.contains("(sleep)") && text.contains("calling session")
        }),
        "{report}"
    );
}
