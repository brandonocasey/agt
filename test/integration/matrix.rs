//! Scenarios that hold for every service, run once per service in [`forge::all`]. What differs
//! between services (the kinds of write an in-diff comment takes, whether a reply to a general
//! comment is native, how an approval is given) is read from the service's `Expect`, so a
//! scenario names a behavior and each service answers for its own capabilities.
//!
//! A service's tests are enabled by adding it to the `matrix!` list below.

use serde_json::{Value, json};

use crate::support::forge::{Call, Comment, FakeService, Remote, assert_read_only};
use crate::support::scenario::{
    Setup, ids, kinds, local_changes, marked, open_ids, skipped_items, thread, write_starting,
    write_text, writes_for,
};
use crate::support::{Env, numbered, snapshot};

type Svc<'a> = &'a dyn FakeService;

/// One `#[test]` per scenario and service.
macro_rules! matrix {
    ($($scenario:ident),* $(,)?) => {
        $(
            mod $scenario {
                #[test]
                fn github() {
                    super::$scenario(&crate::support::forge::github::Github::new());
                }

                #[test]
                fn gitlab() {
                    super::$scenario(&crate::support::forge::gitlab::Gitlab::new());
                }
            }
        )*
    };
}

matrix!(
    sync_is_read_only_and_a_second_sync_imports_nothing,
    publish_posts_every_payload_once,
    publish_refuses_a_pr_by_someone_else,
    not_own_publish_posts_only_replies_and_resolves_on_others_threads,
    own_published_general_thread_stays_open,
    finding_kill_refuses_a_posting_finding,
    sync_never_reuses_a_killed_finding_id,
    comments_before_first_sync_are_mine,
    target_without_local_branch_uses_pr_head,
    publish_target_on_detached_head,
    reply_on_general_finding_follows_the_reply_capability,
    locked_pass_findings_wait_for_reveal,
    rejected_finding_returns_to_pending_and_can_be_killed,
    stale_unposted_findings_are_not_posted,
    run_end_releases_locked_findings,
    my_unmarked_comments_are_not_open,
    target_on_detached_rebased_pr_head_uses_head,
    sync_from_detached_head_adopts_the_branch_target,
    foreign_marker_never_claims_a_pending_comment,
    record_counts_reviews_like_status,
    records_are_imported_only_when_edits_are_attributed,
    rename_merge_keeps_colliding_human_threads,
    runs_ended_before_the_pr_are_never_published,
    local_reply_is_never_published,
    not_own_findings_posts_findings_and_my_threads_but_no_record,
    only_limits_publish_to_named_threads,
    comments_outside_the_pr_diff_are_never_posted_unless_on_another_file,
    approvals_post_a_comment_unless_formal,
    formal_approval_on_someone_elses_pr,
    approval_of_an_old_head_is_not_posted,
    approving_twice_posts_once_and_reopened_findings_hold_it,
    rejected_findings_resolve_with_a_reason_and_count_in_the_record,
    a_remote_unresolve_clears_the_fixed_outcome,
);

/// The kinds of the writes of a publish that posts one in-diff comment, then `rest`.
fn inline_then(svc: Svc, rest: &[&str]) -> Vec<String> {
    svc.expect()
        .inline()
        .into_iter()
        .chain(rest.iter().copied())
        .map(str::to_owned)
        .collect()
}

fn publish(env: &Env, svc: Svc, extra: &[&str]) -> Value {
    let mut args = vec!["publish", "--pr", svc.number()];
    args.extend(extra);
    env.run(&args)
}

fn dry_run(env: &Env, svc: Svc, extra: &[&str]) -> Value {
    let mut args = vec!["--dry-run"];
    args.extend(extra);
    publish(env, svc, &args)
}

fn sync(env: &Env, svc: Svc) -> Value {
    env.run(&["sync", "--pr", svc.number()])
}

/// A sync adopts the PR's key for the branch's, reads and never writes, and finds nothing new
/// the second time.
fn sync_is_read_only_and_a_second_sync_imports_nothing(svc: Svc) {
    let setup = Setup::synced_branch(svc.rich_origin());
    let env = &setup.env;
    // Local: the host has no answers yet and this test counts the calls of `sync`.
    let branch_key =
        env.run_with(&["status", "--full"], None, &[("AGT_OFFLINE", "1")])["target"].clone();
    svc.rich_sync(&setup);
    let envs = svc.rich_env();

    let synced = env.run_with(&["sync"], None, &envs);
    assert_eq!(synced["target"], svc.target());
    assert_eq!(synced["renamed_from"], branch_key);
    assert_eq!(synced["login"], "me");
    assert_read_only(svc, env);
    let status = env.run(&["status", "--full"]);
    assert_eq!(status["target"], svc.target());
    assert_eq!(status["runs"][0]["id"], setup.run.as_str());

    let again = env.run_with(&["sync"], None, &envs);
    assert_eq!(again["threads"]["imported"], 0);
    assert_eq!(again["threads"]["total"], synced["threads"]["total"]);
    assert!(again["renamed_from"].is_null());
}

fn publish_posts_every_payload_once(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let [before, posted, done] = svc.stages(&setup);
    svc.sync(&setup, &before);
    sync(env, svc);
    local_changes(
        &setup,
        &svc.local_thread(svc.alice_thread()),
        &svc.local_general(svc.alice_general()),
    );

    let state = snapshot(&env.state());
    let calls = env.forge_calls().len();
    let dry = dry_run(env, svc, &[]);
    assert_eq!(snapshot(&env.state()), state, "a dry run writes nothing");
    svc.assert_dry_reads(env, calls);
    let every_write = inline_then(
        svc,
        &[
            "thread",
            "thread",
            "reply",
            "reply",
            "resolve",
            "unresolve",
            "summary",
        ],
    );
    assert_eq!(kinds(&dry, "writes"), every_write);
    svc.assert_dry_run(&setup, &dry);

    env.reset_calls();
    svc.sync_with(&setup, Some(1), &before);
    svc.sync_with(&setup, Some(2), &posted);
    svc.sync(&setup, &done);
    svc.publish_writes(env, &setup);
    let published = publish(env, svc, &[]);
    assert_eq!(kinds(&published, "posted"), every_write);
    svc.assert_posted(&setup);

    let again = publish(env, svc, &[]);
    assert_eq!(again["posted"], json!([]), "nothing is posted twice");
    svc.assert_counts_once(env);
}

fn publish_refuses_a_pr_by_someone_else(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    svc.sync(&setup, &Remote::by("alice"));
    sync(env, svc);
    env.run(&["run", "end", "--run", &setup.run]);
    let error = env.fail(&["publish", "--pr", svc.number()], None);
    assert!(error.contains("authored by alice, not me"), "{error}");
    assert!(env.forge_calls().iter().all(|call| !call.contains("POST")));
}

fn not_own_publish_posts_only_replies_and_resolves_on_others_threads(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let mut remote = Remote::by("alice");
    remote.thread(
        "t-a",
        false,
        3,
        vec![Comment::new(301, "alice", "Alice asks why.")],
    );
    svc.sync(&setup, &remote);
    sync(env, svc);
    env.run(&["comment", "--body", "My own note.", "--author", "me"]);
    let alice = svc.local_thread(301);
    env.run(&["reply", &alice, "--body", "Because of X.", "--author", "me"]);
    env.run(&["resolve", &alice, "--resolution", "answered"]);
    env.run(&["run", "end", "--run", &setup.run]);
    let dry = dry_run(env, svc, &["--not-own"]);
    assert_eq!(kinds(&dry, "writes"), ["reply", "resolve"], "{dry}");
    let skipped = skipped_items(&dry);
    for item in [setup.finding(1), "h1".to_owned(), "summary".to_owned()] {
        assert!(skipped.contains(&item), "{item}: {dry}");
    }
}

fn own_published_general_thread_stays_open(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    env.run(&[
        "comment",
        "--body",
        "General note from me.",
        "--author",
        "me",
    ]);
    let mut remote = svc.before(&setup);
    remote.general(
        "n-602",
        Comment::new(602, "me", &marked("General note from me.", "h1/c1")),
    );
    svc.sync(&setup, &remote);
    sync(env, svc);
    let open = open_ids(&env.run(&["status"]));
    assert!(open.contains(&"h1".to_owned()), "{open:?}");
    assert!(
        !open.contains(&svc.local_general(svc.alice_general())),
        "{open:?}"
    );
    let listed = ids(&env.run(&["list", "--open"]));
    assert_eq!(listed, open, "list --open and status agree");
}

fn finding_kill_refuses_a_posting_finding(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    svc.link(&setup);
    svc.lose(env, Call::PostInline);
    env.fail(&["publish", "--pr", svc.number()], None);
    let error = env.fail(
        &["finding", "kill", &setup.finding(1), "--reason", "late"],
        None,
    );
    assert!(error.contains("resolve"), "{error}");
}

fn sync_never_reuses_a_killed_finding_id(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f2 = setup.finding(2);
    env.run(&["finding", "kill", &f2, "--reason", "wrong"]);
    let mut remote = Remote::new();
    let body = format!(
        "**nit:** x\n<!-- review-finding v1 id={f2} sha={} -->",
        setup.head
    );
    remote.general("n-601", Comment::new(601, "me", &body));
    svc.sync(&setup, &remote);
    sync(env, svc);
    let list = env.run(&["list"]);
    assert!(!ids(&list).contains(&f2), "{list}");
    assert!(ids(&list).contains(&svc.local_general(601)));
}

fn comments_before_first_sync_are_mine(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    env.run(&["comment", "--body", "Written before sync."]);
    svc.link(&setup);
    let dry = dry_run(env, svc, &[]);
    assert_eq!(dry["skipped"], json!([]), "{dry}");
    assert_eq!(writes_for(&dry, "h1/c1"), ["thread"]);
    sync(env, svc);
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, "h1")["comments"][0]["author"]["login"], "me");
}

fn target_without_local_branch_uses_pr_head(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    svc.sync(&setup, &Remote::new());
    sync(env, svc);
    env.git(&["switch", "-q", "--detach", "main"]);
    env.git(&["branch", "-q", "-D", "feat"]);
    let status = env.run(&["--target", svc.target(), "status"]);
    assert_eq!(status["head"], setup.head);
}

fn publish_target_on_detached_head(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    svc.link(&setup);
    sync(env, svc);
    env.git(&["switch", "-q", "--detach"]);
    let dry = env.run(&["--target", svc.target(), "publish", "--dry-run"]);
    assert_eq!(
        kinds(&dry, "writes"),
        inline_then(svc, &["thread", "summary"])
    );
}

/// A reply to a general finding is a native reply where the service has them for general
/// comments, else a general comment ending in `Re: <finding>`.
fn reply_on_general_finding_follows_the_reply_capability(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f2 = setup.finding(2);
    // Both findings land with lost responses, so sync knows them by their markers.
    let (mut remote, general) = svc.land_findings(&setup);
    env.run(&[
        "reply",
        &f2,
        "--body",
        "Not fixed: needs a design change.",
        "--author",
        "me",
    ]);

    let native = svc.expect().replies_to_general_natively();
    env.reset_calls();
    svc.sync_with(&setup, Some(1), &remote);
    svc.sync_with(&setup, Some(2), &remote);
    let text = marked("Not fixed: needs a design change.", &format!("{f2}/c2"));
    let answer = if native {
        text
    } else {
        format!("{text}\nRe: {f2}")
    };
    remote.add_reply(&general, Comment::new(603, "me", &answer));
    svc.sync(&setup, &remote);
    if native {
        svc.accept(env, Call::Reply(&general), 603);
        svc.accept(env, Call::PostNote, 604);
    } else {
        svc.accept_nth(env, Call::PostNote, 1, 603);
        svc.accept_nth(env, Call::PostNote, 2, 604);
    }
    let published = publish(env, svc, &[]);
    assert_eq!(kinds(&published, "posted"), ["reply", "summary"]);
    let inputs = env.forge_inputs();
    let (_, posted) = inputs
        .iter()
        .find(|(_, input)| input.contains("Not fixed"))
        .unwrap();
    let posted: Value = serde_json::from_str(posted).unwrap();
    let posted = posted["body"].as_str().unwrap();
    assert!(
        posted.starts_with(&format!(
            "Not fixed: needs a design change.\n<!-- agent-review id={f2}/c2 attempt=at-"
        )),
        "{posted}"
    );
    if native {
        assert!(posted.ends_with(" -->"), "{posted}");
    } else {
        assert!(posted.ends_with(&format!(" -->\nRe: {f2}")), "{posted}");
    }
    let list = env.run(&["list"]);
    let general = thread(&list, &f2);
    assert_eq!(general["comments"].as_array().unwrap().len(), 2);
    assert_eq!(general["comments"][1]["remote"]["id"], "603");
    assert!(!ids(&list).contains(&svc.local_general(603)));
}

fn locked_pass_findings_wait_for_reveal(svc: Svc) {
    let setup = Setup::locked(svc.origin());
    let env = &setup.env;
    svc.sync(&setup, &Remote::new());
    let held = dry_run(env, svc, &[]);
    assert_eq!(
        writes_for(&held, &format!("{}/c1", setup.finding(1))),
        Vec::<String>::new()
    );
    assert!(
        held["skipped"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("reveal"),
        "{held}"
    );
    let reveal = [
        "pass",
        "start",
        "--run",
        &setup.run,
        "--kind",
        "reveal",
        "--reviewers",
        "-",
    ];
    env.run_stdin(&reveal, crate::support::scenario::PUBLISH.reviewers);
    env.run(&["pass", "end", "--run", &setup.run, "--pass", "c1p1"]);
    let shown = dry_run(env, svc, &[]);
    assert_eq!(
        writes_for(&shown, &format!("{}/c1", setup.finding(1))),
        svc.expect().inline()
    );
}

fn rejected_finding_returns_to_pending_and_can_be_killed(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f1 = setup.finding(1);
    svc.link(&setup);
    svc.lose(env, Call::PostInline);
    env.fail(&["publish", "--pr", svc.number()], None);
    env.fail(&["publish", "--pr", svc.number()], None);
    sync(env, svc);
    let list = env.run(&["list"]);
    let first = &thread(&list, &f1)["comments"][0];
    assert_eq!(first["post"], "pending", "no marker found: back to pending");
    assert!(first["attempt"].is_string(), "the attempt id stays");
    env.run(&["finding", "kill", &f1, "--reason", "the host rejects it"]);
}

fn stale_unposted_findings_are_not_posted(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f2 = setup.finding(2);
    env.run(&["resolve", &f2, "--triage", "stale"]);
    svc.link(&setup);
    let dry = dry_run(env, svc, &[]);
    assert_eq!(writes_for(&dry, &format!("{f2}/c1")), Vec::<String>::new());
    assert_eq!(kinds(&dry, "writes"), inline_then(svc, &["summary"]));
}

fn run_end_releases_locked_findings(svc: Svc) {
    let setup = Setup::locked(svc.origin());
    let env = &setup.env;
    svc.link(&setup);
    let dry = dry_run(env, svc, &[]);
    assert_eq!(
        kinds(&dry, "writes"),
        inline_then(svc, &["thread", "summary"])
    );
}

fn my_unmarked_comments_are_not_open(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let mut remote = Remote::new();
    remote.general("n-950", Comment::new(950, "me", "PTAL"));
    remote
        .reviews
        .extend(svc.plain_review(751, "me", "Looked at the retry path."));
    svc.sync(&setup, &remote);
    sync(env, svc);
    env.run(&["comment", "--body", "Local note.", "--author", "me"]);
    let open = open_ids(&env.run(&["status"]));
    assert!(!open.contains(&svc.local_general(950)), "{open:?}");
    let review = format!("{}-r751", &svc.target()[..2]);
    assert!(!open.contains(&review), "{open:?}");
    assert!(open.contains(&"h1".to_owned()), "{open:?}");
}

fn target_on_detached_rebased_pr_head_uses_head(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    svc.sync(&setup, &Remote::new());
    sync(env, svc);
    env.git(&["switch", "-q", "--detach", "main"]);
    env.write("a.txt", &numbered(10, &[(3, "THREE")]));
    let pushed = env.commit_all("force-pushed PR head");
    svc.sync(&setup, &Remote::new().head(pushed));
    env.run(&["--target", svc.target(), "sync"]);
    env.write("a.txt", &numbered(10, &[(3, "THREE"), (5, "five")]));
    let fix = env.commit_all("fix on the pushed head");
    assert_eq!(env.run(&["--target", svc.target(), "status"])["head"], fix);
}

fn sync_from_detached_head_adopts_the_branch_target(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let branch_key = env.run(&["status"])["target"].as_str().unwrap().to_owned();
    env.git(&["switch", "-q", "--detach"]);
    svc.sync(&setup, &Remote::new());
    let synced = sync(env, svc);
    assert_eq!(synced["target"], svc.target());
    assert_eq!(synced["renamed_from"], branch_key);
    let list = env.run(&["--target", svc.target(), "list"]);
    assert!(ids(&list).contains(&setup.finding(1)), "{list}");
}

fn foreign_marker_never_claims_a_pending_comment(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    env.run(&["comment", "--body", "Local pending note.", "--author", "me"]);
    let mut remote = Remote::new();
    remote.general(
        "n-950",
        Comment::new(950, "me", &marked("Other store text.", "h1/c1")),
    );
    svc.sync(&setup, &remote);
    sync(env, svc);
    let list = env.run(&["list"]);
    let local = &thread(&list, "h1")["comments"][0];
    assert_eq!(local["body"], "Local pending note.");
    assert_eq!(local["post"], "pending");
    assert_eq!(local["remote"], Value::Null);
    assert!(ids(&list).contains(&svc.local_general(950)), "{list}");
}

fn record_counts_reviews_like_status(svc: Svc) {
    let env = Env::with_branch();
    env.git(&["remote", "add", "origin", svc.origin()]);
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("C0");
    let base = env.git(&["rev-parse", "main"]);
    env.write("a.txt", &numbered(10, &[(3, "three"), (6, "six")]));
    let run = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    crate::support::scenario::add_pass(&env, &run, &crate::support::scenario::PUBLISH);
    env.run(&["pass", "end", "--run", &run, "--pass", "c1p0"]);
    let head = env.commit_all("commit exactly the reviewed tree");
    let setup = Setup {
        env,
        run,
        head,
        base,
    };
    svc.link(&setup);
    assert_eq!(setup.env.run(&["status"])["reviews_at_head"], 1);
    let dry = dry_run(&setup.env, svc, &[]);
    let summary = write_starting(&dry, "body", "**⚠️ Agent review: ");
    assert!(
        !summary.contains("not posted"),
        "the threads this publish posts show as posted: {summary}"
    );
    assert!(
        summary.starts_with(&format!(
            "**⚠️ Agent review: 2 open at {}.** 2 findings: 2 open.",
            &setup.head[..7]
        )),
        "{summary}"
    );
    assert!(
        summary.contains(&format!(
            "<!-- review-record v2 id={} head={} reviews-at-head=1 ",
            setup.run, setup.head
        )),
        "{summary}"
    );
}

/// A host that shows no edits cannot vouch for a record, so only text this store sent counts.
fn records_are_imported_only_when_edits_are_attributed(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let record = format!(
        "<!-- review-record v2 id=rv-zzzz head={} reviews-at-head=2 open=0 -->\n<!-- agent-review id=rv-zzzz/record -->",
        setup.head
    );
    let mut remote = Remote::new();
    remote.general("d-r", Comment::new(6001, "me", &record));
    svc.sync(&setup, &remote);
    let synced = sync(env, svc);
    let imported = usize::from(svc.expect().edits_attributed());
    assert_eq!(
        synced["records"], imported,
        "only a service that shows edits trusts a record it did not send"
    );
    assert_eq!(
        env.run(&["status", "--full"])["imported_records"]
            .as_array()
            .unwrap()
            .len(),
        imported
    );
}

fn rename_merge_keeps_colliding_human_threads(svc: Svc) {
    let env = Env::with_branch();
    env.git(&["remote", "add", "origin", svc.origin()]);
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    let head = env.commit_all("change");
    let base = env.git(&["rev-parse", "main"]);
    let setup = Setup {
        env,
        run: String::new(),
        head,
        base,
    };
    let env = &setup.env;
    svc.sync(&setup, &Remote::new());
    env.git(&["switch", "-q", "--detach"]);
    sync(env, svc);
    env.run(&[
        "--target",
        svc.target(),
        "comment",
        "--body",
        "Detached note.",
        "--author",
        "me",
    ]);
    env.git(&["switch", "-q", "feat"]);
    env.run(&["comment", "--body", "Branch note.", "--author", "me"]);
    sync(env, svc);
    let list = env.run(&["--target", svc.target(), "list"]);
    let mut texts: Vec<&str> = list["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["comments"][0]["body"].as_str().unwrap())
        .collect();
    texts.sort_unstable();
    assert_eq!(texts, ["Branch note.", "Detached note."], "{list}");
}

fn runs_ended_before_the_pr_are_never_published(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let (f1, f2) = (setup.finding(1), setup.finding(2));
    env.run(&[
        "resolve",
        &f1,
        "--resolution",
        &format!("fixed {}", setup.head),
    ]);
    env.run(&["run", "end", "--run", &setup.run]);
    let later = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    svc.sync(&setup, &Remote::new());
    env.run_with(
        &["sync", "--pr", svc.number()],
        None,
        &[("AGT_NOW", &later.to_string())],
    );
    let dry = dry_run(env, svc, &[]);
    assert_eq!(dry["writes"], json!([]), "{dry}");
    let skipped = skipped_items(&dry);
    assert!(skipped.contains(&f1) && skipped.contains(&f2), "{dry}");
}

fn local_reply_is_never_published(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f1 = setup.finding(1);
    svc.link(&setup);
    let dry = dry_run(env, svc, &[]);
    assert_eq!(
        writes_for(&dry, &format!("{f1}/c1")).len(),
        svc.expect().inline().len(),
        "{dry}"
    );
    env.run(&[
        "reply",
        &f1,
        "--local",
        "--body",
        "Answer to the held question.",
        "--author",
        "me",
    ]);
    let dry = dry_run(env, svc, &[]);
    assert_eq!(
        writes_for(&dry, &format!("{f1}/c2")),
        Vec::<String>::new(),
        "{dry}"
    );
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f1)["comments"][1]["local"], true);
    let prompt = env.text(&["list", "--format", "prompt"]);
    assert!(prompt.contains("Answer to the held question."), "{prompt}");
}

fn not_own_findings_posts_findings_and_my_threads_but_no_record(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    svc.sync(&setup, &Remote::by("alice"));
    sync(env, svc);
    env.run(&["comment", "--body", "My own note.", "--author", "me"]);
    env.run(&["run", "end", "--run", &setup.run]);
    let only = format!("{},h1", setup.finding(1));
    let dry = dry_run(env, svc, &["--not-own", "--findings", "--only", &only]);
    assert_eq!(
        kinds(&dry, "writes"),
        inline_then(svc, &["thread"]),
        "{dry}"
    );
    assert_eq!(writes_for(&dry, "h1/c1"), ["thread"]);
    let skipped = dry["skipped"].to_string();
    assert!(skipped.contains("\"summary\""), "{dry}");
    assert!(
        !dry.to_string().contains("The summary comment lists"),
        "{dry}"
    );
}

fn only_limits_publish_to_named_threads(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    svc.sync(&setup, &Remote::by("alice"));
    sync(env, svc);
    env.run(&["run", "end", "--run", &setup.run]);
    let second = env.run(&["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    crate::support::scenario::add_pass(env, &second, &crate::support::scenario::PUBLISH);
    env.run(&["pass", "end", "--run", &second, "--pass", "c1p0"]);
    let wanted = format!("{second}.c1p0.r1.f1");

    let error = env.fail(
        &[
            "publish",
            "--pr",
            svc.number(),
            "--not-own",
            "--findings",
            "--dry-run",
        ],
        None,
    );
    assert!(error.contains("--only"), "{error}");
    let dry = dry_run(env, svc, &["--not-own", "--findings", "--only", &wanted]);
    let items: Vec<String> = dry["writes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|write| write["items"].as_array().unwrap().clone())
        .map(|item| item.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        items,
        vec![format!("{wanted}/c1"); svc.expect().inline().len()],
        "{dry}"
    );
    let skipped = dry["skipped"].to_string();
    assert!(skipped.contains(&setup.finding(1)), "{dry}");
}

fn comments_outside_the_pr_diff_are_never_posted_unless_on_another_file(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let started = env.run_stdin(
        &["run", "start", "--kind", "full", "--reviewers", "-"],
        crate::support::scenario::PUBLISH.reviewers,
    );
    let run = started["run"].as_str().unwrap();
    let ended = env.run_stdin(
        &[
            "pass",
            "end",
            "--run",
            run,
            "--pass",
            "c1p0",
            "--findings",
            "-",
            "--reviewer",
            "r1",
        ],
        r#"[
            {"type":"nit","path":"a.txt","line":9,"scenario":"Out of the diff."},
            {"type":"nit","path":"a.txt","start_line":1,"line":3,"scenario":"Range in one hunk."},
            {"type":"nit","path":"lib/b.txt","line":2,"scenario":"A file the PR leaves alone."}
        ]"#,
    );
    let outside = &ended["added"][0];
    // Link the PR before the run ends, so the run counts for it.
    svc.link(&setup);
    env.run(&["run", "end", "--run", run]);
    let dry = dry_run(env, svc, &[]);
    let skipped = dry["skipped"].as_array().unwrap();
    assert!(
        skipped.iter().any(|item| &item["item"] == outside
            && item["reason"]
                .as_str()
                .unwrap()
                .contains("not in the PR/MR diff")),
        "{dry}"
    );
    assert!(svc.expect().ranges());
    svc.assert_range(&dry, 1, 3, "Range in one hunk.");
    // A file the PR does not change gets a general comment naming the location.
    let general = write_text(&dry, "body", "A file the PR leaves alone.");
    assert!(
        general.starts_with("`lib/b.txt:2`\n\n**✏️ nit:**"),
        "{general}"
    );
}

fn approvals_post_a_comment_unless_formal(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f1 = setup.finding(1);
    let error = env.fail(&["approve"], None);
    assert!(
        error.contains(&format!("open findings block approval: {f1}")),
        "{error}"
    );
    env.run(&["resolve", &f1, "--triage", "stale"]);
    let approved = env.run(&["approve"]);
    let id = approved["thread"]["id"].as_str().unwrap().to_owned();
    let open = env.run(&["status"])["open_threads"].to_string();
    assert!(
        !open.contains(&format!("\"{id}\"")),
        "an approval is never open: {open}"
    );
    // A formal approval of the same head upgrades the comment approval: one comment.
    let formal = env.run(&["approve", "--formal"])["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(formal, id);
    svc.link(&setup);
    let dry = dry_run(env, svc, &[]);
    let comment = dry["writes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|write| write["items"] == json!([format!("{id}/c1")]))
        .unwrap_or_else(|| panic!("{dry}"));
    assert_eq!(comment["call"], json!(svc.call(Call::PostNote)));
    let text = comment["input"]["body"].as_str().unwrap();
    assert!(
        text.starts_with(&format!("Approved at {}.", &setup.head[..12])),
        "{text}"
    );
    if svc.expect().approves_by_review() && !svc.expect().self_approval_allowed() {
        // On your own PR a formal approval posts as a comment, with a note on the formal part.
        let skipped = dry["skipped"].as_array().unwrap();
        assert!(
            skipped
                .iter()
                .any(|item| item["item"] == format!("{formal}/formal")
                    && item["reason"]
                        .as_str()
                        .unwrap()
                        .contains("approve their own PR")),
            "{dry}"
        );
    }
    if svc.expect().approves_by_call() {
        assert_eq!(writes_for(&dry, &formal), ["approve"], "{dry}");
    }
    assert_eq!(approval_writes(&dry), 1, "{dry}");
}

/// Approval text in the writes of a dry run.
fn approval_writes(dry: &Value) -> usize {
    dry["writes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|write| {
            write["input"]["body"]
                .as_str()
                .is_some_and(|body| body.starts_with("Approved at"))
        })
        .count()
}

fn formal_approval_on_someone_elses_pr(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    env.run(&["resolve", &setup.finding(1), "--triage", "stale"]);
    let id = env.run(&["approve", "--formal"])["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    svc.sync(&setup, &Remote::by("alice"));
    sync(env, svc);
    let dry = dry_run(env, svc, &["--not-own", "--only", &id]);
    let writes = dry["writes"].as_array().unwrap();
    if svc.expect().approves_by_review() {
        assert_eq!(writes.len(), 1, "{dry}");
        assert_eq!(writes[0]["input"]["event"], "APPROVE");
        assert_eq!(writes[0]["input"]["commit_id"], setup.head.as_str());
    } else {
        assert_eq!(kinds(&dry, "writes"), ["approve", "thread"], "{dry}");
        assert_eq!(writes[0]["input"]["sha"], setup.head.as_str());
        assert_eq!(writes[0]["call"], json!(svc.call(Call::Approve)));
    }
}

fn approval_of_an_old_head_is_not_posted(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    env.run(&["resolve", &setup.finding(1), "--triage", "stale"]);
    let id = env.run(&["approve"])["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    svc.sync(&setup, &Remote::new().head(env.git(&["rev-parse", "main"])));
    sync(env, svc);
    let dry = dry_run(env, svc, &[]);
    assert!(
        dry["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["item"] == id.as_str()
                && item["reason"]
                    .as_str()
                    .unwrap()
                    .contains("PR/MR head is now")),
        "{dry}"
    );
}

fn approving_twice_posts_once_and_reopened_findings_hold_it(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f1 = setup.finding(1);
    env.run(&["resolve", &f1, "--triage", "stale"]);
    let first = env.run(&["approve"])["thread"]["id"].clone();
    assert_eq!(env.run(&["approve"])["thread"]["id"], first);
    svc.link(&setup);
    let dry = dry_run(env, svc, &[]);
    assert_eq!(approval_writes(&dry), 1, "{dry}");
    env.run(&["resolve", &f1, "--reopen"]);
    let dry = dry_run(env, svc, &[]);
    assert_eq!(approval_writes(&dry), 0, "{dry}");
    assert!(
        dry["skipped"]
            .to_string()
            .contains("open findings block approval"),
        "{dry}"
    );
}

fn rejected_findings_resolve_with_a_reason_and_count_in_the_record(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f1 = setup.finding(1);
    assert!(
        env.fail(&["resolve", &f1, "--resolution", "rejected:"], None)
            .contains("rejected reason must not be empty")
    );
    let thread = &env.run(&["resolve", &f1, "--rejected", "Intended behavior."])["thread"];
    assert_eq!(thread["resolved"], true);
    assert_eq!(thread["resolution"], "rejected: Intended behavior");
    let replies = thread["comments"].as_array().unwrap();
    assert_eq!(
        replies.last().unwrap()["body"],
        "🚫 Rejected: Intended behavior."
    );
    let open = env.run(&["status"])["open_threads"].to_string();
    assert!(!open.contains(&f1), "{open}");
    svc.link(&setup);
    let dry = dry_run(env, svc, &[]);
    let summary = write_starting(&dry, "body", "**⚠️ Agent review: ");
    assert!(
        summary.contains("2 findings: 1 rejected, 1 open."),
        "{summary}"
    );
    assert!(
        summary.contains("| 🚫 Rejected: Intended behavior |"),
        "{summary}"
    );
}

fn a_remote_unresolve_clears_the_fixed_outcome(svc: Svc) {
    let setup = Setup::reviewed(svc.origin());
    let env = &setup.env;
    let f1 = setup.finding(1);
    // The finding lands as this store sent it, so sync claims it on every service.
    let (mut remote, _) = svc.land_findings(&setup);
    let list = env.run(&["list"]);
    let id = thread(&list, &f1)["remote"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("{list}"))
        .to_owned();
    let fixed = env.run(&["resolve", &f1, "--fixed", &setup.head]);
    let reply = fixed["thread"]["comments"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["body"]
        .as_str()
        .unwrap()
        .to_owned();
    let subject = env.git(&["log", "-1", "--format=%s", &setup.head]);
    assert!(
        reply.starts_with(&format!(
            "✅ Fixed in [{} {}](",
            &setup.head[..7],
            subject.trim()
        )) && reply.ends_with(&format!("/commit/{}).", setup.head)),
        "a fix reply links the commit once the PR is linked: {reply}"
    );
    remote.set_resolved(&id, true);
    svc.sync(&setup, &remote);
    sync(env, svc);
    let list = env.run(&["list"]);
    assert_eq!(thread(&list, &f1)["resolved"], true);
    remote.set_resolved(&id, false);
    svc.sync(&setup, &remote);
    sync(env, svc);
    let list = env.run(&["list"]);
    let synced = thread(&list, &f1);
    assert_eq!(synced["resolved"], false, "{synced}");
    assert_eq!(synced["resolution"], Value::Null, "{synced}");
    let dry = dry_run(env, svc, &[]);
    let summary = write_starting(&dry, "body", "**⚠️ Agent review: ");
    assert!(summary.contains("2 findings: 2 open."), "{summary}");
}
