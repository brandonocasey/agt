//! `edit` and `delete` of comments nothing has posted: they change the store alone. Posted
//! comments and what `publish` sends for them are in `matrix.rs`.

use crate::support::Env;
use crate::support::forge::github::Github;
use crate::support::forge::{Call, FakeService};
use crate::support::scenario::{Setup, ids, thread};

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

/// A comment whose post may or may not have reached the host is neither edited nor deleted
/// until a sync settles it.
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
