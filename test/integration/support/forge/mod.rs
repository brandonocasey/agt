//! A fake hosting service for scenarios. A [`FakeService`] speaks the scenario's language (a
//! [`Remote`] PR, semantic [`Call`]s, an [`Expect`] derived from the service's capabilities)
//! and turns it into the exact argv and response bodies the fake `gh`/`glab` replays. One
//! scenario function then runs against every service in [`all`].
//!
//! A new service adds `<service>.rs` here, a `test/fixtures/<service>/capabilities.json` and its
//! fixtures, and one line to [`all`].

use serde_json::Value;

use super::Env;
use super::scenario::{Setup, count_calls, input_of, strs};

pub mod github;
pub mod gitlab;

pub const TIME: &str = "2026-10-16T10:00:00Z";

pub struct Comment {
    pub id: u64,
    pub login: String,
    pub body: String,
}

impl Comment {
    pub fn new(id: u64, login: &str, body: &str) -> Self {
        Self {
            id,
            login: login.to_owned(),
            body: body.to_owned(),
        }
    }
}

/// One conversation on the PR/MR, in the host's own terms once rendered.
pub enum Item {
    /// A thread on a line of `a.txt`.
    Thread {
        id: String,
        resolved: bool,
        line: u32,
        comments: Vec<Comment>,
    },
    /// General comments on the PR/MR. `discussion` is whether the host groups them into one
    /// discussion (GitLab); a host that does not shows each comment alone (GitHub).
    General {
        id: String,
        discussion: bool,
        comments: Vec<Comment>,
    },
    /// A thread in the service's own response shape, for what the neutral items cannot say.
    RawThread(Value),
    /// A general comment or discussion in the service's own response shape.
    RawGeneral(Value),
}

/// The state of a PR/MR on the host: what a sync reads.
pub struct Remote {
    pub author: &'static str,
    /// The PR head when it is not the setup's head, as after a force-push.
    pub head: Option<String>,
    pub items: Vec<Item>,
    /// Review bodies, in the service's own shape; only a service with reviews reads them.
    pub reviews: Vec<Value>,
    /// Draft notes, in the service's own shape; only a service with drafts reads them.
    pub drafts: Vec<Value>,
}

impl Remote {
    pub fn new() -> Self {
        Self {
            author: "me",
            head: None,
            items: Vec::new(),
            reviews: Vec::new(),
            drafts: Vec::new(),
        }
    }

    pub fn by(author: &'static str) -> Self {
        Self {
            author,
            ..Self::new()
        }
    }

    pub fn head(mut self, head: String) -> Self {
        self.head = Some(head);
        self
    }

    pub fn thread(&mut self, id: &str, resolved: bool, line: u32, comments: Vec<Comment>) {
        self.items.push(Item::Thread {
            id: id.to_owned(),
            resolved,
            line,
            comments,
        });
    }

    /// A general comment of one author, shown alone.
    pub fn general(&mut self, id: &str, comment: Comment) {
        self.items.push(Item::General {
            id: id.to_owned(),
            discussion: false,
            comments: vec![comment],
        });
    }

    /// Sets the resolved state of the thread `id`.
    pub fn set_resolved(&mut self, id: &str, value: bool) {
        for item in &mut self.items {
            if let Item::Thread {
                id: at, resolved, ..
            } = item
                && at == id
            {
                *resolved = value;
            }
        }
    }
}

/// A write a service takes, named by what it does rather than how the service spells it.
#[derive(Clone, Copy)]
pub enum Call {
    /// The in-diff comments: a review (GitHub) or a draft note (GitLab).
    PostInline,
    /// Publishes the draft `PostInline` made; only with drafts.
    PublishDraft,
    /// A new general thread.
    PostThread,
    /// A general comment that blocks nothing: the record, an approval comment.
    PostNote,
    /// A native reply to the remote thread.
    Reply(&'static str),
    Resolve(&'static str),
    Unresolve(&'static str),
    /// The formal approval.
    Approve,
}

/// What a scenario expects of a service, read from its `capabilities.json` contract fixture,
/// which a unit test holds equal to the service's [`Capabilities`](crate::service).
pub struct Expect(Value);

impl Expect {
    pub fn load(service: &str) -> Self {
        let path = format!(
            "{}/test/fixtures/{service}/capabilities.json",
            env!("CARGO_MANIFEST_DIR")
        );
        Self(serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap())
    }

    fn word(&self, key: &str) -> &str {
        self.0[key].as_str().unwrap_or_else(|| panic!("{key}"))
    }

    fn flag(&self, key: &str) -> bool {
        self.0[key].as_bool().unwrap_or_else(|| panic!("{key}"))
    }

    /// The `kind`s of the writes that post one in-diff comment.
    pub fn inline(&self) -> Vec<&'static str> {
        match self.word("inline") {
            "review_batch" => vec!["review"],
            "draft_then_publish" => vec!["draft", "publish-draft"],
            other => panic!("{other}"),
        }
    }

    pub fn ranges(&self) -> bool {
        self.flag("ranges")
    }

    /// A reply to a general finding is a native reply, not a general comment ending in `Re:`.
    pub fn replies_to_general_natively(&self) -> bool {
        self.word("reply") == "all"
    }

    pub fn resolves_on_host(&self) -> bool {
        self.flag("resolve")
    }

    /// The formal approval is a separate call next to a note.
    pub fn approves_by_call(&self) -> bool {
        self.word("approve") == "call"
    }

    /// The formal approval is a review, which its author may not give on their own PR.
    pub fn approves_by_review(&self) -> bool {
        self.word("approve") == "review"
    }

    pub fn self_approval_allowed(&self) -> bool {
        self.0["approve_self_allowed"].as_bool().unwrap_or(false)
    }

    pub fn edits_attributed(&self) -> bool {
        self.flag("edits_attributed")
    }
}

pub trait FakeService {
    fn name(&self) -> &'static str;
    /// The `origin` remote of a repository on this service.
    fn origin(&self) -> &'static str;
    /// The PR/MR number a scenario syncs.
    fn number(&self) -> &'static str;
    /// The local key of the synced PR/MR.
    fn target(&self) -> &'static str;
    fn expect(&self) -> &Expect;

    /// Registers `remote` as the answer to every sync, with the login and PR lookups.
    fn sync(&self, setup: &Setup, remote: &Remote) {
        self.sync_with(setup, None, remote);
    }
    /// Registers `remote` for the `call`-th sync only (from 1); `None` is every sync.
    fn sync_with(&self, setup: &Setup, call: Option<usize>, remote: &Remote);

    /// Whether the arguments of a fake call only read from the host.
    fn is_read(&self, args: &str) -> bool;
    /// The arguments the fake sees for `call`.
    fn call(&self, call: Call) -> Vec<String>;
    /// The response of a successful `call` that created the comment `id`.
    fn created(&self, call: Call, id: u64) -> String;

    /// The first comment id of Alice's resolved thread in [`FakeService::before`].
    fn alice_thread(&self) -> u64;
    /// The comment id of Alice's general comment in [`FakeService::before`].
    fn alice_general(&self) -> u64;
    /// The local id a thread opened by comment `id` is imported as.
    fn local_thread(&self, id: u64) -> String;
    /// The local id a general comment `id` is imported as.
    fn local_general(&self, id: u64) -> String;
    /// The host's id of Alice's thread in [`FakeService::before`], as a [`Call`] names it.
    fn alice_remote(&self) -> &'static str;
    /// A PR with Alice's resolved thread (line 5) and general comment.
    fn before(&self, setup: &Setup) -> Remote;

    /// The PR the publish of every payload sees on its first sync (`before`), on its second
    /// (everything but the record and the replies posted) and from its third on (all posted).
    fn stages(&self, setup: &Setup) -> [Remote; 3];
    /// Registers the host's answer to each write of that publish.
    fn publish_writes(&self, env: &Env, setup: &Setup);
    /// A dry run since call number `before` only read from the host.
    fn assert_dry_reads(&self, env: &Env, before: usize);
    /// The payloads of the dry run of that publish, in the host's own shape.
    fn assert_dry_run(&self, setup: &Setup, dry: &Value);
    /// The payloads that publish sent and what a sync read back.
    fn assert_posted(&self, setup: &Setup);
    /// A second publish of that PR wrote nothing again.
    fn assert_counts_once(&self, env: &Env);
    /// A dry run's comment containing `text` spans lines `start` to `end` of `a.txt`.
    fn assert_range(&self, dry: &Value, start: u32, end: u32, text: &str);

    /// Links the reviewed branch to the PR, then ends its run: `publish` posts only runs that
    /// end after the target is linked.
    fn link(&self, setup: &Setup) {
        self.sync(setup, &Remote::new());
        setup.env.run(&["sync", "--pr", self.number()]);
        setup.env.run(&["run", "end", "--run", &setup.run]);
    }

    /// `agent-review sync --pr` against the registered responses.
    fn run_sync(&self, setup: &Setup) -> Value {
        setup.env.run(&["sync", "--pr", self.number()])
    }

    /// Accepts `call`, answering with the comment `id` it created.
    fn accept(&self, env: &Env, call: Call, id: u64) {
        env.respond(&strs(&self.call(call)), &self.created(call, id));
    }

    /// Accepts the `nth` `call` (from 1) only.
    fn accept_nth(&self, env: &Env, call: Call, nth: usize, id: u64) {
        env.respond_nth(&strs(&self.call(call)), nth, &self.created(call, id));
    }

    /// Makes `call` fail after its input is read, as when the write lands and its response is
    /// lost.
    fn lose(&self, env: &Env, call: Call) {
        env.fail_call(&strs(&self.call(call)));
    }

    /// Undoes [`FakeService::lose`].
    fn restore(&self, env: &Env, call: Call) {
        env.pass_call(&strs(&self.call(call)));
    }

    fn count(&self, env: &Env, call: Call) -> usize {
        count_calls(env, &self.call(call))
    }

    /// The JSON bodies sent with `call`.
    fn sent_to(&self, env: &Env, call: Call) -> Vec<Value> {
        input_of(&env.forge_inputs(), &self.call(call))
    }
}

/// Every service the scenario matrix runs against.
pub fn all() -> Vec<Box<dyn FakeService>> {
    vec![
        Box::new(github::Github::new()),
        Box::new(gitlab::Gitlab::new()),
    ]
}

/// Every call to the fake was a read; the fake refuses unknown calls.
pub fn assert_read_only(service: &dyn FakeService, env: &Env) {
    let calls = env.forge_calls();
    assert!(!calls.is_empty());
    for call in calls {
        let args = call.split('\t').nth(1).unwrap();
        assert!(service.is_read(args), "unexpected forge call: {call}");
        assert!(!args.contains("mutation"), "{call}");
    }
}
