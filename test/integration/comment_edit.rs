//! `edit` and `delete` of unposted comments change the store alone.
//! This file also holds what `publish` sends for posted ones on one service.
//! The matrix has the rest.

use serde_json::{Value, json};

use crate::support::forge::argv::{gh_write, tea_bodyless, tea_get, tea_write};
use crate::support::forge::forgejo::{self, Forgejo};
use crate::support::forge::github::{self, Github, bodyless};
use crate::support::forge::gitlab::Gitlab;
use crate::support::forge::{Call, Comment, FakeService, Remote};
use crate::support::scenario::{
    PUBLISH, Setup, count_calls, ids, input_of, kinds, sent, skipped_items, start_review, thread,
    write_text,
};
use crate::support::{BIN, Env, numbered, strs};

/// A branch with the thread h1 holding my comment and a reply to it.
fn thread_with_a_reply() -> Env {
    let env = Env::with_branch();
    env.run(&["comment", "--body", "Opening.", "--author", "me"]);
    env.run(&["reply", "h1", "--body", "Answer.", "--author", "me"]);
    env
}

fn bodies(env: &Env) -> Vec<String> {
    let list = env.run(&["list"]);
    thread(&list, "h1")["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|comment| comment["body"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn edit_rewrites_an_unposted_comment_from_text_or_stdin() {
    let env = thread_with_a_reply();
    let edited = env.run(&["edit", "h1/c2", "--body", "Better answer."]);
    assert_eq!(edited["thread"]["comments"][1]["body"], "Better answer.");
    assert_eq!(bodies(&env), ["Opening.", "Better answer."]);
    env.run_stdin(&["edit", "h1/c1", "--body", "-"], "From stdin.\n");
    assert_eq!(bodies(&env), ["From stdin.\n", "Better answer."]);
    let list = env.run(&["list"]);
    assert!(
        thread(&list, "h1")["comments"][0]
            .get("edit_pending")
            .is_none(),
        "an unposted edit leaves nothing pending: {list}"
    );
}

#[test]
fn delete_removes_an_unposted_reply_and_keeps_the_thread() {
    let env = thread_with_a_reply();
    let deleted = env.run(&["delete", "h1/c2"]);
    assert_eq!(deleted["deleted"], "h1/c2");
    assert_eq!(deleted["pending"], false);
    assert_eq!(deleted["thread_removed"], false);
    assert_eq!(bodies(&env), ["Opening."]);
}

#[test]
fn deleting_the_opener_of_an_unposted_thread_removes_the_thread() {
    let env = thread_with_a_reply();
    env.run(&["comment", "--body", "Another thread.", "--author", "me"]);
    let deleted = env.run(&["delete", "h1/c1"]);
    assert_eq!(deleted["thread_removed"], true);
    assert_eq!(ids(&env.run(&["list"])), ["h2"]);
}

#[test]
fn an_unknown_comment_is_refused_and_a_thread_id_is_not_a_comment_id() {
    let env = thread_with_a_reply();
    for id in ["h1/c9", "h1", "h7/c1"] {
        let error = env.fail(&["edit", id, "--body", "x"], None);
        assert!(
            error.contains(&format!("comment {id} not found")),
            "{error}"
        );
        let error = env.fail(&["delete", id], None);
        assert!(
            error.contains(&format!("comment {id} not found")),
            "{error}"
        );
    }
    assert_eq!(bodies(&env), ["Opening.", "Answer."]);
}

#[test]
fn an_empty_edit_is_refused_like_an_empty_comment() {
    let env = thread_with_a_reply();
    let error = env.fail(&["edit", "h1/c1", "--body", ""], None);
    assert!(error.contains("body"), "{error}");
    assert_eq!(bodies(&env), ["Opening.", "Answer."]);
}

/// A comment being posted may or may not have reached the host.
/// Edit and delete refuse it until a sync settles it.
#[test]
fn a_comment_being_posted_is_refused() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let comment = format!("{}/c1", setup.finding(1));
    svc.link(&setup);
    svc.lose(env, Call::PostInline);
    env.fail(&["publish", "--pr", svc.number()], None);
    for args in [
        vec!["edit", &comment, "--body", "x"],
        vec!["delete", &comment],
    ] {
        let error = env.fail(&args, None);
        assert!(
            error.contains(&format!("comment {comment} is being posted")),
            "{error}"
        );
    }
}

const PR: &str = "42";

fn issue_comment(id: u64) -> String {
    format!("repos/me/repo/issues/comments/{id}")
}

fn dry_run(env: &Env, only: &str, extra: &[&str]) -> Value {
    let mut args = vec!["publish", "--pr", PR, "--dry-run", "--only", only];
    args.extend(extra);
    env.run(&args)
}

fn skipped_reason<'a>(dry: &'a Value, item: &str) -> &'a str {
    dry["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .find(|skip| skip["item"] == item)
        .unwrap_or_else(|| panic!("{item} is not skipped: {dry}"))["reason"]
        .as_str()
        .unwrap()
}

/// A branch on GitHub and what the host shows of it.
struct Posted {
    svc: Github,
    setup: Setup,
    remote: Remote,
}

impl Posted {
    /// Posts comment `index` of thread `id`, and the host loses the answer.
    /// Then syncs it back as host comment `host_id`.
    fn land(&mut self, id: &str, index: usize, host_id: u64) {
        let (svc, env) = (&self.svc, &self.setup.env);
        svc.lose(env, Call::PostThread);
        env.fail(&["publish", "--pr", PR, "--only", id], None);
        let text = thread(&env.run(&["list"]), id)["comments"][index]["sent"]
            .as_str()
            .unwrap()
            .to_owned();
        let comment = Comment::new(host_id, "me", &text);
        self.remote.general(&format!("n-{host_id}"), comment);
        svc.sync(&self.setup, &self.remote);
        env.pull(&["sync", "--pr", PR]);
        svc.restore(env, Call::PostThread);
    }
}

/// Thread h1 holds my comment and my reply.
/// The host has them as general comments 602 and 603.
/// With `file`, the comment is on that file, which the PR leaves alone.
fn posted_pair(file: Option<&str>) -> Posted {
    let svc = Github::new();
    let env = Env::with_branch();
    env.git(&["remote", "add", "origin", svc.origin()]);
    if let Some(file) = file {
        env.git(&["switch", "-q", "main"]);
        env.write(file, "one\ntwo\n");
        env.commit_all("other file");
        env.git(&["switch", "-q", "feat"]);
        env.git(&["merge", "-q", "--ff-only", "main"]);
    }
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let head = env.commit_all("change");
    let base = env.git(&["rev-parse", "main"]);
    let run = start_review(&env, &PUBLISH, false);
    let setup = Setup {
        env,
        run,
        head,
        base,
    };
    let env = &setup.env;
    let mut comment = vec!["comment", "--body", "Opening.", "--author", "me"];
    if let Some(file) = file {
        comment.extend(["--path", file, "--line", "2"]);
    }
    env.run(&comment);
    svc.link(&setup);
    let mut posted = Posted {
        svc,
        setup,
        remote: Remote::new(),
    };
    posted.land("h1", 0, 602);
    posted
        .setup
        .env
        .run(&["reply", "h1", "--body", "Answer.", "--author", "me"]);
    posted.land("h1", 1, 603);
    posted
}

fn comment_of(env: &Env, id: &str, index: usize) -> Value {
    thread(&env.run(&["list"]), id)["comments"][index].clone()
}

/// An edit sends the text a post would send.
/// A reply keeps the quote of its comment.
#[test]
fn editing_a_posted_reply_keeps_its_quote() {
    let posted = posted_pair(None);
    let env = &posted.setup.env;
    env.run(&["edit", "h1/c2", "--body", "Better answer."]);
    let dry = dry_run(env, "h1", &[]);
    assert_eq!(kinds(&dry, "writes"), ["edit"], "{dry}");
    assert_eq!(
        dry["writes"][0]["call"],
        json!(gh_write("PATCH", &issue_comment(603)))
    );
    let text = write_text(&dry, "body", "Better answer.");
    assert!(
        text.starts_with("> Opening.\n> — @me, [comment]("),
        "{text}"
    );
}

/// An edit sends the text a post would send.
/// A comment on an untouched file keeps its lines.
#[test]
fn editing_a_posted_comment_on_another_file_keeps_its_lines() {
    let posted = posted_pair(Some("lib/b.txt"));
    let env = &posted.setup.env;
    let first = comment_of(env, "h1", 0);
    assert!(
        first["body"].as_str().unwrap().starts_with("`lib/b.txt:2`"),
        "{first}"
    );
    env.run(&["edit", "h1/c1", "--body", "Better opening."]);
    let dry = dry_run(env, "h1", &[]);
    let text = write_text(&dry, "body", "Better opening.");
    assert!(
        text.starts_with("`lib/b.txt:2`\n\nBetter opening."),
        "{text}"
    );
}

/// A reply on a review's thread went out as a plain comment.
/// It changes as one.
#[test]
fn a_reply_on_a_review_thread_is_edited_and_deleted_as_a_general_comment() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let mut remote = Remote::new();
    remote
        .reviews
        .push(github::review(801, "alice", "COMMENTED", "Looks risky."));
    svc.sync(&setup, &remote);
    setup.env.pull(&["sync", "--pr", PR]);
    let reply = ["reply", "gh-r801", "--body", "Thanks.", "--author", "me"];
    setup.env.run(&reply);
    let mut posted = Posted { svc, setup, remote };
    posted.land("gh-r801", 1, 603);
    let env = &posted.setup.env;
    let reply = comment_of(env, "gh-r801", 1)["id"]
        .as_str()
        .unwrap()
        .to_owned();

    env.run(&["edit", &reply, "--body", "Thanks a lot."]);
    let dry = dry_run(env, "gh-r801", &[]);
    assert_eq!(kinds(&dry, "writes"), ["edit"], "{dry}");
    assert_eq!(
        dry["writes"][0]["call"],
        json!(gh_write("PATCH", &issue_comment(603)))
    );
    let text = write_text(&dry, "body", "Thanks a lot.");
    assert!(text.starts_with("> Looks risky."), "{text}");

    env.run(&["delete", &reply]);
    let dry = dry_run(env, "gh-r801", &[]);
    assert_eq!(kinds(&dry, "writes"), ["delete"], "{dry}");
    assert_eq!(
        dry["writes"][0]["call"],
        json!(bodyless("DELETE", &issue_comment(603)))
    );
}

/// Posts thread h1, a general comment, then edits and deletes it with no sync between.
/// `patch` and `delete` are the calls the service sends for its host comment.
fn a_fresh_general_comment_is_planned_before_any_sync(
    svc: &dyn FakeService,
    patch: &[String],
    delete: &[String],
) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    env.run(&["comment", "--body", "Opening.", "--author", "me"]);
    svc.link(&setup);
    // The host lists the comment under its id, but with text no sync can match to h1.
    let mut remote = Remote::new();
    remote.general("n-602", Comment::new(602, "me", "Not the posted text."));
    svc.sync(&setup, &remote);
    svc.accept(env, Call::PostThread, 602);
    let posted = env.run(&["publish", "--pr", svc.number(), "--only", "h1"]);
    assert_eq!(kinds(&posted, "posted"), ["thread"], "{posted}");
    let comment = "h1/c1";
    let plan = || {
        let args = ["publish", "--pr", svc.number(), "--dry-run", "--only", "h1"];
        env.run(&args)
    };

    env.run(&["edit", comment, "--body", "Edited."]);
    let dry = plan();
    assert_eq!(kinds(&dry, "writes"), ["edit"], "{dry}");
    assert_eq!(dry["writes"][0]["call"], json!(patch), "{dry}");
    write_text(&dry, "body", "Edited.");

    env.run(&["delete", comment]);
    let dry = plan();
    assert_eq!(kinds(&dry, "writes"), ["delete"], "{dry}");
    assert_eq!(dry["writes"][0]["call"], json!(delete));
}

#[test]
fn a_fresh_github_comment_is_edited_and_deleted_before_any_sync() {
    a_fresh_general_comment_is_planned_before_any_sync(
        &Github::new(),
        &gh_write("PATCH", &issue_comment(602)),
        &bodyless("DELETE", &issue_comment(602)),
    );
}

#[test]
fn a_fresh_forgejo_comment_is_edited_and_deleted_before_any_sync() {
    let path = "repos/me/proj/issues/comments/602";
    a_fresh_general_comment_is_planned_before_any_sync(
        &Forgejo::new(),
        &tea_write("PATCH", path),
        &tea_bodyless("DELETE", path),
    );
}

/// A reply stays when its opener is deleted.
/// Sync must not import it again.
#[test]
fn deleting_a_posted_opener_does_not_import_its_reply_twice() {
    let Posted { svc, setup, remote } = posted_pair(None);
    let env = &setup.env;
    env.run(&["delete", "h1/c1"]);
    let delete = bodyless("DELETE", &issue_comment(602));
    env.respond(&strs(&delete), "");

    // The host shows the opener until the delete is sent, and not after it.
    let mut left = Remote::new();
    left.general("n-603", Comment::new(603, "me", &remote_text(&remote, 603)));
    env.reset_calls();
    svc.sync_with(&setup, Some(1), &remote);
    svc.sync_with(&setup, Some(2), &remote);
    svc.sync(&setup, &left);
    let published = env.run(&["publish", "--pr", PR, "--only", "h1"]);
    assert_eq!(kinds(&published, "posted"), ["delete"], "{published}");
    assert_eq!(count_calls(env, &delete), 1);

    let list = env.run(&["list"]);
    let holders = list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|thread| {
            thread["comments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|comment| comment["remote"]["id"] == "603")
        })
        .count();
    assert_eq!(holders, 1, "the reply is not a second thread: {list}");
    assert!(!ids(&list).contains(&svc.local_general(603)), "{list}");
    let comments = thread(&list, "h1")["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1, "{list}");
    assert_eq!(comments[0]["id"], "h1/c2");
    assert_eq!(comments[0]["remote"]["id"], "603");
}

fn remote_text(remote: &Remote, id: u64) -> String {
    use crate::support::forge::Item;
    remote
        .items
        .iter()
        .find_map(|item| match item {
            Item::General { comments, .. } => comments.iter().find(|c| c.id == id),
            Item::Thread { .. } => None,
        })
        .unwrap_or_else(|| panic!("no comment {id}"))
        .body
        .clone()
}

/// An edit made while `publish` sends an earlier one stays pending.
/// A sync keeps its text.
#[test]
fn an_edit_made_while_publish_sends_another_stays_pending() {
    let posted = posted_pair(None);
    let env = &posted.setup.env;
    env.run(&["edit", "h1/c1", "--body", "First edit."]);
    let patch = gh_write("PATCH", &issue_comment(602));
    env.respond(&strs(&patch), r#"{"id":602}"#);
    env.hook_call(
        &strs(&patch),
        &format!("'{BIN}' review edit h1/c1 --body 'Second edit.'"),
    );

    let published = env.run(&["publish", "--pr", PR, "--only", "h1"]);
    assert_eq!(kinds(&published, "posted"), ["edit"], "{published}");
    let first = comment_of(env, "h1", 0);
    assert_eq!(first["body"], "Second edit.", "{first}");
    assert_eq!(first["edit_pending"], true, "{first}");

    env.hook_call(&strs(&patch), ":");
    env.run(&["publish", "--pr", PR, "--only", "h1"]);
    let sent = input_of(&env.forge_inputs(), &patch);
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(
        sent[1]["body"].as_str().unwrap().contains("Second edit."),
        "{sent:?}"
    );
    assert!(comment_of(env, "h1", 0).get("edit_pending").is_none());
}

/// A change to a review-body paragraph is skipped once.
/// It stays pending.
#[test]
fn a_change_to_a_review_body_paragraph_is_skipped_and_stays_pending() {
    let svc = Github::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f2 = setup.finding(2);
    let mut remote = Remote::by("alice");
    svc.sync(&setup, &remote);
    env.pull(&["sync", "--pr", PR]);
    env.run(&["run", "end", "--run", &setup.run]);
    let args = ["publish", "--pr", PR, "--not-own", "--only", f2.as_str()];
    svc.lose(env, Call::PostInline);
    env.fail(&args, None);
    let body = format!(
        "{}\n\n<!-- agent-review id=review/{} -->",
        sent(env, &f2),
        setup.head
    );
    remote
        .reviews
        .push(github::review(700, "me", "COMMENTED", &body));
    svc.sync(&setup, &remote);
    svc.restore(env, Call::PostInline);
    env.pull(&["sync", "--pr", PR]);

    let comment = format!("{f2}/c1");
    env.run(&["edit", &comment, "--body", "Better finding."]);
    let dry = env.run(&[args.as_slice(), &["--dry-run"]].concat());
    assert_eq!(dry["writes"], json!([]), "{dry}");
    let listed = skipped_items(&dry);
    assert_eq!(
        listed.iter().filter(|item| **item == comment).count(),
        1,
        "{dry}"
    );
    let reason = skipped_reason(&dry, &comment);
    assert!(reason.contains("part of a review's body"), "{reason}");
    assert_eq!(comment_of(env, &f2, 0)["edit_pending"], true);
}

/// Drafts wait for `publish`; `sync` does not settle them.
#[test]
fn a_gitlab_draft_is_to_be_published_before_it_changes() {
    let svc = Gitlab::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f1 = setup.finding(1);
    let comment = format!("{f1}/c1");
    svc.link(&setup);
    svc.lose(env, Call::PostInline);
    env.fail(&["publish", "--pr", svc.number()], None);
    let mut remote = Remote::new();
    remote
        .drafts
        .push(json!({ "id": 77, "note": sent(env, &f1) }));
    svc.sync(&setup, &remote);
    env.pull(&["sync", "--pr", svc.number()]);
    for args in [
        vec!["edit", &comment, "--body", "x"],
        vec!["delete", &comment],
    ] {
        let error = env.fail(&args, None);
        assert!(
            error.contains(&format!("comment {comment} is a draft")) && error.contains("publish"),
            "{error}"
        );
        assert!(!error.contains("run sync"), "{error}");
    }
}

/// Forgejo deletes a diff comment through its review.
/// With no review known, the delete is skipped and stays pending.
#[test]
fn a_forgejo_diff_comment_with_no_known_review_is_not_deleted() {
    let svc = Forgejo::new();
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f1 = setup.finding(1);
    let mut remote = svc.land_inline(&setup);
    // The host names no review for the comment.
    let comments = format!("{}/700/comments", forgejo::REVIEWS);
    let list = match &remote.items[0] {
        crate::support::forge::Item::Thread { comments, .. } => comments,
        crate::support::forge::Item::General { .. } => unreachable!(),
    };
    let mut node = forgejo::code_comment(&list[0], 3, false, &setup.head);
    node.as_object_mut()
        .unwrap()
        .remove("pull_request_review_id");
    env.respond(
        &strs(&tea_get(&comments)),
        &Value::Array(vec![node]).to_string(),
    );
    remote.items.clear();
    env.pull(&["sync", "--pr", svc.number()]);

    env.run(&["delete", &format!("{f1}/c1")]);
    let dry = env.run(&["publish", "--pr", svc.number(), "--dry-run", "--only", &f1]);
    assert_eq!(dry["writes"], json!([]), "{dry}");
    let reason = skipped_reason(&dry, &format!("{f1}/c1"));
    assert!(reason.contains("through its review"), "{reason}");
}
