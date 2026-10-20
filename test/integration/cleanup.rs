//! `agt cleanup`: every rule deletes what it should and keeps its near miss. The clock is the
//! `AGT_NOW` override, so no test waits.

use std::fs::{self, File};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::support::{Env, Sealed, proc_start, unix_now};

const DAY: i64 = 86_400;
/// 2027-01-01T00:00:00Z.
const NOW: i64 = 1_798_761_600;

/// `NOW` minus `days`, as an RFC 3339 timestamp.
fn ago(days: i64) -> String {
    iso(NOW - days * DAY)
}

fn iso(secs: i64) -> String {
    let (days, rest) = (secs.div_euclid(DAY), secs.rem_euclid(DAY));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

struct Prune {
    env: Env,
}

impl Prune {
    fn new() -> Self {
        Self { env: Env::new() }
    }

    fn agents(&self) -> PathBuf {
        self.env.path("state").join("agents")
    }

    fn write(&self, path: &Path, value: &Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    fn target(&self, repo: &str, key: &str, age: i64) -> PathBuf {
        let path = self
            .agents()
            .join("reviews")
            .join(repo)
            .join(format!("{key}.json"));
        self.write(
            &path,
            &json!({
                "version": 1, "revision": 1, "key": key, "repo": repo, "kind": "branch",
                "branch": key, "branch_birth": null, "base": null, "head": null,
                "created_at": ago(age), "updated_at": ago(age), "review": null,
                "runs": [], "records": [], "threads": [],
            }),
        );
        path
    }

    fn issue(&self, name: &str, age: i64, state: &str, edit: impl FnOnce(&mut Value)) -> PathBuf {
        let fields = json!({
            "title": "t", "body": "b", "state": state, "reason": null,
            "labels": [], "assignees": [],
        });
        let mut issue = json!({
            "version": 1, "key": format!("o/r#{name}"), "tracker": "gh-o-r",
            "number": name.parse::<u64>().unwrap(),
            "remote": {"id": "I_1", "url": "u", "updated_at": ago(age)},
            "fields": fields, "base": fields, "author": null, "native_state": null,
            "milestone": null, "due_date": null,
            "created_at": ago(age), "updated_at": ago(age), "synced_at": ago(age),
            "comments": [], "links": {"prs": [], "issues": [], "reviews": [], "tasks": [], "branches": []},
            "notes": [], "conflicts": [], "errors": [], "create": null,
        });
        edit(&mut issue);
        let path = self
            .agents()
            .join("issues")
            .join("gh-o-r")
            .join(format!("{name}.json"));
        self.write(&path, &issue);
        path
    }

    fn task(&self, id: &str, status: &str, age: i64) -> PathBuf {
        let path = self.agents().join("tasks").join(format!("{id}.json"));
        self.write(
            &path,
            &json!({
                "version": 1, "id": id, "revision": 1, "objective": "o", "status": status,
                "createdAt": ago(age + 1), "updatedAt": ago(age),
            }),
        );
        path
    }

    fn backup(&self, name: &str) -> PathBuf {
        let path = self.agents().join("backups").join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("copy.json"), "{}").unwrap();
        path
    }

    /// Runs until nothing is skipped as locked, for a lock the test just released: a lock is an
    /// open file description, and a thread that forks a child (other tests spawn `agt`) hands it
    /// to the child until the child's `exec` closes it, so a released lock can stay held briefly.
    fn run_settled(&self, args: &[&str]) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let report = self.run(args);
            let skipped = report["skipped"].as_array().is_some_and(|s| !s.is_empty());
            if !skipped || std::time::Instant::now() > deadline {
                return report;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn run(&self, args: &[&str]) -> Value {
        self.run_at(NOW, args)
    }

    fn run_at(&self, now: i64, args: &[&str]) -> Value {
        let output = self
            .env
            .bin(&self.env.repo)
            .arg("cleanup")
            .args(args)
            .env("AGT_NOW", now.to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "agt cleanup {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn keys(report: &Value, category: &str) -> Vec<String> {
    let mut found: Vec<String> = report["plan"][category]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["key"].as_str().unwrap().to_owned())
        .collect();
    found.sort();
    found
}

/// The stamp agt gives a backup taken `days` ago.
fn stamp(days: i64) -> String {
    iso(NOW - days * DAY).replace(':', "-")
}

fn snapshot(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            found.push(entry.path());
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            }
        }
    }
    found.sort();
    found
}

#[test]
fn reviews_older_than_90_days_go_in_every_repo() {
    let prune = Prune::new();
    let old = prune.target("github.com/o/a", "old", 91);
    let near = prune.target("github.com/o/a", "near", 89);
    let other = prune.target("github.com/o/b", "other-old", 120);
    let report = prune.run(&["--yes"]);
    assert_eq!(
        keys(&report, "reviews"),
        ["github.com/o/a/old", "github.com/o/b/other-old"]
    );
    assert!(!old.exists() && !other.exists() && near.exists());
    assert_eq!(report["dry_run"], false);
    assert_eq!(report["totals"]["items"], 2);
    assert!(report["bytes_freed"].as_u64().unwrap() > 0);
    assert_eq!(report["deleted"].as_array().unwrap().len(), 2);
    assert_eq!(report["skipped"], json!([]));
}

#[test]
fn local_repos_are_listed_only_when_every_target_expired() {
    let prune = Prune::new();
    let all_old = prune.target("local/gone-aaaaaaaaaaaa", "t", 100);
    let account = all_old.with_file_name(".account.json");
    fs::write(&account, "{}").unwrap();
    let mixed_old = prune.target("local/live-bbbbbbbbbbbb", "old", 100);
    let mixed_new = prune.target("local/live-bbbbbbbbbbbb", "new", 3);
    let report = prune.run(&[]);
    assert_eq!(keys(&report, "local_repos"), ["local/gone-aaaaaaaaaaaa"]);
    assert!(
        report["plan"]["local_repos"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("cannot be checked")
    );
    assert!(all_old.exists());
    prune.run(&["--yes"]);
    assert!(!all_old.parent().unwrap().exists());
    assert!(!mixed_old.exists() && mixed_new.exists());
}

#[test]
fn issues_drop_only_when_closed_and_holding_nothing_local() {
    let prune = Prune::new();
    let old = prune.issue("1", 91, "closed", |_| {});
    let near = prune.issue("2", 89, "closed", |_| {});
    let open = prune.issue("3", 400, "open", |_| {});
    let noted = prune.issue("4", 400, "closed", |issue| {
        issue["notes"] = json!([{"id": "n1", "text": "mine", "created_at": ago(400)}]);
    });
    let queued = prune.issue("5", 400, "closed", |issue| {
        issue["fields"]["title"] = json!("renamed");
    });
    let unsent = prune.issue("6", 400, "closed", |issue| {
        issue["comments"] = json!([{
            "id": "c1", "author": {"kind": "human", "login": "a"}, "body": "b",
            "created_at": ago(400), "post": "pending", "attempt": null, "sent": null,
            "remote": null, "draft_id": null, "local": false, "deleted_remote": false,
            "deleted_seen_at": null,
        }]);
    });
    let conflicted = prune.issue("7", 400, "closed", |issue| {
        let fields = issue["fields"].clone();
        issue["conflicts"] = json!([{
            "field": "title", "local": fields, "remote": fields, "seen_at": ago(400),
        }]);
    });
    let tracker = old.with_file_name("tracker.json");
    fs::write(&tracker, "{}").unwrap();
    let report = prune.run(&[]);
    assert_eq!(keys(&report, "issues"), ["o/r#1"]);
    assert!(old.exists());
    prune.run(&["--yes"]);
    assert!(!old.exists());
    for kept in [near, open, noted, queued, unsent, conflicted, tracker] {
        assert!(kept.exists(), "{}", kept.display());
    }
}

#[test]
fn tasks_drop_when_complete_for_90_days_and_the_index_stays() {
    let prune = Prune::new();
    let old = prune.task("done-old", "complete", 91);
    let near = prune.task("done-near", "complete", 89);
    let active = prune.task("active", "active", 400);
    let paused = prune.task("paused", "paused", 400);
    let index = prune.agents().join("tasks/session-index/index.json");
    fs::create_dir_all(index.parent().unwrap()).unwrap();
    fs::write(&index, "{}").unwrap();
    let report = prune.run(&["--yes"]);
    assert_eq!(keys(&report, "tasks"), ["done-old"]);
    assert!(!old.exists());
    for kept in [near, active, paused, index] {
        assert!(kept.exists(), "{}", kept.display());
    }
}

#[test]
fn stray_locks_go_unless_held_or_their_document_exists() {
    let prune = Prune::new();
    let live = prune.target("github.com/o/a", "live", 1);
    let live_lock = live.with_file_name(".live.lock");
    let live_publish = live.with_file_name(".live.publish.lock");
    let stray = live.with_file_name(".gone.lock");
    let stray_publish = live.with_file_name(".gone.publish.lock");
    let held = live.with_file_name(".busy.lock");
    for path in [&live_lock, &live_publish, &stray, &stray_publish, &held] {
        File::create(path).unwrap();
    }
    let guard = File::options().write(true).open(&held).unwrap();
    guard.lock().unwrap();
    let report = prune.run(&["--yes"]);
    assert!(!stray.exists() && !stray_publish.exists());
    for kept in [&live_lock, &live_publish, &held, &live] {
        assert!(kept.exists(), "{}", kept.display());
    }
    let skipped = report["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert_eq!(skipped[0]["key"], ".busy.lock");
    assert!(skipped[0]["why"].as_str().unwrap().contains("locked"));
    drop(guard);
}

#[test]
fn backups_keep_the_newest_per_namespace_and_foreign_names() {
    let prune = Prune::new();
    let oldest = prune.backup(&format!("reviews-v1-{}", stamp(200)));
    let older = prune.backup(&format!("reviews-v1-{}-2", stamp(100)));
    let newest_old = prune.backup(&format!("reviews-v1-{}", stamp(95)));
    let only_issues = prune.backup(&format!("issues-v1-{}", stamp(300)));
    let recent = prune.backup(&format!("tasks-v1-{}", stamp(10)));
    let tasks_old = prune.backup(&format!("tasks-v1-{}", stamp(120)));
    let foreign = [
        prune.backup("some-repo"),
        prune.backup(&format!("reviews-{}", stamp(400))),
        prune.backup(&format!("reviews-v1-{}-1", stamp(400))),
        prune.backup(&format!("reviews-v1-{}.bak", stamp(400))),
        prune.backup(&format!(".other-{}.partial", stamp(400))),
    ];
    let partial = prune.backup(&format!(".issues-v1-{}.partial", stamp(1)));
    let report = prune.run(&["--only", "backups"]);
    assert_eq!(report["totals"]["by_category"]["backups"]["items"], 3);
    assert!(oldest.exists());
    prune.run(&["--yes", "--only", "backups"]);
    for gone in [&oldest, &older, &tasks_old, &partial] {
        assert!(!gone.exists(), "{}", gone.display());
    }
    for kept in [&newest_old, &only_issues, &recent] {
        assert!(kept.exists(), "{}", kept.display());
    }
    for kept in &foreign {
        assert!(kept.exists(), "{}", kept.display());
    }
}

#[test]
fn a_backup_in_progress_is_skipped() {
    let prune = Prune::new();
    let partial = prune.backup(&format!(".reviews-v1-{}.partial", stamp(1)));
    let lock = prune.agents().join(".reviews-backup.lock");
    File::create(&lock).unwrap();
    let guard = File::options().write(true).open(&lock).unwrap();
    guard.lock().unwrap();
    let report = prune.run(&["--yes"]);
    assert!(partial.exists());
    assert_eq!(report["skipped"].as_array().unwrap().len(), 1);
    drop(guard);
    prune.run_settled(&["--yes"]);
    assert!(!partial.exists());
}

#[test]
fn a_target_held_by_another_run_is_skipped() {
    let prune = Prune::new();
    let old = prune.target("github.com/o/a", "busy", 120);
    let lock = old.with_file_name(".busy.lock");
    File::create(&lock).unwrap();
    let guard = File::options().write(true).open(&lock).unwrap();
    guard.lock().unwrap();
    let report = prune.run(&["--yes"]);
    assert!(old.exists());
    assert_eq!(report["deleted"], json!([]));
    assert!(
        report["skipped"][0]["why"]
            .as_str()
            .unwrap()
            .contains("locked")
    );
    drop(guard);
}

#[test]
fn a_dry_run_deletes_nothing_and_only_limits_the_pass() {
    let prune = Prune::new();
    prune.target("github.com/o/a", "old", 120);
    prune.target("local/gone-cccccccccccc", "t", 120);
    prune.issue("1", 120, "closed", |_| {});
    prune.task("t1", "complete", 120);
    prune.backup(&format!("reviews-v1-{}", stamp(200)));
    prune.backup(&format!("reviews-v1-{}", stamp(150)));
    File::create(prune.agents().join("reviews/github.com/o/a/.nobody.lock")).unwrap();
    let before = snapshot(&prune.agents());
    let report = prune.run(&[]);
    assert_eq!(snapshot(&prune.agents()), before);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["retention_days"], 90);
    assert_eq!(report["totals"]["items"], 7);
    assert!(report.get("deleted").is_none());
    for category in [
        "reviews",
        "local_repos",
        "issues",
        "tasks",
        "locks",
        "backups",
    ] {
        assert_eq!(
            report["totals"]["by_category"][category]["items"],
            report["plan"][category].as_array().unwrap().len(),
            "{category}"
        );
    }
    let only = prune.run(&["--only", "issues", "--only", "tasks"]);
    assert_eq!(only["totals"]["items"], 2);
}

fn len(path: &Path) -> u64 {
    fs::symlink_metadata(path).unwrap().len()
}

fn local_issue_links(issue: &mut Value, field: &str) {
    issue["links"][field] = json!(["x"]);
}

#[test]
fn issues_with_local_only_data_are_kept() {
    let prune = Prune::new();
    let pr = |source: &str| {
        json!({
            "forge": "github", "repo": "o/r", "number": 9, "url": null, "branch": null,
            "state": null, "source": source,
        })
    };
    let comment = |edit: &dyn Fn(&mut Value)| {
        let mut comment = json!({
            "id": "c1", "author": {"kind": "human", "login": "a"}, "body": "b",
            "created_at": ago(400), "post": "posted", "attempt": null, "sent": null,
            "remote": {"id": "9", "url": null, "login": null, "is_me": false, "editor": null, "review_id": null}, "draft_id": null, "local": false, "deleted_remote": false,
            "deleted_seen_at": null,
        });
        edit(&mut comment);
        json!([comment])
    };
    let removable = prune.issue("1", 400, "closed", |issue| {
        issue["links"]["prs"] = json!([pr("remote")]);
        issue["comments"] = comment(&|_| {});
    });
    let mut kept = Vec::new();
    for (i, field) in ["issues", "reviews", "tasks", "branches"]
        .into_iter()
        .enumerate()
    {
        kept.push(prune.issue(&format!("{}", 10 + i), 400, "closed", |issue| {
            local_issue_links(issue, field);
        }));
    }
    kept.push(prune.issue("20", 400, "closed", |issue| {
        issue["links"]["prs"] = json!([pr("local")]);
    }));
    kept.push(prune.issue("21", 400, "closed", |issue| {
        issue["comments"] = comment(&|c| c["deleted_remote"] = json!(true));
    }));
    kept.push(prune.issue("22", 400, "closed", |issue| {
        issue["comments"] = comment(&|c| c["local"] = json!(true));
    }));
    kept.push(prune.issue("23", 400, "closed", |issue| {
        issue["errors"] =
            json!([{"what": "create", "message": "m", "refused": false, "at": ago(400)}]);
    }));
    kept.push(prune.issue("24", 400, "closed", |issue| {
        issue["create"] = json!({"post": "pending", "attempt": null, "sent": null});
    }));
    let report = prune.run(&["--yes"]);
    assert_eq!(report["warnings"], json!([]));
    assert_eq!(keys(&report, "issues"), ["o/r#1"]);
    assert!(!removable.exists());
    for path in kept {
        assert!(path.exists(), "{}", path.display());
    }
}

#[test]
fn issues_with_unreadable_or_future_timestamps_are_kept() {
    let prune = Prune::new();
    let bad_local = prune.issue("1", 400, "closed", |issue| {
        issue["updated_at"] = json!("not a time");
    });
    let bad_remote = prune.issue("2", 400, "closed", |issue| {
        issue["remote"]["updated_at"] = json!("garbage");
    });
    let future = prune.issue("3", -30, "closed", |_| {});
    let report = prune.run(&["--yes"]);
    assert_eq!(report["warnings"], json!([]));
    assert_eq!(keys(&report, "issues"), Vec::<String>::new());
    for path in [bad_local, bad_remote, future] {
        assert!(path.exists(), "{}", path.display());
    }
}

#[test]
fn a_target_with_a_future_timestamp_is_kept() {
    let prune = Prune::new();
    let future = prune.target("github.com/o/a", "future", -30);
    prune.run(&["--yes"]);
    assert!(future.exists());
}

#[test]
fn busy_issue_and_task_locks_skip_the_item() {
    let prune = Prune::new();
    let issue = prune.issue("1", 400, "closed", |_| {});
    let lock = issue.with_file_name(".1.lock");
    File::create(&lock).unwrap();
    let guard = File::options().write(true).open(&lock).unwrap();
    guard.lock().unwrap();
    let task = prune.task("busy-task", "complete", 400);
    let task_lock = prune.agents().join("tasks/.busy-task.lock");
    fs::create_dir(&task_lock).unwrap();
    let report = prune.run(&["--yes"]);
    assert!(issue.exists() && task.exists());
    let skipped = report["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 2, "{skipped:?}");
    assert!(
        skipped
            .iter()
            .all(|item| item["why"].as_str().unwrap().contains("locked"))
    );
    drop(guard);
    fs::remove_dir(&task_lock).unwrap();
    prune.run_settled(&["--yes"]);
    assert!(!issue.exists() && !task.exists());
}

#[test]
fn a_held_publish_lock_skips_its_review() {
    let prune = Prune::new();
    let old = prune.target("github.com/o/a", "busy", 120);
    let lock = old.with_file_name(".busy.publish.lock");
    File::create(&lock).unwrap();
    let guard = File::options().write(true).open(&lock).unwrap();
    guard.lock().unwrap();
    let report = prune.run(&["--yes"]);
    assert!(old.exists());
    assert!(
        report["skipped"][0]["why"]
            .as_str()
            .unwrap()
            .contains("locked")
    );
    drop(guard);
    let report = prune.run_settled(&["--yes"]);
    assert!(!old.exists());
    assert!(!lock.exists());
    assert_eq!(report["skipped"], json!([]));
}

#[test]
fn the_issue_tracker_push_lock_is_kept() {
    let prune = Prune::new();
    let issue = prune.issue("1", 1, "open", |_| {});
    let dir = issue.parent().unwrap();
    fs::write(dir.join("tracker.json"), "{}").unwrap();
    let push = dir.join(".push.publish.lock");
    File::create(&push).unwrap();
    let report = prune.run(&["--yes"]);
    assert!(push.exists());
    assert_eq!(report["plan"]["locks"], json!([]));
    fs::remove_file(dir.join("tracker.json")).unwrap();
    prune.run(&["--yes"]);
    assert!(!push.exists());
}

#[test]
fn a_local_repo_whose_path_is_gone_and_unseen_for_a_month_is_listed_at_any_age() {
    let prune = Prune::new();
    let alive = prune.env.path("alive");
    fs::create_dir_all(&alive).unwrap();
    let gone = prune.target("local/gone-aaaaaaaaaaaa", "fresh", 1);
    let dir = gone.parent().unwrap().to_path_buf();
    let missing = prune.env.path("missing/.git");
    fs::write(
        dir.join(".repo.json"),
        json!({"common_dir": missing, "last_seen": ago(31)}).to_string(),
    )
    .unwrap();
    fs::write(dir.join(".account.json"), "{}").unwrap();
    // Missing too, but agt wrote there a day ago (a moved repository), or the path is on a
    // volume that may be unmounted: both stay.
    let moved = prune.target("local/moved-dddddddddddd", "fresh", 1);
    fs::write(
        moved.with_file_name(".repo.json"),
        json!({"common_dir": prune.env.path("moved/.git"), "last_seen": ago(1)}).to_string(),
    )
    .unwrap();
    let mounted = prune.target("local/mounted-eeeeeeeeeeee", "fresh", 1);
    fs::write(
        mounted.with_file_name(".repo.json"),
        json!({"common_dir": "/Volumes/nowhere/r/.git", "last_seen": ago(400)}).to_string(),
    )
    .unwrap();
    let live = prune.target("local/live-bbbbbbbbbbbb", "fresh", 1);
    fs::write(
        live.with_file_name(".repo.json"),
        json!({"common_dir": alive}).to_string(),
    )
    .unwrap();
    let unrecorded = prune.target("local/unrecorded-cccccccccccc", "fresh", 1);
    let report = prune.run(&[]);
    assert_eq!(keys(&report, "local_repos"), ["local/gone-aaaaaaaaaaaa"]);
    let reason = report["plan"]["local_repos"][0]["reason"].as_str().unwrap();
    assert!(
        reason.contains("is gone")
            && reason.contains("missing")
            && reason.contains("31 day(s) ago"),
        "{reason}"
    );
    // The fresh target is part of the repository item; nothing is double counted.
    let expected = len(&gone) + len(&dir.join(".repo.json")) + len(&dir.join(".account.json"));
    assert_eq!(report["totals"]["bytes"], expected);
    let report = prune.run(&["--yes"]);
    assert!(!dir.exists());
    assert_eq!(report["bytes_freed"], expected);
    assert!(live.exists() && unrecorded.exists() && moved.exists() && mounted.exists());
}

#[test]
fn agt_records_the_common_dir_of_a_local_repo() {
    let prune = Prune {
        env: Env::with_branch(),
    };
    prune.env.write("a.txt", "one\n");
    prune.env.commit_all("change");
    prune.env.run(&["run", "start"]);
    let recorded: Value =
        serde_json::from_slice(&fs::read(prune.env.state().join(".repo.json")).unwrap()).unwrap();
    let git = prune.env.repo.join(".git");
    assert_eq!(recorded["common_dir"], git.to_string_lossy().as_ref());
    let report = prune.run(&[]);
    assert_eq!(report["plan"]["local_repos"], json!([]));
    assert!(recorded["last_seen"].is_string());
    fs::rename(&git, prune.env.repo.join(".git-moved")).unwrap();
    let legacy = prune.agents().join("reviews/local/recorded-aaaaaaaaaaaa");
    fs::create_dir_all(&legacy).unwrap();
    let file = legacy.join(".repo.json");
    let seen = |days| {
        let mut record = recorded.clone();
        record["last_seen"] = json!(ago(days));
        fs::write(&file, record.to_string()).unwrap();
    };
    seen(0);
    let report = prune.run(&[]);
    assert_eq!(report["plan"]["local_repos"], json!([]));
    seen(31);
    let report = prune.run(&[]);
    assert_eq!(report["plan"]["local_repos"].as_array().unwrap().len(), 1);
}

#[test]
fn a_local_repo_with_other_files_or_links_is_never_planned() {
    let prune = Prune::new();
    let foreign = prune.target("local/foreign-aaaaaaaaaaaa", "t", 100);
    fs::write(foreign.with_file_name("notes.txt"), "mine").unwrap();
    let nested = prune.target("local/nested-bbbbbbbbbbbb", "t", 100);
    fs::create_dir(nested.with_file_name("sub")).unwrap();
    // A repository directory that is a link to outside the store.
    let outside = prune.env.path("outside");
    fs::create_dir_all(&outside).unwrap();
    let stranger = outside.join("t.json");
    fs::copy(&foreign, &stranger).unwrap();
    std::os::unix::fs::symlink(
        &outside,
        prune.agents().join("reviews/local/linked-cccccccccccc"),
    )
    .unwrap();
    let report = prune.run(&["--yes"]);
    assert_eq!(keys(&report, "local_repos"), Vec::<String>::new());
    assert!(foreign.with_file_name("notes.txt").exists());
    assert!(nested.with_file_name("sub").exists());
    assert!(stranger.exists());
}

#[test]
fn files_and_locks_that_are_links_are_not_followed() {
    let prune = Prune::new();
    let outside = prune.env.path("outside");
    fs::create_dir_all(&outside).unwrap();
    let stranger = outside.join("t.json");
    let lock_target = outside.join("lock");
    fs::write(&lock_target, "x").unwrap();
    let kept = prune.target("github.com/o/a", "real", 1);
    let dir = kept.parent().unwrap();
    std::os::unix::fs::symlink(&lock_target, dir.join(".dangling.lock")).unwrap();
    let report = prune.run(&["--yes"]);
    assert_eq!(report["plan"]["locks"], json!([]));
    assert!(lock_target.exists());
    // A target file that is a link is reported, not read through.
    prune.target("github.com/o/b", "real", 200);
    fs::write(&stranger, "{}").unwrap();
    let linked = prune.agents().join("reviews/github.com/o/b/link.json");
    std::os::unix::fs::symlink(&stranger, &linked).unwrap();
    let report = prune.run(&["--yes"]);
    assert!(stranger.exists() && linked.symlink_metadata().is_ok());
    assert_eq!(report["warnings"].as_array().unwrap().len(), 1);
}

#[test]
fn local_repo_bytes_are_counted_once() {
    let prune = Prune::new();
    let target = prune.target("local/gone-aaaaaaaaaaaa", "t", 100);
    let dir = target.parent().unwrap().to_path_buf();
    let account = dir.join(".account.json");
    fs::write(&account, "12345").unwrap();
    let stray = dir.join(".other.lock");
    fs::write(&stray, "123").unwrap();
    let report = prune.run(&[]);
    assert_eq!(report["plan"]["locks"], json!([]));
    assert_eq!(report["plan"]["reviews"][0]["bytes"], len(&target));
    assert_eq!(report["plan"]["local_repos"][0]["bytes"], 8);
    let total = len(&target) + 8;
    assert_eq!(report["totals"]["bytes"], total);
    let report = prune.run(&["--yes"]);
    assert_eq!(report["bytes_freed"], total);
    assert!(!dir.exists());
    // A lock alone, outside a listed repository, is its own item.
    let live = prune.target("github.com/o/a", "live", 1);
    let lone = live.with_file_name(".lone.lock");
    fs::write(&lone, "1234").unwrap();
    let report = prune.run(&["--only", "locks"]);
    assert_eq!(report["totals"]["bytes"], 4);
}

#[test]
fn an_unreadable_place_is_a_warning_and_the_rest_is_still_planned() {
    let prune = Prune::new();
    prune.target("github.com/o/a", "old", 120);
    fs::write(
        prune.agents().join("reviews/github.com/o/broken.json"),
        "{ nope",
    )
    .unwrap();
    prune.task("t1", "complete", 120);
    let report = prune.run(&[]);
    assert!(
        !report["warnings"].as_array().unwrap().is_empty(),
        "{report}"
    );
    assert_eq!(keys(&report, "tasks"), ["t1"]);
}

#[test]
fn a_backup_stamped_in_the_future_does_not_become_the_newest() {
    let prune = Prune::new();
    let oldest = prune.backup(&format!("reviews-v1-{}", stamp(200)));
    let newest = prune.backup(&format!("reviews-v1-{}", stamp(150)));
    let future = prune.backup(&format!("reviews-v1-{}", stamp(-30)));
    prune.run(&["--yes", "--only", "backups"]);
    assert!(!oldest.exists());
    assert!(newest.exists() && future.exists());
}

#[test]
fn a_review_with_fields_this_agt_does_not_know_is_kept_with_a_warning() {
    let prune = Prune::new();
    let newer = prune.target("github.com/o/a", "newer", 200);
    let mut document: Value = serde_json::from_slice(&fs::read(&newer).unwrap()).unwrap();
    document["from_the_future"] = json!(true);
    prune.write(&newer, &document);
    let old = prune.target("github.com/o/a", "old", 200);
    let report = prune.run(&["--yes"]);
    assert_eq!(keys(&report, "reviews"), ["github.com/o/a/old"]);
    let warnings = report["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("from_the_future")),
        "{report}"
    );
    assert!(newer.exists() && !old.exists());
}

#[test]
fn one_corrupt_file_does_not_stop_the_review_and_local_repo_passes() {
    let prune = Prune::new();
    let old = prune.target("github.com/o/a", "old", 200);
    fs::write(old.with_file_name("corrupt.json"), "{ nope").unwrap();
    let other = prune.target("github.com/o/b", "old", 200);
    let broken_local = prune.target("local/broken-aaaaaaaaaaaa", "t", 200);
    fs::write(broken_local.with_file_name("corrupt.json"), "{ nope").unwrap();
    let local = prune.target("local/fine-bbbbbbbbbbbb", "t", 200);
    let report = prune.run(&["--yes"]);
    assert!(!old.exists() && !other.exists() && !local.exists());
    assert!(broken_local.with_file_name("corrupt.json").exists());
    assert!(
        report["warnings"].as_array().unwrap().len() >= 2,
        "{report}"
    );
}

#[test]
fn cleanup_skips_a_review_whose_publish_lock_is_held() {
    let prune = Prune::new();
    let old = prune.target("github.com/o/a", "old", 200);
    let free = prune.target("github.com/o/a", "free", 200);
    let publish = File::options()
        .create(true)
        .append(true)
        .open(old.with_file_name(".old.publish.lock"))
        .unwrap();
    publish.lock().unwrap();
    let report = prune.run(&["--yes", "--only", "reviews"]);
    assert_eq!(report["deleted"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(
        report["skipped"][0]["key"], "github.com/o/a/old",
        "{report}"
    );
    assert!(old.exists() && !free.exists());
}

#[test]
fn the_review_plan_writes_nothing() {
    let prune = Prune {
        env: Env::with_branch(),
    };
    prune.run(&["--only", "reviews"]);
    assert!(!prune.env.state().join("local").exists());
}

#[test]
fn the_review_pass_goes_on_past_a_corrupt_file_and_reports_it_once() {
    let prune = Prune::new();
    let a = prune.target("github.com/o/a", "old-a", 120);
    let b = prune.target("github.com/o/b", "old-b", 120);
    let bad = a.with_file_name("bad.json");
    fs::write(&bad, "not json").unwrap();
    let report = prune.run(&["--only", "reviews"]);
    assert_eq!(
        keys(&report, "reviews"),
        ["github.com/o/a/old-a", "github.com/o/b/old-b"],
        "{report}"
    );
    prune.run(&["--yes", "--only", "reviews"]);
    assert!(!a.exists() && !b.exists());
    assert!(bad.exists());
    // The pass warns once about the corrupt file, next to a healthy target.
    let keep = prune.target("github.com/o/a", "fresh", 1);
    let report = prune.run(&["--only", "reviews"]);
    let warnings = report["warnings"].as_array().unwrap();
    let about_bad = warnings
        .iter()
        .filter(|warning| warning.as_str().unwrap().contains("bad.json"))
        .count();
    assert_eq!(about_bad, 1, "{warnings:?}");
    assert!(keep.exists());
}

#[test]
fn cleanup_lists_a_review_it_could_not_remove_as_skipped() {
    let prune = Prune::new();
    let old = prune.target("github.com/o/a", "busy", 120);
    let lock = old.with_file_name(".busy.lock");
    File::create(&lock).unwrap();
    let guard = File::options().write(true).open(&lock).unwrap();
    guard.lock().unwrap();
    let report = prune.run(&["--yes", "--only", "reviews"]);
    assert!(old.exists());
    assert_eq!(report["skipped"][0]["key"], "github.com/o/a/busy");
    assert!(
        report["skipped"][0]["why"]
            .as_str()
            .unwrap()
            .contains("locked")
    );
    drop(guard);
}

#[cfg(unix)]
#[test]
fn an_unreadable_repository_directory_is_a_warning_and_the_pass_goes_on() {
    let prune = Prune::new();
    let old = prune.target("github.com/o/a", "old", 120);
    let closed = prune.target("github.com/o/b", "hidden", 120);
    let dir = closed.parent().unwrap().to_path_buf();
    let sealed = Sealed::new(&dir);
    let report = prune.run(&["--only", "reviews"]);
    let plan = prune.run(&[]);
    drop(sealed);
    // A reader that ignores permissions (root) sees both; otherwise one is a warning.
    let seen = report["plan"]["reviews"].as_array().unwrap().len();
    let warned = report["warnings"].as_array().unwrap().len();
    assert!(
        (seen == 1 && warned == 1) || (seen == 2 && warned == 0),
        "{report}"
    );
    assert!(old.exists());
    assert!(plan["warnings"].is_array());
}

#[test]
fn a_local_repo_with_no_targets_is_listed_only_after_a_quiet_quarter() {
    let prune = Prune::new();
    let alive = prune.env.path("alive");
    fs::create_dir_all(&alive).unwrap();
    let dir = prune.agents().join("reviews/local/empty-dddddddddddd");
    fs::create_dir_all(&dir).unwrap();
    let record = |days| {
        fs::write(
            dir.join(".repo.json"),
            json!({"common_dir": alive, "last_seen": ago(days)}).to_string(),
        )
        .unwrap();
    };
    record(89);
    assert_eq!(
        keys(&prune.run(&["--yes"]), "local_repos"),
        Vec::<String>::new()
    );
    assert!(dir.exists());
    record(91);
    let report = prune.run(&[]);
    assert_eq!(keys(&report, "local_repos"), ["local/empty-dddddddddddd"]);
    let reason = report["plan"]["local_repos"][0]["reason"].as_str().unwrap();
    assert!(
        reason.contains("no targets left") && reason.contains("91 day(s) ago"),
        "{reason}"
    );
    prune.run(&["--yes"]);
    assert!(!dir.exists());
}

/// 30 days after the real time, for tests that compare the clock with real process start
/// times and git timestamps, where the fixed `NOW` would fail once the real date passes it.
fn later() -> i64 {
    i64::try_from(unix_now()).unwrap() + 30 * DAY
}

/// Sets a path's modification time to `days` before `NOW`.
fn age(path: &Path, days: i64) {
    let secs = u64::try_from(NOW - days * DAY).unwrap();
    File::open(path)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
        .unwrap();
}

/// A file `days` old, in a directory created as needed.
fn old_file(path: &Path, days: i64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "x").unwrap();
    age(path, days);
}

#[test]
fn output_and_scratch_go_whole_once_every_file_is_old() {
    let prune = Prune::new();
    let cache = prune.env.path("cache").join("agents");
    let output = cache.join("output");
    old_file(&output.join("old/a.md"), 8);
    age(&output.join("old"), 8);
    old_file(&output.join("mixed/a.md"), 8);
    old_file(&output.join("mixed/b.md"), 1);
    age(&output.join("mixed"), 8);
    old_file(&output.join("loose.md"), 6);
    let scratch = cache.join("scratch");
    old_file(&scratch.join("task/notes.md"), 31);
    age(&scratch.join("task"), 31);
    old_file(&scratch.join(".keep"), 400);
    old_file(&scratch.join("young/notes.md"), 29);
    age(&scratch.join("young"), 29);
    let uid = unsafe { libc::getuid() };
    let harness = prune.env.path("tmp").join(format!("claude-{uid}"));
    let session = harness.join("-Users-me-repo/0f0f");
    old_file(&session.join("scratchpad/x"), 40);
    age(&session.join("scratchpad"), 40);
    age(&session, 40);
    let live = harness.join("-Users-me-other/1a1a");
    old_file(&live.join("tasks/out"), 2);

    let report = prune.run(&["--only", "output", "--only", "scratch"]);
    assert_eq!(keys(&report, "output"), ["old"]);
    let session_key = session.to_string_lossy().into_owned();
    assert_eq!(keys(&report, "scratch"), [session_key.as_str(), "task"]);
    assert!(output.join("old").exists());

    let report = prune.run(&["--only", "output", "--only", "scratch", "--yes"]);
    assert_eq!(report["deleted"].as_array().unwrap().len(), 3, "{report}");
    assert!(!output.join("old").exists() && output.join("mixed/a.md").exists());
    assert!(output.join("loose.md").exists() && scratch.join(".keep").exists());
    assert!(!scratch.join("task").exists() && scratch.join("young").exists());
    // The emptied cwd directory goes with its last session; the live one stays.
    assert!(!session.parent().unwrap().exists() && live.exists());
}

/// Not a test of its own: with `AGT_TEST_SLEEP` set it stands in for a tool a session
/// started. The test binary is used because macOS hides the environment of Apple's binaries.
#[test]
fn sleeper() {
    if std::env::var_os("AGT_TEST_SLEEP").is_some() {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

#[test]
fn orphan_processes_are_signalled_only_when_named() {
    let prune = Prune::new();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cleanup::sleeper",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("AGT_TEST_SLEEP", "1")
        // A session pid that no process has.
        .env("CLAUDE_PID", "2000000000")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id().to_string();
    let run = |args: &[&str]| {
        let output = prune
            .env
            .bin(&prune.env.repo)
            .arg("cleanup")
            .args(args)
            .env("AGT_NOW", later().to_string())
            .env("AGT_CLEANUP_PID", &pid)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    // The environment is readable a moment after the spawn, once the child has exec'd.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let listed = loop {
        let report = run(&["--only", "processes"]);
        if !keys(&report, "processes").is_empty() || std::time::Instant::now() > deadline {
            break report;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(keys(&listed, "processes"), [pid.as_str()], "{listed}");
    let reason = listed["plan"]["processes"][0]["reason"].as_str().unwrap();
    assert!(
        reason.contains("Claude session 2000000000 is gone"),
        "{reason}"
    );

    let report = run(&["--yes"]);
    assert_eq!(report["signalled"], json!([]));
    assert!(
        report["skipped"][0]["why"]
            .as_str()
            .unwrap()
            .contains("--only processes")
    );
    assert!(child.try_wait().unwrap().is_none());

    let report = run(&["--only", "processes", "--yes"]);
    assert_eq!(
        report["signalled"],
        json!([{ "pid": pid, "signal": "SIGTERM" }])
    );
    assert!(child.wait().unwrap().signal().is_some());
}

#[test]
fn a_session_is_gone_only_when_no_record_matches_its_process() {
    let prune = Prune::new();
    let home = prune.env.path("home");
    let me = std::process::id();
    let record = |dir: &str, start: &str| {
        let sessions = home.join(dir).join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join(format!("{me}.json")),
            json!({ "pid": me, "procStart": start }).to_string(),
        )
        .unwrap();
    };
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "cleanup::sleeper",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("AGT_TEST_SLEEP", "1")
        .env("CLAUDE_PID", me.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let child_pid = child.id().to_string();
    let listed = || {
        // The environment is readable a moment after the spawn, once the child has exec'd.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let output = prune
                .env
                .bin(&prune.env.repo)
                .args(["cleanup", "--only", "processes"])
                .env("AGT_NOW", later().to_string())
                .env("AGT_CLEANUP_PID", &child_pid)
                .output()
                .unwrap();
            let report: Value = serde_json::from_slice(&output.stdout).unwrap();
            let found = keys(&report, "processes");
            if !found.is_empty() || std::time::Instant::now() > deadline {
                return found;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    let stale = if cfg!(target_os = "linux") {
        "1"
    } else {
        "Mon Jan  1 00:00:00 2024"
    };
    // A record for a process that started at another time: the pid was reused.
    record(".claude", stale);
    assert_eq!(listed(), [child_pid.as_str()]);
    // A matching record in another config directory wins over the stale one.
    record(".claude-two", &proc_start(me));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        assert_eq!(listed(), Vec::<String>::new());
    }
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn manual_leftovers_are_listed_and_saved_never_deleted() {
    let prune = Prune::new();
    let home = prune.env.path("home");
    let repo = home.join("projects/owner/app");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let status = prune
            .env
            .command("git", &repo)
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["commit", "-q", "--allow-empty", "-m", "one"]);
    git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(&["branch", "done"]);
    // Long-lived names stay protected even when merged.
    git(&["branch", "develop"]);
    git(&["branch", "master"]);
    git(&["branch", "busy"]);
    git(&["worktree", "add", "-q", ".worktrees/busy", "busy"]);
    let releases = home.join(".codex/packages/standalone/releases");
    for name in ["0.9.0", "0.10.0", "0.11.0", "0.12.0"] {
        fs::create_dir_all(releases.join(name)).unwrap();
    }
    std::os::unix::fs::symlink(
        releases.join("0.11.0"),
        home.join(".codex/packages/standalone/current"),
    )
    .unwrap();
    let browsers = home.join(".cache/ms-playwright");
    for name in ["chromium-1100", "chromium-1200", "ffmpeg-1010"] {
        fs::create_dir_all(browsers.join(name)).unwrap();
    }

    let report = prune.run_at(later(), &["--only", "manual"]);
    let manual = &report["manual"];
    let repo_path = repo.canonicalize().unwrap();
    assert_eq!(
        manual["branches"],
        json!([format!("git -C {} branch -d done", repo_path.display())])
    );
    // Merged and clean. Its branch is checked out, so only the worktree is listed.
    assert_eq!(
        manual["worktrees"],
        json!([format!(
            "agt worktree remove {} --yes",
            repo_path.join(".worktrees/busy").display()
        )])
    );
    assert_eq!(
        manual["codex_releases"],
        json!([
            releases.join("0.9.0").display().to_string(),
            releases.join("0.12.0").display().to_string()
        ])
    );
    assert_eq!(
        manual["playwright"],
        json!([browsers.join("chromium-1100").display().to_string()])
    );
    assert!(report.get("manual_report").is_none());
    let count = |prune: &Prune| {
        let output = prune
            .env
            .bin(&prune.env.repo)
            .args(["cleanup", "--count"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    };
    assert_eq!(count(&prune), "");

    let report = prune.run_at(later(), &["--only", "manual", "--yes"]);
    let saved = PathBuf::from(report["manual_report"].as_str().unwrap());
    let text = fs::read_to_string(&saved).unwrap();
    assert!(text.contains("<!-- count: 5 -->"), "{text}");
    assert!(text.contains(&format!("git -C {} branch -d done", repo_path.display())));
    assert_eq!(
        count(&prune),
        format!("agt: 5 cleanup candidates in {}\n", saved.display())
    );
    assert!(releases.join("0.9.0").exists() && browsers.join("chromium-1100").exists());
    assert!(repo_path.join(".worktrees/busy").exists());
}

#[test]
fn scratch_never_follows_a_link_out_of_the_harness_root() {
    let prune = Prune::new();
    let uid = unsafe { libc::getuid() };
    // A victim directory with old files that a link could point the sweep at.
    let victim = prune.env.path("victim");
    old_file(&victim.join("precious/data"), 400);
    age(&victim.join("precious"), 400);
    old_file(&victim.join("notes.txt"), 400);
    // `/tmp/claude-<uid>` itself made a link by someone else.
    let tmp = prune.env.path("tmp");
    fs::create_dir_all(&tmp).unwrap();
    std::os::unix::fs::symlink(&victim, tmp.join(format!("claude-{uid}"))).unwrap();
    // And a slug that is a link inside a real `~/.cache/claude-tmp/claude-<uid>`.
    let harness = prune
        .env
        .path("home")
        .join(format!(".cache/claude-tmp/claude-{uid}"));
    fs::create_dir_all(&harness).unwrap();
    std::os::unix::fs::symlink(&victim, harness.join("evil")).unwrap();

    let report = prune.run(&["--only", "scratch", "--yes"]);
    assert_eq!(keys(&report, "scratch"), Vec::<String>::new(), "{report}");
    assert!(
        report["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("not a directory of this user")),
        "{report}"
    );
    assert!(victim.join("precious/data").exists() && victim.join("notes.txt").exists());
}

#[test]
fn manual_report_skips_a_worktree_changed_in_the_last_3_days() {
    let prune = Prune::new();
    let repo = prune.env.path("home/projects/owner/app");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let status = prune
            .env
            .command("git", &repo)
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["commit", "-q", "--allow-empty", "-m", "one"]);
    git(&["worktree", "add", "-q", ".worktrees/fresh", "-b", "fresh"]);

    let report = prune.run_at(i64::try_from(unix_now()).unwrap(), &["--only", "manual"]);
    assert_eq!(report["manual"]["worktrees"], json!([]));
    let report = prune.run_at(later(), &["--only", "manual"]);
    assert_eq!(report["manual"]["worktrees"].as_array().unwrap().len(), 1);
}

#[test]
fn manual_report_skips_a_worktree_holding_an_ignored_nested_repository() {
    let prune = Prune::new();
    let repo = prune.env.path("home/projects/owner/app");
    fs::create_dir_all(&repo).unwrap();
    let git = |dir: &Path, args: &[&str]| {
        let status = prune
            .env
            .command("git", dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
    fs::write(repo.join(".git/info/exclude"), "vendor/\n").unwrap();
    git(
        &repo,
        &["worktree", "add", "-q", ".worktrees/done", "-b", "done"],
    );

    let report = prune.run_at(later(), &["--only", "manual"]);
    assert_eq!(report["manual"]["worktrees"].as_array().unwrap().len(), 1);

    // An ignored clone has history that a backup cannot hold, so the worktree is not offered.
    let nested = repo.join(".worktrees/done/vendor/lib");
    fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "-q"]);
    git(&nested, &["commit", "-q", "--allow-empty", "-m", "lib"]);
    let report = prune.run_at(later(), &["--only", "manual"]);
    assert_eq!(report["manual"]["worktrees"], json!([]));
}

#[test]
fn manual_report_skips_a_worktree_with_a_process_running_in_it_as_the_sweep_does() {
    let prune = Prune::new();
    let repo = prune.env.path("home/projects/owner/app");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let status = prune
            .env
            .command("git", &repo)
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["commit", "-q", "--allow-empty", "-m", "one"]);
    git(&["worktree", "add", "-q", ".worktrees/done", "-b", "done"]);
    let done = repo.join(".worktrees/done");

    let report = prune.run_at(later(), &["--only", "manual"]);
    assert_eq!(report["manual"]["worktrees"].as_array().unwrap().len(), 1);

    // The sweep keeps a worktree where another process runs, so the list must not offer it.
    let mut child = prune.env.command("sleep", &done).arg("30").spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut listed = 1;
    while listed != 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let report = prune.run_at(later(), &["--only", "manual"]);
        listed = report["manual"]["worktrees"].as_array().unwrap().len();
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(listed, 0);
}

#[test]
fn manual_report_lists_the_other_rows_when_one_worktree_has_an_unreadable_directory() {
    let prune = Prune::new();
    let repo = prune.env.path("home/projects/owner/app");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let status = prune
            .env
            .command("git", &repo)
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["commit", "-q", "--allow-empty", "-m", "one"]);
    git(&["worktree", "add", "-q", ".worktrees/a-open", "-b", "a-open"]);
    git(&[
        "worktree",
        "add",
        "-q",
        ".worktrees/z-sealed",
        "-b",
        "z-sealed",
    ]);
    let locked = repo.join(".worktrees/z-sealed/locked");
    fs::create_dir_all(&locked).unwrap();
    let _sealed = Sealed::new(&locked);

    let report = prune.run_at(later(), &["--only", "manual"]);
    let listed = report["manual"]["worktrees"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "{report}");
    assert!(listed[0].as_str().unwrap().contains("a-open"), "{report}");
}

#[test]
fn manual_report_skips_a_worktree_whose_submodule_holds_a_stash() {
    let prune = Prune::new();
    let repo = prune.env.path("home/projects/owner/app");
    let origin = prune.env.path("sub-origin");
    for dir in [&repo, &origin] {
        fs::create_dir_all(dir).unwrap();
    }
    let git = |dir: &Path, args: &[&str]| {
        let status = prune
            .env
            .command("git", dir)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "protocol.file.allow")
            .env("GIT_CONFIG_VALUE_0", "always")
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&origin, &["init", "-q", "-b", "main"]);
    fs::write(origin.join("lib.txt"), "lib\n").unwrap();
    git(&origin, &["add", "-A"]);
    git(&origin, &["commit", "-q", "-m", "lib"]);
    git(&repo, &["init", "-q", "-b", "main"]);
    git(
        &repo,
        &["submodule", "add", "-q", origin.to_str().unwrap(), "mod"],
    );
    git(&repo, &["commit", "-q", "-m", "one"]);
    git(
        &repo,
        &["worktree", "add", "-q", ".worktrees/done", "-b", "done"],
    );
    let done = repo.join(".worktrees/done");
    git(&done, &["submodule", "update", "--init"]);

    let report = prune.run_at(later(), &["--only", "manual"]);
    assert_eq!(report["manual"]["worktrees"].as_array().unwrap().len(), 1);

    // A stash lives only in the clone under the worktree's git directory.
    fs::write(done.join("mod/lib.txt"), "changed\n").unwrap();
    git(&done.join("mod"), &["stash", "push", "-q"]);
    let report = prune.run_at(later(), &["--only", "manual"]);
    assert_eq!(report["manual"]["worktrees"], json!([]));
}

/// A `git` in front of the real one that records every command line it is given.
fn recording_git(prune: &Prune) -> (String, PathBuf) {
    let log = prune.env.path("git-calls");
    let path = prune.env.git_wrapper(
        "recording-bin",
        &format!("echo \"$@\" >> '{}'\n", log.display()),
    );
    (path, log)
}

#[test]
fn the_manual_report_never_asks_a_remote_and_does_not_list_a_worktree_holding_an_unheld_tag() {
    let prune = Prune::new();
    let repo = prune.env.path("home/projects/owner/app");
    let origin = prune.env.path("sub-origin");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&origin).unwrap();
    let git = |dir: &Path, args: &[&str]| {
        let status = prune
            .env
            .command("git", dir)
            .args(["-c", "protocol.file.allow=always"])
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&origin, &["init", "-q", "-b", "main"]);
    git(&origin, &["commit", "-q", "--allow-empty", "-m", "lib"]);
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
    git(
        &repo,
        &["submodule", "add", "-q", &origin.to_string_lossy(), "mod"],
    );
    git(&repo, &["commit", "-q", "-m", "add submodule"]);
    git(
        &repo,
        &["worktree", "add", "-q", ".worktrees/done", "-b", "done"],
    );
    let done = repo.join(".worktrees/done");
    git(&done, &["submodule", "update", "--init", "-q"]);
    // A tag on a commit that only this clone has.
    let sub = done.join("mod");
    git(&sub, &["commit", "-q", "--allow-empty", "-m", "local only"]);
    git(&sub, &["tag", "mine"]);
    git(&sub, &["checkout", "-q", "--detach", "HEAD~1"]);

    let (path, log) = recording_git(&prune);
    let output = prune
        .env
        .bin(&prune.env.repo)
        .arg("cleanup")
        .args(["--only", "manual"])
        .env("AGT_NOW", later().to_string())
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["manual"]["worktrees"], json!([]), "{report}");
    let calls = fs::read_to_string(&log).unwrap();
    assert!(!calls.contains("ls-remote"), "{calls}");
}

#[test]
fn cleanup_discovers_local_state_without_creating_it() {
    let prune = Prune::new();
    prune.env.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/owner/repo.git",
    ]);
    let source = prune.target("github.com/owner/repo", "branch-old", 100);
    prune.run(&["--only", "reviews"]);
    assert!(!prune.env.repo.join(".agt").exists());
    let local = prune.env.state().join("branch-old.json");
    fs::create_dir_all(local.parent().unwrap()).unwrap();
    fs::copy(&source, &local).unwrap();
    let report = prune.run(&["--only", "reviews", "--yes"]);
    assert!(!local.exists(), "{report}");
    assert!(prune.env.repo.join(".agt").is_dir());
}

#[test]
fn repository_storage_is_shared_with_linked_worktrees() {
    let env = Env::with_branch();
    let linked = env.path("linked");
    env.git(&["worktree", "add", "-b", "linked", linked.to_str().unwrap()]);
    let started = env.run(&["run", "start"]);
    let key = started["target"].as_str().unwrap();
    assert!(env.state().join(format!("{key}.json")).exists());
    let output = env
        .agt(&linked)
        .args(["--target", key, "list"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let list: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(list.to_string().contains(key), "{list}");
    assert!(!linked.join(".agt").exists());
}

#[test]
fn cleanup_backup_retention_keeps_newest_for_each_clone() {
    let prune = Prune::new();
    let first = "a".repeat(40);
    let second = "b".repeat(40);
    let old = prune.backup(&format!("repositories/{first}/reviews-v1-{}", stamp(300)));
    let newest_first = prune.backup(&format!("repositories/{first}/reviews-v1-{}", stamp(200)));
    let newest_second = prune.backup(&format!("repositories/{second}/reviews-v1-{}", stamp(400)));
    prune.run(&["--only", "backups", "--yes"]);
    assert!(!old.exists());
    assert!(newest_first.exists());
    assert!(newest_second.exists());
}

#[test]
fn cleanup_configured_and_current_linked_roots_are_deduplicated() {
    let env = Env::with_branch();
    let linked = env.path("linked");
    env.git(&["worktree", "add", "-b", "linked", linked.to_str().unwrap()]);
    env.write_config(&format!(
        "[worktree]\nroots = [{}]\n",
        serde_json::to_string(&env.repo).unwrap()
    ));
    let prune = Prune { env };
    let source = prune.target("github.com/other/repo", "branch-old", 120);
    let local = prune.env.state().join("branch-old.json");
    fs::create_dir_all(local.parent().unwrap()).unwrap();
    fs::copy(source, &local).unwrap();
    let output = prune
        .env
        .bin(&linked)
        .args(["cleanup", "--only", "reviews"])
        .env("AGT_NOW", NOW.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let matching = report["plan"]["reviews"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["path"] == local.to_str().unwrap())
        .count();
    assert_eq!(matching, 1, "{report}");
}

#[test]
fn repository_storage_identity_survives_relocation_after_private_edits() {
    use crate::support::issue::{Github, IssueFake, Spec};
    let mut env = Env::with_branch();
    env.git(&["remote", "add", "origin", "https://github.com/o/r.git"]);
    env.write_config(&Github.config(true));
    (&Github as &dyn IssueFake).serve(&env, &[Spec::new(1, "Moved issue")]);
    env.issue(&["sync"]);
    env.issue(&["list"]);
    env.issue(&["note", "1", "Private relocation note"]);
    let review = env.run(&["run", "start"]);
    let review_key = review["target"].as_str().unwrap();
    let identity = fs::read(env.repo.join(".git/agt-repository-id")).unwrap();
    let moved = env.path("moved-repo");
    fs::rename(&env.repo, &moved).unwrap();
    env.repo = moved;
    env.issue(&["list"]);
    let shown = env.issue(&["show", "1"]);
    assert!(
        shown.to_string().contains("Private relocation note"),
        "{shown}"
    );
    let reviews = env.run(&["list"]);
    assert!(reviews.to_string().contains(review_key), "{reviews}");
    assert_eq!(
        fs::read(env.repo.join(".git/agt-repository-id")).unwrap(),
        identity
    );
}

#[test]
fn repository_storage_identity_is_private_shared_and_validated() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let env = Env::with_branch();
    let identity = env.repo.join(".git/agt-repository-id");
    let lock = env.repo.join(".git/.agt-repository-id.lock");
    let preview = env
        .bin(&env.repo)
        .args(["cleanup", "--only", "reviews"])
        .output()
        .unwrap();
    assert!(preview.status.success());
    assert!(!identity.exists());
    assert!(!lock.exists());
    env.run(&["list"]);
    let original = fs::read(&identity).unwrap();
    assert_eq!(original.len(), 40);
    assert_eq!(
        fs::metadata(&identity).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let linked = env.path("linked-id");
    env.git(&[
        "worktree",
        "add",
        "-b",
        "linked-id",
        linked.to_str().unwrap(),
    ]);
    assert!(env.agt(&linked).args(["list"]).status().unwrap().success());
    assert_eq!(fs::read(&identity).unwrap(), original);
    let saved = env.path("saved-id");
    fs::rename(&identity, &saved).unwrap();
    symlink(&saved, &identity).unwrap();
    assert!(env.fail(&["list"], None).contains("must be a regular file"));
    assert_eq!(fs::read(&saved).unwrap(), original);
    fs::remove_file(&identity).unwrap();
    fs::create_dir(&identity).unwrap();
    assert!(env.fail(&["list"], None).contains("must be a regular file"));
    fs::remove_dir(&identity).unwrap();
    fs::write(&identity, "../unsafe").unwrap();
    assert!(
        env.fail(&["list"], None)
            .contains("invalid repository identity")
    );
    assert_eq!(fs::read_to_string(&identity).unwrap(), "../unsafe");
}

#[test]
fn repository_storage_reused_checkout_path_gets_a_new_identity() {
    let mut env = Env::with_branch();
    env.run(&["list"]);
    let original_path = env.repo.clone();
    let first = fs::read(original_path.join(".git/agt-repository-id")).unwrap();
    let moved = env.path("first-clone");
    fs::rename(&original_path, &moved).unwrap();
    fs::create_dir(&original_path).unwrap();
    env.git(&["init", "-q", "-b", "main"]);
    env.git(&["commit", "-q", "--allow-empty", "-m", "New clone"]);
    env.run(&["list"]);
    let second = fs::read(original_path.join(".git/agt-repository-id")).unwrap();
    assert_ne!(first, second);
    env.repo = moved;
    env.run(&["list"]);
    assert_eq!(
        fs::read(env.repo.join(".git/agt-repository-id")).unwrap(),
        first
    );
}
