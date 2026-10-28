//! The fake GitLab: merge request, discussion and draft note reads, REST writes, and the
//! `glab api` argument lists the CLI transport renders for them.

use serde_json::{Value, json};

use super::super::scenario::{
    Setup, assert_never_posted, assert_summary, input_of, marked, sent, thread,
};
use super::super::{Env, args, fixture, strs};
use super::argv::{glab_get, glab_is_get, glab_paginate, glab_write};
use super::{Call, Comment, Expect, FakeService, Item, Remote, TIME};

pub const MR: &str = "projects/me%2Fproj/merge_requests/3";
pub const ORIGIN: &str = "git@gitlab.com:me/proj.git";

pub struct Gitlab {
    expect: Expect,
}

impl Gitlab {
    pub fn new() -> Self {
        Self {
            expect: Expect::load("gitlab"),
        }
    }
}

pub fn write(method: &str, path: &str) -> Vec<String> {
    glab_write(method, path)
}

pub fn read(path: &str) -> Vec<String> {
    glab_get(path)
}

/// A paginated list of `path` of the merge request `mr`.
pub fn list(mr: &str, path: &str) -> Vec<String> {
    glab_paginate(&format!("{mr}/{path}?per_page=100"))
}

/// Answers `glab api user` as `me`.
pub fn login(env: &Env) {
    env.respond(&strs(&glab_get("user")), r#"{"username":"me"}"#);
}

pub fn note(id: u64, login: &str, body: &str) -> Value {
    json!({
        "id": id,
        "body": body,
        "author": { "username": login },
        "created_at": TIME,
        "system": false,
        "resolvable": true,
        "resolved": false,
    })
}

fn notes(comments: &[Comment]) -> Vec<Value> {
    comments
        .iter()
        .map(|c| note(c.id, &c.login, &c.body))
        .collect()
}

pub fn discussion(
    id: &str,
    resolved: bool,
    line: Option<u32>,
    notes: Vec<Value>,
    setup: &Setup,
) -> Value {
    let notes: Vec<Value> = notes
        .into_iter()
        .map(|mut note| {
            note["resolved"] = json!(resolved);
            if let Some(line) = line {
                note["position"] = json!({
                    "base_sha": setup.base,
                    "start_sha": setup.base,
                    "head_sha": setup.head,
                    "old_path": "a.txt",
                    "new_path": "a.txt",
                    "new_line": line,
                    "old_line": null,
                });
            }
            note
        })
        .collect();
    json!({ "id": id, "individual_note": false, "notes": notes })
}

/// A general note on its own; a reply turns it into a discussion.
pub fn general(id: &str, note: Value) -> Value {
    let mut note = note;
    note["resolvable"] = json!(false);
    json!({ "id": id, "individual_note": true, "notes": [note] })
}

fn render(setup: &Setup, remote: &Remote) -> Vec<Value> {
    remote
        .items
        .iter()
        .map(|item| match item {
            Item::Thread {
                id,
                resolved,
                line,
                comments,
            } => discussion(id, *resolved, Some(*line), notes(comments), setup),
            Item::General {
                id,
                discussion: true,
                comments,
            } => discussion(id, false, None, notes(comments), setup),
            Item::General { id, comments, .. } => {
                let mut notes = notes(comments).into_iter();
                let mut item = general(id, notes.next().unwrap());
                let rest: Vec<Value> = notes.collect();
                if !rest.is_empty() {
                    item["individual_note"] = json!(false);
                    item["notes"].as_array_mut().unwrap().extend(rest);
                }
                item
            }
        })
        .collect()
}

impl FakeService for Gitlab {
    fn origin(&self) -> &'static str {
        ORIGIN
    }

    fn number(&self) -> &'static str {
        "3"
    }

    fn target(&self) -> &'static str {
        "gl-3"
    }

    fn expect(&self) -> &Expect {
        &self.expect
    }

    fn sync_with(&self, setup: &Setup, call: Option<usize>, remote: &Remote) {
        let env = &setup.env;
        login(env);
        let head = remote.head.as_deref().unwrap_or(&setup.head);
        let vars = [
            ("HOST", "gitlab.com"),
            ("REPO", "me/proj"),
            ("HEAD", head),
            ("BASE", setup.base.as_str()),
            ("AUTHOR", remote.author),
        ];
        let answers = [
            (read(MR), fixture("gitlab/mr.json", &vars)),
            (
                list(MR, "discussions"),
                json!(render(setup, remote)).to_string(),
            ),
            (list(MR, "draft_notes"), json!(remote.drafts).to_string()),
        ];
        for (request, body) in answers {
            match call {
                Some(call) => env.respond_nth(&strs(&request), call, &body),
                None => env.respond(&strs(&request), &body),
            }
        }
    }

    fn rich_origin(&self) -> &'static str {
        "git@gitlab.example.com:group/sub/proj.git"
    }

    fn rich_env(&self) -> Vec<(&'static str, &'static str)> {
        vec![("GITLAB_HOST", "gitlab.example.com")]
    }

    fn rich_sync(&self, setup: &Setup) {
        let old = setup.env.git(&["rev-parse", "HEAD~1"]);
        sync_fixtures(setup, &old);
    }

    fn is_read(&self, args: &str) -> bool {
        glab_is_get(args)
    }

    fn call(&self, call: Call) -> Vec<String> {
        match call {
            Call::PostInline => write("POST", &format!("{MR}/draft_notes")),
            Call::PublishDraft => {
                args(&["api", "-X", "PUT", &format!("{MR}/draft_notes/77/publish")])
            }
            Call::PostThread => write("POST", &format!("{MR}/discussions")),
            Call::PostNote => write("POST", &format!("{MR}/notes")),
            Call::Reply(thread) => write("POST", &format!("{MR}/discussions/{thread}/notes")),
            Call::Resolve(thread) | Call::Unresolve(thread) => {
                write("PUT", &format!("{MR}/discussions/{thread}"))
            }
            Call::Approve => write("POST", &format!("{MR}/approve")),
        }
    }

    fn created(&self, call: Call, id: u64) -> String {
        match call {
            Call::PostInline | Call::PostNote | Call::Reply(_) => json!({ "id": id }),
            Call::PublishDraft => return String::new(),
            Call::PostThread => json!({ "id": format!("d-{id}"), "notes": [{ "id": id }] }),
            Call::Resolve(thread) => json!({ "id": thread, "resolved": true }),
            Call::Unresolve(thread) => json!({ "id": thread, "resolved": false }),
            Call::Approve => json!({ "approved": true }),
        }
        .to_string()
    }

    fn alice_thread(&self) -> u64 {
        4001
    }

    fn alice_general(&self) -> u64 {
        4002
    }

    fn local_thread(&self, id: u64) -> String {
        format!("gl-n{id}")
    }

    fn local_general(&self, id: u64) -> String {
        format!("gl-n{id}")
    }

    fn before(&self, _setup: &Setup) -> Remote {
        let mut remote = Remote::new();
        remote.thread(
            "d-o",
            true,
            5,
            vec![Comment::new(4001, "alice", "Alice thread text.")],
        );
        remote.general("d-g", Comment::new(4002, "alice", "Alice general text."));
        remote
    }

    fn land_inline(&self, setup: &Setup) -> Remote {
        let env = &setup.env;
        let f1 = setup.finding(1);
        self.link(setup);
        self.lose(env, Call::PostInline);
        env.fail(&["publish", "--pr", "3"], None);
        let mut remote = Remote::new();
        remote
            .drafts
            .push(json!({ "id": 77, "note": sent(env, &f1) }));
        self.sync(setup, &remote);
        env.respond(&strs(&self.call(Call::PublishDraft)), "");
        self.restore(env, Call::PostInline);
        env.run(&["publish", "--pr", "3", "--only", &f1]);
        let mut remote = Remote::new();
        remote.thread(
            "d-f1",
            false,
            3,
            vec![Comment::new(5001, "me", "{{LAST_NOTE}}")],
        );
        self.sync(setup, &remote);
        env.pull(&["sync", "--pr", "3"]);
        remote
    }

    fn land_findings(&self, setup: &Setup) -> (Remote, String) {
        let env = &setup.env;
        let (f1, f2) = (setup.finding(1), setup.finding(2));
        self.link(setup);
        self.lose(env, Call::PostInline);
        env.fail(&["publish", "--pr", "3"], None);
        let mut remote = Remote::new();
        remote
            .drafts
            .push(json!({ "id": 77, "note": sent(env, &f1) }));
        self.sync(setup, &remote);
        env.respond(&strs(&self.call(Call::PublishDraft)), "");
        self.lose(env, Call::PostThread);
        env.fail(&["publish", "--pr", "3"], None);
        let mut remote = Remote::new();
        remote.thread(
            "d-f1",
            false,
            3,
            vec![Comment::new(5001, "me", "{{LAST_NOTE}}")],
        );
        remote.items.push(Item::General {
            id: "d-f2".to_owned(),
            discussion: true,
            comments: vec![Comment::new(5002, "me", &sent(env, &f2))],
        });
        self.sync(setup, &remote);
        self.restore(env, Call::PostThread);
        env.pull(&["sync", "--pr", "3"]);
        (remote, "d-f2".to_owned())
    }

    fn stages(&self, setup: &Setup) -> [Remote; 3] {
        let f1 = setup.finding(1);
        let f2 = setup.finding(2);
        let discussion = |remote: &mut Remote, id: &str, comment| {
            remote.items.push(Item::General {
                id: id.to_owned(),
                discussion: true,
                comments: vec![comment],
            });
        };
        let d_f2 = Comment::new(5002, "me", &marked("f2", &format!("{f2}/c1")));
        let d_h1 = Comment::new(5003, "me", &marked("h1", "h1/c1"));
        // The inline finding is already posted, so a fix to it is answered and resolved.
        let mut start = self.before(setup);
        start.thread(
            "d-f1",
            false,
            3,
            vec![Comment::new(5001, "me", "{{LAST_NOTE}}")],
        );
        let mut posted = self.before(setup);
        posted.thread(
            "d-f1",
            false,
            3,
            vec![Comment::new(5001, "me", "{{LAST_NOTE}}")],
        );
        discussion(&mut posted, "d-f2", d_f2);
        discussion(&mut posted, "d-h1", d_h1);
        let mut done = Remote::new();
        done.thread(
            "d-o",
            false,
            5,
            vec![Comment::new(4001, "alice", "Alice thread text.")],
        );
        // A reply turns an individual note into a discussion.
        done.items.push(Item::General {
            id: "d-g".to_owned(),
            discussion: false,
            comments: vec![
                Comment::new(4002, "alice", "Alice general text."),
                Comment::new(5012, "me", &marked("Thanks", "gl-n4002/c1")),
            ],
        });
        done.thread(
            "d-f1",
            true,
            3,
            vec![
                Comment::new(5001, "me", "{{LAST_NOTE}}"),
                Comment::new(5011, "me", &marked("Fixed.", &format!("{f1}/c2"))),
            ],
        );
        discussion(
            &mut done,
            "d-f2",
            Comment::new(5002, "me", &marked("f2", &format!("{f2}/c1"))),
        );
        discussion(
            &mut done,
            "d-h1",
            Comment::new(5003, "me", &marked("h1", "h1/c1")),
        );
        // GitLab shows no edits, so the record must read as this store sent it.
        done.general("d-rec", Comment::new(5020, "me", "{{LAST_BODY}}"));
        [start, posted, done]
    }

    fn publish_writes(&self, env: &Env, _setup: &Setup) {
        env.respond(
            &strs(&self.call(Call::PostInline)),
            &self.created(Call::PostInline, 77),
        );
        env.respond(&strs(&self.call(Call::PublishDraft)), "");
        let threads = self.call(Call::PostThread);
        env.respond_nth(&strs(&threads), 1, r#"{"id":"d-f2","notes":[{"id":5002}]}"#);
        env.respond_nth(&strs(&threads), 2, r#"{"id":"d-h1","notes":[{"id":5003}]}"#);
        for (discussion, id) in [("d-f1", 5011), ("d-g", 5012), ("d-o", 5013)] {
            self.accept(env, Call::Reply(discussion), id);
        }
        for (discussion, resolved) in [("d-f1", true), ("d-o", false)] {
            let call = if resolved {
                Call::Resolve(discussion)
            } else {
                Call::Unresolve(discussion)
            };
            self.accept(env, call, 0);
        }
        self.accept(env, Call::PostNote, 5020);
    }

    fn assert_dry_reads(&self, env: &Env, before: usize) {
        for call in &env.forge_calls()[before..] {
            assert!(call.contains(" GET "), "{call}");
        }
    }

    fn assert_dry_run(&self, _setup: &Setup, _dry: &Value) {}

    fn assert_inline_dry(&self, setup: &Setup, dry: &Value) {
        let draft = &dry["writes"][0]["input"];
        assert_eq!(
            draft["position"],
            json!({
                "position_type": "text",
                "base_sha": setup.base,
                "start_sha": setup.base,
                "head_sha": setup.head,
                "old_path": "a.txt",
                "new_path": "a.txt",
                "new_line": 3,
            })
        );
        let note = draft["note"].as_str().unwrap();
        // The title sits on its own line; the footer names agt review and the model.
        assert!(
            note.starts_with(
                "**🐛 bug (high)**\n\nIn-diff text of f1.\n\n> <sub>🔍 agt review · m1</sub>\n"
            ),
            "{note}"
        );
    }

    fn assert_posted(&self, setup: &Setup) {
        let env = &setup.env;
        let inputs = env.forge_inputs();
        let general = input_of(&inputs, &self.call(Call::PostThread));
        assert!(
            general[0]["body"]
                .as_str()
                .unwrap()
                .starts_with("**✏️ nit**\n\nGeneral text of f2.")
        );
        assert_eq!(
            input_of(&inputs, &self.call(Call::Unresolve("d-o"))),
            [json!({ "resolved": false })]
        );
        assert_eq!(
            input_of(&inputs, &self.call(Call::Resolve("d-f1"))),
            [json!({ "resolved": true })]
        );
        let summary = &input_of(&inputs, &self.call(Call::PostNote))[0];
        assert_summary(setup, summary["body"].as_str().unwrap());
        assert_never_posted(env);
        let list = env.run(&["list"]);
        let f1 = thread(&list, &setup.finding(1));
        assert_eq!(f1["remote"]["id"], "d-f1");
        assert_eq!(f1["comments"][0]["draft_id"], Value::Null);
        assert_eq!(f1["comments"][1]["remote"]["id"], "5011");
        let status = env.run(&["status", "--full"]);
        assert_eq!(status["reviews_at_head"], 1);
        assert_eq!(status["runs"][0]["record"]["remote_id"], "5020");
    }

    fn assert_counts_once(&self, env: &Env) {
        // The one draft is the lost write that landed f1; it is not posted again.
        assert_eq!(self.count(env, Call::PostInline), 1);
        assert_eq!(self.count(env, Call::PublishDraft), 1);
        assert_eq!(self.count(env, Call::PostThread), 2);
        assert_eq!(self.count(env, Call::PostNote), 1);
    }

    fn assert_range(&self, dry: &Value, start: u32, end: u32, text: &str) {
        let draft = dry["writes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|write| {
                write["input"]["note"]
                    .as_str()
                    .is_some_and(|note| note.contains(text))
            })
            .unwrap_or_else(|| panic!("{dry}"));
        let range = &draft["input"]["position"]["line_range"];
        assert_eq!(range["start"]["new_line"], start, "{dry}");
        assert_eq!(range["end"]["new_line"], end);
        assert_eq!(range["end"]["type"], "new");
    }
}

/// The rich sync fixtures of a self-hosted GitLab: a merge request with discussions and a draft
/// note; `old` is the commit the first position was made at.
pub fn sync_fixtures(setup: &Setup, old: &str) {
    let env = &setup.env;
    let project = "projects/group%2Fsub%2Fproj";
    let vars = [
        ("RUN", setup.run.as_str()),
        ("HEAD", setup.head.as_str()),
        ("BASE", setup.base.as_str()),
        ("OLD", old),
        ("HOST", "gitlab.example.com"),
        ("REPO", "group/sub/proj"),
        ("AUTHOR", "me"),
    ];
    login(env);
    env.respond(
        &[
            "api",
            "-X",
            "GET",
            &format!("{project}/merge_requests?source_branch=feat&state=all&per_page=20"),
        ],
        r#"[{"iid":9,"state":"opened","source_project_id":555},{"iid":3,"state":"opened","source_project_id":87447936}]"#,
    );
    env.respond(&["api", "-X", "GET", project], r#"{"id":87447936}"#);
    let mr = format!("{project}/merge_requests/3");
    env.respond(&strs(&read(&mr)), &fixture("gitlab/mr.json", &vars));
    env.respond(
        &strs(&list(&mr, "discussions")),
        &fixture("gitlab/discussions.json", &vars),
    );
    env.respond(
        &strs(&list(&mr, "draft_notes")),
        &fixture("gitlab/drafts.json", &vars),
    );
}
