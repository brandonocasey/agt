//! One `[[tracker]]` list for issues and pull requests: which remotes become trackers, the
//! repository file's limits, and the old keys that still load.

use crate::support::forge::FakeService;
use crate::support::forge::github::Github;
use crate::support::issue::{Github as GithubIssues, Gitlab as GitlabIssues, IssueFake, Spec};
use crate::support::scenario::{Setup, kinds};
use crate::support::{Env, strs};

/// A repository whose `origin` is a GitHub project and `upstream` a GitLab one.
fn two_remotes() -> Env {
    let env = Env::new();
    // The logins answer, the listings do not: the calls made say which trackers a sync used.
    env.respond(&strs(&GithubIssues.user_call()), r#"{"login":"me"}"#);
    env.respond(&strs(&GitlabIssues.user_call()), r#"{"username":"me"}"#);
    env.git(&["remote", "add", "origin", "https://github.com/o/r.git"]);
    env.git(&["remote", "add", "upstream", "https://gitlab.com/g/p.git"]);
    env
}

fn reached(env: &Env, needle: &str) -> bool {
    env.forge_calls().iter().any(|call| call.contains(needle))
}

#[test]
fn every_remote_is_a_tracker_unless_remotes_says_origin() {
    let env = two_remotes();
    // GitHub answers, so the sync goes on to GitLab, which does not.
    let github: &dyn IssueFake = &GithubIssues;
    github.serve(&env, &[Spec::new(1, "One")]);
    env.write_config(
        "[[tracker]]\nhost = \"github.com\"\nrepo = \"o/r\"\nissues = { scope = \"all\" }\n",
    );
    env.issue_exec(&["sync"], "", &[]);
    assert!(reached(&env, "repos/o/r"), "{:?}", env.forge_calls());
    assert!(reached(&env, "projects/g%2Fp"), "{:?}", env.forge_calls());

    let env = two_remotes();
    env.write_config("[sync]\nremotes = \"origin\"\n");
    env.issue_exec(&["sync"], "", &[]);
    assert!(reached(&env, "repos/o/r"), "{:?}", env.forge_calls());
    assert!(!reached(&env, "projects/g%2Fp"), "{:?}", env.forge_calls());
}

/// Both remotes as trackers reading every issue; `gitlab` is extra keys of GitLab's issues table.
fn both_all(gitlab: &str) -> String {
    format!(
        "[sync]\npush = \"manual\"\n\
         [[tracker]]\nhost = \"github.com\"\nrepo = \"o/r\"\nissues = {{ scope = \"all\" }}\n\
         [[tracker]]\nhost = \"gitlab.com\"\nrepo = \"g/p\"\nissues = {{ {gitlab}scope = \"all\" }}\n"
    )
}

#[test]
fn one_failing_tracker_is_named_and_the_others_still_sync() {
    let env = two_remotes();
    // GitHub answers; GitLab's listing fails.
    let github: &dyn IssueFake = &GithubIssues;
    github.serve(&env, &[Spec::new(1, "One")]);
    let gitlab: &dyn IssueFake = &GitlabIssues;
    env.fail_call_with(
        &strs(&gitlab.list_call("2026-07-12T10%3A00%3A00Z")),
        "HTTP 500",
    );
    env.write_config(&both_all(""));
    let output = env.issue_exec(&["sync"], "", &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("gitlab.com/g/p"), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let trackers = report["trackers"].as_array().unwrap();
    assert_eq!(trackers.len(), 2, "{report}");
    assert!(
        trackers.iter().any(|tracker| tracker["error"].is_string()),
        "{report}"
    );
    assert!(
        trackers
            .iter()
            .any(|tracker| tracker.get("error").is_none()),
        "{report}"
    );
}

#[test]
fn sync_reports_the_trackers_that_keep_changes_local_when_only_some_do() {
    let env = two_remotes();
    let github: &dyn IssueFake = &GithubIssues;
    github.serve(&env, &[Spec::new(1, "One")]);
    let gitlab: &dyn IssueFake = &GitlabIssues;
    gitlab.serve(&env, &[Spec::new(1, "One")]);
    env.write_config(&both_all("push = \"off\", "));
    let output = env.issue_exec(&["sync"], "", &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("kept local") && stderr.contains("gitlab.com/g/p"),
        "{stderr}"
    );
    assert!(!stderr.contains("github.com/o/r"), "{stderr}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let kept = report["push"]["kept_local"].as_str().unwrap_or_default();
    assert!(kept.contains("gitlab.com/g/p"), "{report}");
}

#[test]
fn remotes_none_leaves_only_configured_trackers() {
    let env = two_remotes();
    env.write_config("[sync]\nremotes = \"none\"\n");
    let error = env.issue_fail(&["sync"]);
    assert!(error.contains("no tracker serves issues"), "{error}");
    assert!(
        error.contains("`[sync] remotes = \"none\"` turns off"),
        "{error}"
    );
    // Without it the hint is not there.
    let env = Env::new();
    let error = env.issue_fail(&["sync"]);
    assert!(error.contains("no tracker serves issues"), "{error}");
    assert!(!error.contains("remotes"), "{error}");
    assert!(env.forge_calls().is_empty());
}

#[test]
fn pull_off_is_a_config_error_that_names_the_key() {
    for (text, key) in [
        ("[sync]\npull = \"off\"\n", "[sync] pull"),
        ("[issues]\npull = \"off\"\n", "[issues] pull"),
        ("[pull_requests]\npull = \"off\"\n", "[pull_requests] pull"),
        (
            "[[tracker]]\nhost = \"github.com\"\nrepo = \"o/r\"\npull = \"off\"\n",
            "tracker[0].pull",
        ),
        (
            "[[tracker]]\nhost = \"github.com\"\nrepo = \"o/r\"\nissues = { pull = \"off\" }\n",
            "tracker[0].issues.pull",
        ),
    ] {
        let env = two_remotes();
        env.write_config(text);
        let error = env.issue_fail(&["sync"]);
        assert!(
            error.contains(key) && error.contains("off"),
            "{key}: {error}"
        );
        assert!(env.forge_calls().is_empty());
    }
}

#[test]
fn old_auto_pull_false_maps_to_manual_and_pulls_on_an_explicit_command() {
    let env = two_remotes();
    // Only origin is asked, and it answers, so the explicit sync can succeed.
    let github: &dyn IssueFake = &GithubIssues;
    github.serve(&env, &[Spec::new(1, "One")]);
    env.write_config(
        "[sync]\nauto_pull = false\nremotes = \"origin\"\n\
         [[tracker]]\nhost = \"github.com\"\nrepo = \"o/r\"\nissues = { scope = \"all\" }\n",
    );
    let output = env.issue_exec(&["sync"], "", &[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(reached(&env, "repos/o/r"), "{:?}", env.forge_calls());
}

#[test]
fn the_repository_file_cannot_lower_the_global_push() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    for finding in [setup.finding(1), setup.finding(2)] {
        env.run(&["resolve", &finding, "--stale"]);
    }
    svc.link(&setup);
    svc.accept(env, crate::support::forge::Call::PostNote, 604);
    env.write_config("[sync]\npush = \"manual\"\n");
    env.write_repo_config("[sync]\npush = \"off\"\n");
    let published = env.run(&["publish", "--pr", "42"]);
    assert_eq!(kinds(&published, "posted"), ["summary"]);
}

#[test]
fn the_repository_file_can_raise_push() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    svc.link(&setup);
    env.write_config("[sync]\npush = \"off\"\n");
    env.write_repo_config("[sync]\npush = \"manual\"\n");
    let output = env.exec(&["publish", "--pr", "42", "--dry-run"], None, &[]);
    assert!(output.status.success());
    let output = env.exec(&["publish", "--pr", "42"], None, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("kept local"), "{stderr}");
}

#[test]
fn a_repository_tracker_for_another_repository_is_ignored_with_a_warning() {
    let env = two_remotes();
    env.write_repo_config(
        "[[tracker]]\nhost = \"github.com\"\nrepo = \"stranger/else\"\npush = \"auto\"\n",
    );
    let output = env.issue_exec(&["sync"], "", &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ignoring [[tracker]] github.com/stranger/else"),
        "{stderr}"
    );
    assert!(!reached(&env, "stranger"), "{:?}", env.forge_calls());
}

#[test]
fn old_keys_still_load_map_and_warn_once_per_key() {
    let fake: &dyn IssueFake = &GithubIssues;
    let env = Env::new();
    env.write_config(
        "[forge]\npost = true\n[sync]\nauto_pull = false\n[[issues.tracker]]\nforge = \"github\"\nhost = \"github.com\"\nrepo = \"o/r\"\nscope = \"all\"\npush = true\n",
    );
    fake.serve(&env, &[Spec::new(1, "One")]);
    let output = env.issue_exec(&["sync"], "", &[]);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    for old in [
        "`[forge] post`",
        "`[sync] auto_pull`",
        "`[[issues.tracker]]`",
    ] {
        assert_eq!(stderr.matches(old).count(), 1, "{old}: {stderr}");
    }
    assert!(stderr.contains("is deprecated; use"), "{stderr}");
    env.issue(&["comment", "1", "Thanks"]);
    fake.accept_comment(&env, 1, 100);
    let pushed = env.issue_exec(&["push"], "", &[]);
    assert!(pushed.status.success());
    let report: serde_json::Value = serde_json::from_slice(&pushed.stdout).unwrap();
    assert_eq!(
        report["trackers"][0]["sent"][0]["kind"], "comment",
        "{report}"
    );
}

#[test]
fn an_old_key_and_its_new_key_in_one_file_are_an_error() {
    let env = Env::new();
    env.write_config("[forge]\npost = true\n[sync]\npush = \"manual\"\n");
    let error = env.issue_fail(&["sync"]);
    assert!(error.contains("post") && error.contains("push"), "{error}");
    assert!(error.contains("config.toml"), "{error}");
}
