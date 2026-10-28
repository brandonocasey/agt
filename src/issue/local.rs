//! The commands that change only the local copy: `link`, private notes and every write that waits for
//! `push`. Nothing here talks to a host.

use serde_json::Value;

use super::cli::{CreateArgs, EditArgs, LinkArgs, Lists, Reason, State};
use super::merge::{pending_changes, unsent_comments};
use super::trackers::{self, Tracker};
use super::view::summary;
use crate::core::clock;
use crate::core::config::Config;
use crate::core::error::{Result, fail};
use crate::core::random;
use crate::core::text::{MAX_TEXT, argument_or_stdin, bounded, within};
use crate::forge::{self, service};
use crate::store::Store;
use crate::store::issues::{Category, Fields, Issue, LinkSource, Links, Note, PrLink, VERSION};
use crate::store::model::{Author, Comment, Delivery, PostState, last_number};
use crate::store::prune::Removal;
use serde_json::json;

/// `text`, or stdin when it is `-`; never empty unless `allow_empty`.
fn read_text(text: &str, what: &str, allow_empty: bool) -> Result<String> {
    let text = argument_or_stdin(text, what)?;
    if allow_empty {
        within(&text, what, MAX_TEXT)?;
    } else {
        bounded(&text, what, MAX_TEXT, true)?;
    }
    Ok(text)
}

/// Changes the issue `key` names under its lock and reports the result.
fn change(
    store: &Store,
    filter: Option<&str>,
    key: &str,
    edit: impl FnOnce(&Tracker, &mut Issue) -> Result<()>,
) -> Result<Value> {
    let (tracker, issue) = trackers::find(store, filter, key)?;
    let now = clock::now()?;
    let name = issue.doc_name();
    let updated = store.update_existing_doc(&tracker.dir, &name, |issue: &mut Issue| {
        edit(&tracker, issue)?;
        issue.fields.normalize();
        issue.updated_at.clone_from(&now);
        Ok(issue.clone())
    })?;
    Ok(summary(&updated))
}

fn next_comment_id(issue: &Issue) -> String {
    let last = last_number(issue.comments.iter().map(|comment| &comment.id), "c");
    format!("c{}", last + 1)
}

fn queue_comment(tracker: &Tracker, issue: &mut Issue, text: String) {
    let id = next_comment_id(issue);
    let me = tracker.doc.login.clone().unwrap_or_else(|| "local".into());
    issue.comments.push(Comment::new(
        id,
        Author::Human {
            login: me,
            git_name: false,
        },
        text,
        issue.updated_at.clone(),
        PostState::Pending,
        None,
    ));
}

/// Queues `text` as a comment, or with `private` keeps it as a note that is never sent.
pub fn comment(
    store: &Store,
    filter: Option<&str>,
    key: &str,
    text: &str,
    private: bool,
) -> Result<Value> {
    let text = read_text(text, if private { "note" } else { "comment" }, false)?;
    let now = clock::now()?;
    change(store, filter, key, |tracker, issue| {
        if private {
            let last = last_number(issue.notes.iter().map(|note| &note.id), "n");
            issue.notes.push(Note {
                id: format!("n{}", last + 1),
                text,
                created_at: now,
            });
        } else {
            issue.updated_at.clone_from(&now);
            queue_comment(tracker, issue, text);
        }
        Ok(())
    })
}

/// Closes the issue `key` names unless it is closed already; whether this call closed it.
pub(super) fn close_if_open(store: &Store, key: &str) -> Result<bool> {
    let (_, issue) = trackers::find(store, None, key)?;
    if issue.fields.state == Category::Closed {
        return Ok(false);
    }
    change(store, None, key, |tracker, issue| {
        set_state(tracker, issue, State::Closed, None)
    })?;
    Ok(true)
}

fn set_state(
    tracker: &Tracker,
    issue: &mut Issue,
    state: State,
    reason: Option<Reason>,
) -> Result<()> {
    if reason.is_some() && matches!(state, State::Open) {
        return fail("--reason needs --state closed");
    }
    let caps = tracker
        .doc
        .forge
        .map(|forge| service::kind(forge).issue_caps);
    if reason.is_some() && caps.is_some_and(|caps| !caps.close_reasons) {
        return fail("this service takes no close reason");
    }
    match state {
        State::Open => {
            issue.fields.state = Category::Open;
            issue.fields.reason = None;
        }
        State::Closed => {
            issue.fields.state = Category::Closed;
            // The host records `completed` for a close sent without a reason, so the local
            // copy says it too; otherwise the next merge sees a reason both sides "changed".
            issue.fields.reason = reason.map(|reason| reason.as_str().to_owned()).or_else(|| {
                caps.is_some_and(|caps| caps.close_reasons)
                    .then(|| "completed".to_owned())
            });
        }
    }
    Ok(())
}

/// Removals apply first, so one call can swap the assignee on a single-assignee service.
fn assign(tracker: &Tracker, issue: &mut Issue, add: &[String], remove: &[String]) -> Result<()> {
    let assignees = &mut issue.fields.assignees;
    assignees.retain(|login| !remove.contains(login));
    if add.is_empty() {
        return Ok(());
    }
    let had_any = !assignees.is_empty();
    assignees.extend(add.iter().cloned());
    assignees.sort();
    assignees.dedup();
    let caps = tracker
        .doc
        .forge
        .map(|forge| service::kind(forge).issue_caps);
    if caps.is_some_and(|caps| !caps.multi_assignee) && assignees.len() > 1 {
        return fail(if had_any {
            "this service takes one assignee on an issue; remove the current one with --remove-assignee"
        } else {
            "this service takes one assignee on an issue"
        });
    }
    Ok(())
}

/// The label and assignee changes `create` and `edit` share; `create` starts from none.
fn apply_lists(
    tracker: &Tracker,
    issue: &mut Issue,
    add: &Lists,
    remove_labels: &[String],
    remove_assignees: &[String],
) -> Result<()> {
    let labels = &mut issue.fields.labels;
    labels.retain(|label| !remove_labels.contains(label));
    labels.extend(add.add_labels.iter().cloned());
    assign(tracker, issue, &add.add_assignees, remove_assignees)
}

pub fn edit(store: &Store, filter: Option<&str>, args: &EditArgs) -> Result<Value> {
    if args.title.is_none()
        && args.body.is_none()
        && args.state.is_none()
        && args.lists.add_labels.is_empty()
        && args.remove_labels.is_empty()
        && args.lists.add_assignees.is_empty()
        && args.remove_assignees.is_empty()
    {
        return fail(
            "edit needs a change: --title, --body, --state, --add-label, --remove-label, --add-assignee or --remove-assignee",
        );
    }
    let title = args
        .title
        .as_deref()
        .map(|title| read_text(title, "title", false))
        .transpose()?;
    let body = args
        .body
        .as_deref()
        .map(|body| read_text(body, "body", true))
        .transpose()?;
    change(store, filter, &args.key, |tracker, issue| {
        if let Some(title) = title {
            issue.fields.title = title.trim().to_owned();
        }
        if let Some(body) = body {
            issue.fields.body = body;
        }
        if let Some(state) = args.state {
            set_state(tracker, issue, state, args.reason)?;
        }
        apply_lists(
            tracker,
            issue,
            &args.lists,
            &args.remove_labels,
            &args.remove_assignees,
        )
    })
}

/// The PR/MR `arg` names as a link, in the repository of `tracker` unless a URL says otherwise.
fn pr_link(tracker: &Tracker, arg: &str, source: LinkSource) -> Result<PrLink> {
    let pr = forge::parse_pr(arg)?;
    let doc = &tracker.doc;
    let forge = match doc.forge {
        Some(forge) => forge,
        None => {
            let (Some((host, repo)), Some(forge)) = (pr.location, pr.forge) else {
                return fail("a local tracker needs a full PR/MR URL");
            };
            let kind = service::kind(forge);
            return Ok(PrLink {
                forge,
                repo: repo.clone(),
                number: pr.number,
                url: Some(format!(
                    "https://{host}/{repo}{}{}",
                    kind.url_marker, pr.number
                )),
                branch: None,
                state: None,
                source,
            });
        }
    };
    let kind = service::kind(forge);
    let repo = match &pr.location {
        Some((host, _)) if !host.eq_ignore_ascii_case(&doc.host) => {
            return fail(format!(
                "{arg} is on {host}, not on the tracker's host {}",
                doc.host
            ));
        }
        Some((_, repo)) => repo.clone(),
        None => doc.repo.clone(),
    };
    Ok(PrLink {
        forge,
        url: Some(format!(
            "https://{}/{repo}{}{}",
            doc.host, kind.url_marker, pr.number
        )),
        repo,
        number: pr.number,
        branch: None,
        state: None,
        source,
    })
}

fn add_pr(issue: &mut Issue, link: PrLink) {
    let known = issue
        .links
        .prs
        .iter()
        .any(|known| known.repo == link.repo && known.number == link.number);
    if !known {
        issue.links.prs.push(link);
    }
}

pub fn link(store: &Store, filter: Option<&str>, args: &LinkArgs) -> Result<Value> {
    if args.comment && args.pr.is_none() {
        return fail("--comment needs --pr");
    }
    change(store, filter, &args.key, |tracker, issue| {
        match (&args.pr, &args.branch) {
            (Some(pr), _) => {
                let link = pr_link(tracker, pr, LinkSource::Local)?;
                let body = format!("Linked: {}", link.url.as_deref().unwrap_or_default());
                add_pr(issue, link);
                // Keyed on the comment, not the link, so `--comment` still works on a PR that
                // an earlier `link` or `sync` already recorded.
                if args.comment && !issue.comments.iter().any(|known| known.body == body) {
                    queue_comment(tracker, issue, body);
                }
            }
            (_, Some(branch)) => {
                let branches = &mut issue.links.branches;
                if !branches.contains(branch) {
                    branches.push(branch.clone());
                }
            }
            _ => return fail("link needs one of --pr, --branch"),
        }
        Ok(())
    })
}

pub fn create(
    store: &Store,
    config: &Config,
    filter: Option<&str>,
    args: &CreateArgs,
) -> Result<Value> {
    let tracker = if filter.is_none() || filter == Some("local") {
        trackers::local(store)?
    } else {
        let configured = trackers::configured(config, filter)?;
        let [configured] = configured.as_slice() else {
            return fail(
                "several trackers are configured; pass --tracker to say where to create it",
            );
        };
        trackers::ensure(store, configured)?
    };
    let title = read_text(&args.title, "title", false)?;
    let body = read_text(&args.body, "body", true)?;
    let now = clock::now()?;
    let key = random::token("local-", 6);
    let fields = Fields {
        title: title.trim().to_owned(),
        body,
        state: Category::Open,
        reason: None,
        labels: Vec::new(),
        assignees: Vec::new(),
    };
    let mut issue = Issue {
        version: VERSION,
        key: key.clone(),
        tracker: tracker.doc.key.clone(),
        number: None,
        remote: None,
        fields,
        base: None,
        author: tracker.doc.login.clone(),
        native_state: None,
        milestone: None,
        due_date: None,
        created_at: now.clone(),
        updated_at: now,
        synced_at: None,
        comments: Vec::new(),
        links: Links::default(),
        notes: Vec::new(),
        conflicts: Vec::new(),
        errors: Vec::new(),
        create: Some(Delivery::new(PostState::Pending)),
    };
    apply_lists(&tracker, &mut issue, &args.lists, &[], &[])?;
    issue.fields.normalize();
    let created = store.update_doc(&tracker.dir, &key, || issue.clone(), |_| Ok(issue.clone()))?;
    Ok(summary(&created))
}

/// Drops every queued, unpushed change of the issue `key` names. A never-pushed issue goes
/// entirely, with its comments and notes; a pushed one loses only its queued edits and
/// comments, and keeps its synced copy, links and private notes.
pub fn discard(store: &Store, filter: Option<&str>, key: &str) -> Result<Value> {
    let (tracker, issue) = trackers::find(store, filter, key)?;
    // Held for the whole discard: with it no push is running, so a `Posting` mark is stale.
    let Some(_lock) = store.try_publish_lock(&tracker.dir, "push")? else {
        return fail("another agt is pushing issues; try again when it is done");
    };
    let name = issue.doc_name();
    if issue.number.is_none() {
        let mut possibly_sent = Vec::new();
        if issue
            .create
            .as_ref()
            .is_some_and(|create| create.post == PostState::Posting)
        {
            possibly_sent.push("create".to_owned());
        }
        possibly_sent.extend(posting_comments(&issue));
        let removed =
            store.try_remove_doc_if(&tracker.dir, &name, |found: &Issue| found.number.is_none())?;
        return match removed {
            Removal::Removed(_) => {
                let mut discarded = json!({
                    "issue": true,
                    "comments": issue.comments.len(),
                    "notes": issue.notes.len(),
                });
                add_possibly_sent(&mut discarded, possibly_sent);
                Ok(json!({"key": issue.key, "tracker": issue.tracker, "discarded": discarded}))
            }
            Removal::Busy => fail(format!("{} is locked by another process", issue.key)),
            Removal::Kept => {
                // A document a newer agt wrote is kept too; an unchanged rewrite reports it.
                store.update_existing_doc(&tracker.dir, &name, |_: &mut Issue| Ok(()))?;
                fail(format!("{} changed while discarding; try again", issue.key))
            }
        };
    }
    let dropped = store.update_existing_doc(&tracker.dir, &name, |issue: &mut Issue| {
        let edits = pending_changes(issue);
        let mut fields = Vec::new();
        for (name, changed) in [
            ("title", edits.title.is_some()),
            ("body", edits.body.is_some()),
            ("state", edits.state.is_some()),
            ("labels", edits.labels.is_some()),
            ("assignees", edits.assignees.is_some()),
        ] {
            if changed {
                fields.push(name);
            }
        }
        let comments = unsent_comments(issue).len();
        if fields.is_empty() && comments == 0 && issue.conflicts.is_empty() {
            return fail(format!("nothing is queued for {}", issue.key));
        }
        let possibly_sent = posting_comments(issue);
        if let Some(base) = issue.base.clone() {
            issue.fields = base;
        }
        issue
            .comments
            .retain(|comment| comment.remote.is_some() || comment.local);
        issue.conflicts.clear();
        issue.errors.clear();
        issue.updated_at = clock::now()?;
        Ok((fields, comments, possibly_sent))
    })?;
    let mut discarded = json!({"issue": false, "edits": dropped.0, "comments": dropped.1});
    add_possibly_sent(&mut discarded, dropped.2);
    Ok(json!({"key": issue.key, "tracker": issue.tracker, "discarded": discarded}))
}

/// Ids of the queued comments marked `Posting`: a push may have sent them before it stopped.
fn posting_comments(issue: &Issue) -> Vec<String> {
    unsent_comments(issue)
        .iter()
        .filter(|comment| comment.delivery.post == PostState::Posting)
        .map(|comment| comment.id.clone())
        .collect()
}

/// Lists what a stale `Posting` mark dropped, so the user can check whether it landed.
fn add_possibly_sent(discarded: &mut Value, possibly_sent: Vec<String>) {
    if !possibly_sent.is_empty() {
        discarded["possibly_sent"] = json!(possibly_sent);
    }
}
