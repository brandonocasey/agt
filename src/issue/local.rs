//! The commands that change only the local copy: `link`, `note` and every write that waits for
//! `push`. Nothing here talks to a host.

use std::io::Read;

use serde_json::Value;

use super::cli::{CreateArgs, EditArgs, LabelAction, LinkArgs, Reason};
use super::trackers::{self, Tracker, doc_name};
use super::view::summary;
use crate::core::clock;
use crate::core::config::Config;
use crate::core::error::{Result, ctx, fail};
use crate::core::random;
use crate::forge::{self, service};
use crate::store::Store;
use crate::store::issues::{Category, Fields, Issue, LinkSource, Links, Note, PrLink, VERSION};
use crate::store::model::{Author, Comment, Delivery, PostState};

const MAX_TEXT: usize = 65_536;

/// `text`, or stdin when it is `-`; never empty unless `allow_empty`.
fn read_text(text: &str, what: &str, allow_empty: bool) -> Result<String> {
    let text = if text == "-" {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .map_err(ctx(format!("read {what} from stdin")))?;
        input
    } else {
        text.to_owned()
    };
    if !allow_empty && text.trim().is_empty() {
        return fail(format!("{what} must not be empty"));
    }
    if text.len() > MAX_TEXT {
        return fail(format!("{what} exceeds {MAX_TEXT} bytes"));
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
    let name = doc_name(&issue);
    let updated = store.update_doc(
        &tracker.dir,
        &name,
        || issue.clone(),
        |issue| {
            edit(&tracker, issue)?;
            issue.fields.normalize();
            issue.updated_at.clone_from(&now);
            Ok(issue.clone())
        },
    )?;
    Ok(summary(&updated))
}

fn next_comment_id(issue: &Issue) -> String {
    let last = issue
        .comments
        .iter()
        .filter_map(|comment| comment.id.strip_prefix('c')?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
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

pub fn comment(store: &Store, filter: Option<&str>, key: &str, text: &str) -> Result<Value> {
    let text = read_text(text, "comment", false)?;
    let now = clock::now()?;
    change(store, filter, key, |tracker, issue| {
        issue.updated_at.clone_from(&now);
        queue_comment(tracker, issue, text);
        Ok(())
    })
}

pub fn note(store: &Store, filter: Option<&str>, key: &str, text: &str) -> Result<Value> {
    let text = read_text(text, "note", false)?;
    let now = clock::now()?;
    change(store, filter, key, |_, issue| {
        let last = issue
            .notes
            .iter()
            .filter_map(|note| note.id.strip_prefix('n')?.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        issue.notes.push(Note {
            id: format!("n{}", last + 1),
            text,
            created_at: now,
        });
        Ok(())
    })
}

pub fn set_state(
    store: &Store,
    filter: Option<&str>,
    key: &str,
    state: Category,
    reason: Option<Reason>,
) -> Result<Value> {
    change(store, filter, key, |tracker, issue| {
        let caps = service::kind(tracker.doc.forge).issue_caps;
        if reason.is_some() && !caps.close_reasons {
            return fail("this service takes no close reason");
        }
        issue.fields.state = state;
        issue.fields.reason = match state {
            Category::Open => None,
            // The host records `completed` for a close sent without a reason, so the local
            // copy says it too; otherwise the next merge sees a reason both sides "changed".
            Category::Closed => reason
                .map(|reason| reason.as_str().to_owned())
                .or_else(|| caps.close_reasons.then(|| "completed".to_owned())),
        };
        Ok(())
    })
}

pub fn assign(
    store: &Store,
    filter: Option<&str>,
    key: &str,
    logins: &[String],
    add: bool,
) -> Result<Value> {
    change(store, filter, key, |tracker, issue| {
        let assignees = &mut issue.fields.assignees;
        if add {
            assignees.extend(logins.iter().cloned());
            assignees.sort();
            assignees.dedup();
            let caps = service::kind(tracker.doc.forge).issue_caps;
            if !caps.multi_assignee && assignees.len() > 1 {
                return fail(
                    "this service takes one assignee on an issue; unassign the current one first",
                );
            }
        } else {
            assignees.retain(|login| !logins.contains(login));
        }
        Ok(())
    })
}

pub fn labels(store: &Store, filter: Option<&str>, action: &LabelAction) -> Result<Value> {
    let (key, names, add) = match action {
        LabelAction::Add(args) => (&args.key, &args.labels, true),
        LabelAction::Remove(args) => (&args.key, &args.labels, false),
    };
    change(store, filter, key, |_, issue| {
        if add {
            issue.fields.labels.extend(names.iter().cloned());
        } else {
            issue.fields.labels.retain(|label| !names.contains(label));
        }
        Ok(())
    })
}

pub fn edit(store: &Store, filter: Option<&str>, args: &EditArgs) -> Result<Value> {
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
    change(store, filter, &args.key, |_, issue| {
        if let Some(title) = title {
            issue.fields.title = title.trim().to_owned();
        }
        if let Some(body) = body {
            issue.fields.body = body;
        }
        Ok(())
    })
}

/// The PR/MR `arg` names as a link, in the repository of `tracker` unless a URL says otherwise.
fn pr_link(tracker: &Tracker, arg: &str, source: LinkSource) -> Result<PrLink> {
    let pr = forge::parse_pr(arg)?;
    let doc = &tracker.doc;
    let kind = service::kind(doc.forge);
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
        forge: doc.forge,
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

fn add_pr(issue: &mut Issue, link: PrLink) -> bool {
    let known = issue
        .links
        .prs
        .iter()
        .any(|known| known.repo == link.repo && known.number == link.number);
    if !known {
        issue.links.prs.push(link);
    }
    !known
}

pub fn link(store: &Store, filter: Option<&str>, args: &LinkArgs) -> Result<Value> {
    change(store, filter, &args.key, |tracker, issue| {
        let links: &mut Links = &mut issue.links;
        let push = |list: &mut Vec<String>, value: &str| {
            if !list.iter().any(|known| known == value) {
                list.push(value.to_owned());
            }
        };
        match (
            &args.pr,
            &args.issue,
            &args.review,
            &args.task,
            &args.branch,
        ) {
            (Some(pr), ..) => {
                add_pr(issue, pr_link(tracker, pr, LinkSource::Local)?);
            }
            (_, Some(other), ..) => push(&mut links.issues, other),
            (_, _, Some(review), ..) => push(&mut links.reviews, review),
            (_, _, _, Some(task), _) => push(&mut links.tasks, task),
            (_, _, _, _, Some(branch)) => push(&mut links.branches, branch),
            _ => return fail("link needs one of --pr, --issue, --review, --task, --branch"),
        }
        Ok(())
    })
}

pub fn link_pr(store: &Store, filter: Option<&str>, key: &str, pr: &str) -> Result<Value> {
    change(store, filter, key, |tracker, issue| {
        let link = pr_link(tracker, pr, LinkSource::Local)?;
        let url = link.url.clone().unwrap_or_default();
        if add_pr(issue, link) {
            queue_comment(tracker, issue, format!("Linked: {url}"));
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
    let configured = trackers::configured(config, filter)?;
    let [configured] = configured.as_slice() else {
        return fail("several trackers are configured; pass --tracker to say where to create it");
    };
    let tracker = trackers::ensure(store, configured)?;
    let title = read_text(&args.title, "title", false)?;
    let body = read_text(&args.body, "body", true)?;
    let caps = service::kind(tracker.doc.forge).issue_caps;
    if !caps.multi_assignee && args.assignees.len() > 1 {
        return fail("this service takes one assignee on an issue");
    }
    let now = clock::now()?;
    let key = random::token("local-", 6);
    let mut fields = Fields {
        title: title.trim().to_owned(),
        body,
        state: Category::Open,
        reason: None,
        labels: args.labels.clone(),
        assignees: args.assignees.clone(),
    };
    fields.normalize();
    let issue = Issue {
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
    let created = store.update_doc(&tracker.dir, &key, || issue.clone(), |_| Ok(issue.clone()))?;
    Ok(summary(&created))
}
