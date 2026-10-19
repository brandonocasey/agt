//! The commands that change only the local copy: `link`, `note` and every write that waits for
//! `push`. Nothing here talks to a host.

use serde_json::Value;

use super::cli::{CreateArgs, EditArgs, LinkArgs, Reason, State};
use super::trackers::{self, Tracker, doc_name};
use super::view::summary;
use crate::core::clock;
use crate::core::config::Config;
use crate::core::error::{Result, fail};
use crate::core::local::read_stdin;
use crate::core::random;
use crate::forge::{self, service};
use crate::store::Store;
use crate::store::issues::{Category, Fields, Issue, LinkSource, Links, Note, PrLink, VERSION};
use crate::store::model::{Author, Comment, Delivery, PostState};

const MAX_TEXT: usize = 65_536;

/// `text`, or stdin when it is `-`; never empty unless `allow_empty`.
fn read_text(text: &str, what: &str, allow_empty: bool) -> Result<String> {
    let text = if text == "-" {
        read_stdin(what)?
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
    let updated = store.update_existing_doc(&tracker.dir, &name, |issue: &mut Issue| {
        edit(&tracker, issue)?;
        issue.fields.normalize();
        issue.updated_at.clone_from(&now);
        Ok(issue.clone())
    })?;
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
    assignees.extend(add.iter().cloned());
    assignees.sort();
    assignees.dedup();
    let caps = tracker
        .doc
        .forge
        .map(|forge| service::kind(forge).issue_caps);
    if caps.is_some_and(|caps| !caps.multi_assignee) && assignees.len() > 1 {
        return fail(
            "this service takes one assignee on an issue; remove the current one with --remove-assignee",
        );
    }
    Ok(())
}

pub fn edit(store: &Store, filter: Option<&str>, args: &EditArgs) -> Result<Value> {
    if args.title.is_none()
        && args.body.is_none()
        && args.state.is_none()
        && args.add_labels.is_empty()
        && args.remove_labels.is_empty()
        && args.add_assignees.is_empty()
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
        let labels = &mut issue.fields.labels;
        labels.retain(|label| !args.remove_labels.contains(label));
        labels.extend(args.add_labels.iter().cloned());
        assign(tracker, issue, &args.add_assignees, &args.remove_assignees)
    })
}

/// The PR/MR `arg` names as a link, in the repository of `tracker` unless a URL says otherwise.
fn pr_link(tracker: &Tracker, arg: &str, source: LinkSource) -> Result<PrLink> {
    let pr = forge::parse_pr(arg)?;
    let doc = &tracker.doc;
    let forge = match doc.forge {
        Some(forge) => forge,
        None => {
            let Some((host, repo)) = pr.location else {
                return fail("a local tracker needs a full PR/MR URL");
            };
            let forge = if arg.contains("/-/merge_requests/") {
                crate::store::model::Forge::Gitlab
            } else {
                crate::store::model::Forge::Github
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
                let link = pr_link(tracker, pr, LinkSource::Local)?;
                let body = format!("Linked: {}", link.url.as_deref().unwrap_or_default());
                add_pr(issue, link);
                // Keyed on the comment, not the link, so `--comment` still works on a PR that
                // an earlier `link` or `sync` already recorded.
                if args.comment && !issue.comments.iter().any(|known| known.body == body) {
                    queue_comment(tracker, issue, body);
                }
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
    let caps = tracker
        .doc
        .forge
        .map(|forge| service::kind(forge).issue_caps);
    if caps.is_some_and(|caps| !caps.multi_assignee) && args.assignees.len() > 1 {
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
