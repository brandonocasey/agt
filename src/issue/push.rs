//! `push`: send the queued changes of each tracker that allows it. Per issue the order is create,
//! then (for an existing issue) a refetch and three-way merge, then one update, then the
//! comments. Anything that is in doubt is marked before its call, so a rerun can tell a write
//! that landed from one that did not.

use serde_json::{Value, json};

use super::merge::{self, pending_changes, unsent_comments};
use super::sync::{self, connect};
use super::trackers::{self, Tracker};
use crate::core::clock;
use crate::core::config::Config;
use crate::core::error::{Error, Result, fail};
use crate::core::random;
use crate::forge::issue::{Changes, IssueData, ListQuery, NewIssue};
use crate::forge::markers;
use crate::forge::service::{IssueService, IssueSubject, Request};
use crate::forge::transport::TransportError;
use crate::store::Store;
use crate::store::issues::{Category, Fields, Issue, TRACKER_FILE, WriteError};
use crate::store::model::{PostState, RemoteComment};

/// How long after a create with no answer a push waits before sending it again.
const LISTING_LAG_SECS: i64 = 120;

pub struct Options {
    pub dry_run: bool,
    pub force: bool,
}

pub fn run(
    store: &Store,
    config: &Config,
    filter: Option<&str>,
    options: &Options,
) -> Result<Value> {
    let mut reports = Vec::new();
    for configured in trackers::configured(config, filter)? {
        let tracker = trackers::ensure(store, configured)?;
        if !configured.push {
            if filter.is_some() {
                return fail(format!(
                    "tracker {} has push = false in the configuration; set it to true to send changes",
                    tracker.doc.key
                ));
            }
            reports.push(json!({"tracker": tracker.doc.key, "skipped": "push = false"}));
            continue;
        }
        reports.push(push_tracker(store, &tracker, options)?);
    }
    Ok(json!({ "dry_run": options.dry_run, "trackers": reports }))
}

fn push_tracker(store: &Store, tracker: &Tracker, options: &Options) -> Result<Value> {
    let queued: Vec<String> = store
        .docs::<Issue>(&tracker.dir, &[TRACKER_FILE])?
        .iter()
        .filter(|issue| merge::has_pending(issue) || (options.force && !issue.conflicts.is_empty()))
        .map(trackers::doc_name)
        .collect();
    let mut pushing = Push {
        store,
        tracker,
        service: None,
        login: String::new(),
        attempt: random::token("at-", 8),
        options,
        writes: Vec::new(),
        failed: Vec::new(),
        deferred: Vec::new(),
        conflicts: Vec::new(),
    };
    if !queued.is_empty() {
        let _lock = if options.dry_run {
            None
        } else {
            Some(store.publish_lock(&tracker.dir, "push")?)
        };
        let (service, login) = connect(tracker)?;
        pushing.service = Some(service);
        pushing.login = login;
        for name in queued {
            pushing.issue(&name)?;
        }
    }
    let writes = if options.dry_run { "writes" } else { "sent" };
    Ok(json!({
        "tracker": tracker.doc.key,
        writes: pushing.writes,
        "failed": pushing.failed,
        "deferred": pushing.deferred,
        "conflicts": pushing.conflicts,
    }))
}

struct Push<'a> {
    store: &'a Store,
    tracker: &'a Tracker,
    service: Option<Box<dyn IssueService>>,
    login: String,
    /// One attempt id per push: a marker is claimed only by the write of its own attempt.
    attempt: String,
    options: &'a Options,
    writes: Vec<Value>,
    failed: Vec<Value>,
    /// Writes left for a later push on purpose, not failures.
    deferred: Vec<Value>,
    conflicts: Vec<Value>,
}

/// What happened to an issue's step.
enum Step {
    Done,
    /// The host refused or failed a write; the rest of the issue waits for the next push.
    Stopped,
}

impl Push<'_> {
    fn service(&self) -> &dyn IssueService {
        self.service
            .as_deref()
            .expect("an issue is pushed only after connecting")
    }

    fn repo(&self) -> &str {
        &self.tracker.doc.repo
    }

    /// Applies `change` to the stored issue under its lock, and to the copy in `doc`. A dry run
    /// changes the copy only.
    fn mutate(&self, name: &str, doc: &mut Issue, change: impl FnOnce(&mut Issue)) -> Result<()> {
        if self.options.dry_run {
            change(doc);
            return Ok(());
        }
        let seed = doc.clone();
        *doc = self.store.update_doc(
            &self.tracker.dir,
            name,
            || seed,
            |stored| {
                change(stored);
                Ok(stored.clone())
            },
        )?;
        Ok(())
    }

    fn issue(&mut self, name: &str) -> Result<()> {
        let Some(mut doc) = self.store.load_doc::<Issue>(&self.tracker.dir, name)? else {
            return Ok(());
        };
        let mut name = name.to_owned();
        let mut fresh = false;
        if doc.number.is_none() {
            match self.create(&mut name, &mut doc)? {
                Step::Stopped => return Ok(()),
                Step::Done => fresh = doc.number.is_some(),
            }
            if doc.number.is_none() {
                // A dry run only shows the create; what follows needs the number.
                return Ok(());
            }
        }
        if !fresh {
            self.refetch(&name, &mut doc)?;
        }
        if self.options.force {
            self.mutate(&name, &mut doc, |issue| {
                force(issue);
            })?;
        } else if !doc.conflicts.is_empty() {
            self.conflicts.push(json!({
                "issue": doc.key,
                "fields": doc.conflicts.iter().map(|c| c.field.clone()).collect::<Vec<_>>(),
            }));
        }
        if let Step::Stopped = self.update(&name, &mut doc)? {
            return Ok(());
        }
        for id in unsent_comments(&doc)
            .iter()
            .map(|c| c.id.clone())
            .collect::<Vec<_>>()
        {
            if let Step::Stopped = self.comment(&name, &mut doc, &id)? {
                return Ok(());
            }
        }
        self.mutate(&name, &mut doc, |issue| issue.errors.clear())
    }

    fn subject<'a>(&'a self, doc: &Issue) -> Result<IssueSubject<'a>> {
        let number = doc
            .number
            .ok_or_else(|| Error(format!("issue {} has no number yet", doc.key)))?;
        Ok(IssueSubject {
            repo: self.repo(),
            number,
        })
    }

    fn refetch(&mut self, name: &str, doc: &mut Issue) -> Result<()> {
        let fetched = self.service().fetch_issue(self.subject(doc)?)?;
        let now = clock::now()?;
        let me = self.login.clone();
        self.mutate(name, doc, |issue| {
            merge::apply(issue, &fetched, &me, &now);
        })
    }

    /// Records a failed write on the issue and in the report. A refusal is final until the
    /// text changes, so the item goes back to the queue; any other failure leaves it marked as
    /// possibly sent.
    fn failure(
        &mut self,
        name: &str,
        doc: &mut Issue,
        what: &str,
        error: &TransportError,
    ) -> Result<Step> {
        let at = clock::now()?;
        let record = WriteError {
            what: what.to_owned(),
            message: error.message.clone(),
            refused: error.refused(),
            at,
        };
        let (what, refused) = (what.to_owned(), error.refused());
        self.failed.push(json!({
            "issue": doc.key, "what": what, "message": error.message, "refused": refused,
        }));
        self.mutate(name, doc, |issue| {
            issue.errors.retain(|known| known.what != what);
            issue.errors.push(record);
            if !refused {
                return;
            }
            if what == "create" {
                if let Some(create) = issue.create.as_mut() {
                    create.post = PostState::Pending;
                    create.attempt = None;
                }
            } else if let Some(comment) = issue.comments.iter_mut().find(|c| c.id == what) {
                comment.delivery.post = PostState::Pending;
                comment.delivery.attempt = None;
            }
        })?;
        Ok(Step::Stopped)
    }

    fn show(&mut self, doc: &Issue, kind: &str, request: &Request, extra: Value) {
        let mut write = json!({
            "issue": doc.key,
            "kind": kind,
            "args": self.service().render(request),
            "body": request.body,
        });
        if let (Some(write), Some(extra)) = (write.as_object_mut(), extra.as_object()) {
            write.extend(extra.clone());
        }
        self.writes.push(write);
    }

    fn create(&mut self, name: &mut String, doc: &mut Issue) -> Result<Step> {
        if doc
            .create
            .as_ref()
            .is_some_and(|create| create.is_posting())
            && let Some(found) = self.find_landed_create(doc)?
        {
            if self.options.dry_run {
                self.writes
                    .push(json!({"issue": doc.key, "kind": "claim", "number": found.number}));
                doc.number = Some(found.number);
                return Ok(Step::Done);
            }
            sync::adopt(self.store, self.tracker, &doc.key, &found)?;
            *name = found.number.to_string();
            *doc = self
                .store
                .load_doc(&self.tracker.dir, name)?
                .ok_or_else(|| Error(format!("issue {name} vanished")))?;
            self.writes
                .push(json!({"issue": doc.key, "kind": "claim", "number": found.number}));
            return Ok(Step::Done);
        }
        if self.sent_recently(doc)? {
            self.deferred.push(json!({
                "issue": doc.key,
                "what": "create",
                "reason": "a create was sent moments ago and the host's listing may not show it yet; push again later",
            }));
            return Ok(Step::Stopped);
        }
        let marker = markers::agent(&doc.key, Some(&self.attempt));
        let body = if doc.fields.body.is_empty() {
            marker
        } else {
            format!("{}\n\n{marker}", doc.fields.body)
        };
        let request = self.service().create(
            self.repo(),
            &NewIssue {
                title: &doc.fields.title,
                body: &body,
                labels: &doc.fields.labels,
                assignees: &doc.fields.assignees,
            },
        )?;
        if self.options.dry_run {
            self.show(doc, "create", &request, json!({}));
            return Ok(Step::Done);
        }
        let (attempt, sent) = (self.attempt.clone(), body.clone());
        self.mutate(name, doc, |issue| {
            if let Some(create) = issue.create.as_mut() {
                create.begin(&attempt, Some(sent));
            }
        })?;
        match self.service().send(&request) {
            Ok(response) => {
                let data = self.service().created(&response.body)?;
                sync::adopt(self.store, self.tracker, &doc.key, &data)?;
                *name = data.number.to_string();
                *doc = self
                    .store
                    .load_doc(&self.tracker.dir, name)?
                    .ok_or_else(|| Error(format!("issue {name} vanished")))?;
                self.writes
                    .push(json!({"issue": doc.key, "kind": "create", "number": data.number}));
                Ok(Step::Done)
            }
            Err(error) => self.failure(name, doc, "create", &error),
        }
    }

    /// Whether a create of `doc` went out less than [`LISTING_LAG_SECS`] ago without an answer.
    /// The hosts list new issues a little late, so a search that finds nothing right after a
    /// lost response does not show that the create did not land.
    fn sent_recently(&self, doc: &Issue) -> Result<bool> {
        if !doc
            .create
            .as_ref()
            .is_some_and(|create| create.is_posting())
        {
            return Ok(false);
        }
        let Some(error) = doc.errors.iter().find(|error| error.what == "create") else {
            return Ok(false);
        };
        Ok(clock::now_secs()? - clock::parse(&error.at)? < LISTING_LAG_SECS)
    }

    /// The host's copy of a create that was sent but never answered, found by its marker.
    fn find_landed_create(&self, doc: &Issue) -> Result<Option<IssueData>> {
        let since = clock::format(clock::parse(&doc.created_at)? - 300);
        let listed = self.service().list_changed(&ListQuery {
            repo: self.repo(),
            scope: crate::core::config::Scope::All,
            since: &since,
            login: &self.login,
        })?;
        Ok(listed
            .into_iter()
            .find(|data| sync::is_created_by(doc, data, &self.login)))
    }

    fn update(&mut self, name: &str, doc: &mut Issue) -> Result<Step> {
        let changes = pending_changes(doc);
        if changes.is_empty() {
            return Ok(Step::Done);
        }
        let request = self.service().update(self.subject(doc)?, &changes)?;
        if self.options.dry_run {
            self.show(doc, "update", &request, json!({}));
            return Ok(Step::Done);
        }
        match self.service().send(&request) {
            Ok(_) => {
                self.mutate(name, doc, |issue| {
                    if let Some(base) = issue.base.as_mut() {
                        record_sent(base, &changes);
                    }
                    issue.conflicts.clear();
                })?;
                self.writes
                    .push(json!({"issue": doc.key, "kind": "update"}));
                Ok(Step::Done)
            }
            Err(error) => self.failure(name, doc, "update", &error),
        }
    }

    fn comment(&mut self, name: &str, doc: &mut Issue, id: &str) -> Result<Step> {
        let Some(comment) = doc.comments.iter().find(|comment| comment.id == id) else {
            return Ok(Step::Done);
        };
        let text = format!(
            "{}\n\n{}",
            comment.body,
            markers::agent(id, Some(&self.attempt))
        );
        let request = self.service().comment(self.subject(doc)?, &text);
        if self.options.dry_run {
            self.show(doc, "comment", &request, json!({"comment": id}));
            return Ok(Step::Done);
        }
        let (attempt, sent, local) = (self.attempt.clone(), text.clone(), id.to_owned());
        self.mutate(name, doc, |issue| {
            if let Some(comment) = issue.comments.iter_mut().find(|c| c.id == local) {
                comment.delivery.begin(&attempt, Some(sent));
            }
        })?;
        match self.service().send(&request) {
            Ok(response) => {
                let (remote_id, url) = self
                    .service()
                    .issue_comment_created(&response.body)
                    .ok_or_else(|| Error(format!("no comment id returned for {id}")))?;
                let (me, local) = (self.login.clone(), id.to_owned());
                self.mutate(name, doc, |issue| {
                    if let Some(comment) = issue.comments.iter_mut().find(|c| c.id == local) {
                        comment.delivery.post = PostState::Posted;
                        comment.remote = Some(RemoteComment {
                            id: remote_id,
                            url,
                            login: Some(me),
                            is_me: true,
                            editor: None,
                            review_id: None,
                        });
                    }
                })?;
                self.writes
                    .push(json!({"issue": doc.key, "kind": "comment", "comment": id}));
                Ok(Step::Done)
            }
            Err(error) => self.failure(name, doc, id, &error),
        }
    }
}

/// Makes the local value of every conflicted field the one to send, and forgets the conflicts.
/// Only the fields in conflict move: the merge already set `base` to the remote's value.
fn force(issue: &mut Issue) -> Vec<String> {
    let mut forced = Vec::new();
    for conflict in std::mem::take(&mut issue.conflicts) {
        match conflict.field.as_str() {
            "title" => issue.fields.title = conflict.local.title.clone(),
            "body" => issue.fields.body = conflict.local.body.clone(),
            "state" => {
                issue.fields.state = conflict.local.state;
                issue.fields.reason = conflict.local.reason.clone();
            }
            _ => {}
        }
        forced.push(conflict.field);
    }
    issue.fields.normalize();
    forced
}

/// Moves `base` to what an update just sent.
fn record_sent(base: &mut Fields, changes: &Changes) {
    if let Some(title) = &changes.title {
        base.title.clone_from(title);
    }
    if let Some(body) = &changes.body {
        base.body.clone_from(body);
    }
    if let Some((state, reason)) = &changes.state {
        base.state = *state;
        base.reason = if *state == Category::Open {
            None
        } else {
            reason.clone()
        };
    }
    if let Some(labels) = &changes.labels {
        base.labels.clone_from(labels);
    }
    if let Some(assignees) = &changes.assignees {
        base.assignees.clone_from(assignees);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::issue::{IssueData, IssueFetched};
    use crate::store::issues::Category;

    fn data(title: &str) -> IssueData {
        IssueData {
            number: 1,
            id: "I_1".into(),
            url: "u".into(),
            title: title.into(),
            body: "b".into(),
            state: Category::Open,
            reason: None,
            native_state: "open".into(),
            labels: Vec::new(),
            assignees: Vec::new(),
            author: None,
            milestone: None,
            due_date: None,
            created_at: "2026-10-01T00:00:00Z".into(),
            updated_at: "2026-10-02T00:00:00Z".into(),
        }
    }

    #[test]
    fn force_makes_the_conflicted_local_value_the_one_to_send() {
        let mut issue = merge::from_remote("t", "o/r", &data("Old"), "2026-10-02T00:00:00Z");
        issue.fields.title = "Mine".into();
        let fetched = IssueFetched {
            issue: data("Theirs"),
            comments: Vec::new(),
            prs: Vec::new(),
        };
        merge::apply(&mut issue, &fetched, "me", "2026-10-03T00:00:00Z");
        assert!(pending_changes(&issue).is_empty());
        assert_eq!(force(&mut issue), ["title"]);
        assert_eq!(pending_changes(&issue).title.as_deref(), Some("Mine"));
        assert!(issue.conflicts.is_empty());
    }
}
