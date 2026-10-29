//! The reading commands: `list` (with its text and PR/branch lookups), `show` and `status`. They never touch a host.

use serde_json::{Value, json};

use super::cli::{ListArgs, StateFilter};
use super::hierarchy::Index;
use super::merge::{has_pending, pending_changes, unsent_comments};
use super::refs;
use super::trackers::{self, Tracker};
use crate::core::error::Result;
use crate::forge;
use crate::store::Store;
use crate::store::issues::{Category, Issue, TRACKER_FILE};
use crate::store::model::Author;

fn all(store: &Store, filter: Option<&str>) -> Result<Vec<(Tracker, Vec<Issue>)>> {
    let mut found = Vec::new();
    for tracker in trackers::stored(store, filter)? {
        let issues = store.docs::<Issue>(&tracker.dir, &[TRACKER_FILE])?;
        found.push((tracker, issues));
    }
    Ok(found)
}

fn login(author: &Author) -> &str {
    match author {
        Author::Human { login, .. } => login,
        Author::Agent { model, .. } => model,
    }
}

pub fn summary(issue: &Issue) -> Value {
    json!({
        "key": issue.key,
        "tracker": issue.tracker,
        "number": issue.number,
        "title": issue.fields.title,
        "parent": issue.fields.parent,
        "task": issue.task,
        "state": issue.fields.state.as_str(),
        "reason": issue.fields.reason,
        "labels": issue.fields.labels,
        "assignees": issue.fields.assignees,
        "author": issue.author,
        "url": issue.remote.as_ref().map(|remote| remote.url.clone()),
        "updated_at": issue.remote.as_ref().map_or(&issue.updated_at, |remote| &remote.updated_at),
        "comments": issue.comments.len(),
        "pending": has_pending(issue),
        "conflicts": issue.conflicts.len(),
    })
}

/// What `--text` matched in `issue`, in title, body, comments, notes order.
fn text_places(issue: &Issue, needle: &str) -> Vec<&'static str> {
    let has = |haystack: &str| haystack.to_lowercase().contains(needle);
    let mut places = Vec::new();
    if has(&issue.fields.title) {
        places.push("title");
    }
    if has(&issue.fields.body) {
        places.push("body");
    }
    if issue.comments.iter().any(|comment| has(&comment.body)) {
        places.push("comments");
    }
    if issue.notes.iter().any(|note| has(&note.text)) {
        places.push("notes");
    }
    places
}

/// The `--pr`, `--branch` and `--title` lookups: how an issue relates to the PR/MR or branch,
/// empty when it does not (or when none was asked for).
struct Lookup {
    pr: Option<forge::PrRef>,
    branch: Option<String>,
    named: Vec<(Option<String>, u64)>,
    by_branch: Vec<u64>,
}

impl Lookup {
    fn new(args: &ListArgs) -> Result<Self> {
        Ok(Self {
            pr: args.pr.as_deref().map(forge::parse_pr).transpose()?,
            branch: args.branch.clone(),
            named: args
                .title
                .as_deref()
                .map(refs::references)
                .unwrap_or_default(),
            by_branch: args
                .branch
                .as_deref()
                .map(refs::branch_numbers)
                .unwrap_or_default(),
        })
    }

    fn asked(&self, args: &ListArgs) -> bool {
        args.pr.is_some() || args.branch.is_some() || args.title.is_some()
    }

    fn via(&self, repo: &str, issue: &Issue) -> Vec<&'static str> {
        let mut via = Vec::new();
        if let Some(pr) = &self.pr {
            let wanted = pr.location.as_ref().map_or(repo, |(_, repo)| repo.as_str());
            if issue
                .links
                .prs
                .iter()
                .any(|link| link.number == pr.number && link.repo.eq_ignore_ascii_case(wanted))
            {
                via.push("link");
            }
        }
        if let Some(branch) = &self.branch {
            if issue
                .links
                .prs
                .iter()
                .any(|link| link.branch.as_ref() == Some(branch))
            {
                via.push("pr-branch");
            }
            if issue.links.branches.contains(branch) {
                via.push("linked-branch");
            }
            if issue
                .number
                .is_some_and(|number| self.by_branch.contains(&number))
            {
                via.push("branch-name");
            }
        }
        let titled = self.named.iter().any(|(named_repo, number)| {
            issue.number == Some(*number)
                && named_repo
                    .as_ref()
                    .is_none_or(|named| named.eq_ignore_ascii_case(repo))
        });
        if titled {
            via.push("title");
        }
        via
    }
}

pub fn list(store: &Store, filter: Option<&str>, args: &ListArgs) -> Result<Value> {
    let lookup = Lookup::new(args)?;
    let asked = lookup.asked(args);
    let needle = args.text.as_deref().map(str::to_lowercase);
    // A text or PR/branch lookup searches closed issues too unless `--state` says otherwise.
    let state = args
        .state
        .unwrap_or(if asked || needle.is_some() || args.parent.is_some() {
            StateFilter::All
        } else {
            StateFilter::Open
        });
    let parent = args
        .parent
        .as_deref()
        .map(|key| trackers::find(store, filter, key))
        .transpose()?;
    let mut issues = Vec::new();
    for (tracker, found) in all(store, filter)? {
        let index = Index::from_issues(found.clone())?;
        if parent
            .as_ref()
            .is_some_and(|(wanted, _)| wanted.doc.key != tracker.doc.key)
        {
            continue;
        }
        for mut issue in found {
            index.normalize(&mut issue);
            if let Some((_, parent)) = &parent
                && issue.fields.parent.as_deref() != Some(parent.key.as_str())
            {
                continue;
            }

            let fields = &issue.fields;
            let keep = match state {
                StateFilter::All => true,
                StateFilter::Open => fields.state == Category::Open,
                StateFilter::Closed => fields.state == Category::Closed,
            } && args
                .label
                .as_ref()
                .is_none_or(|label| fields.labels.contains(label))
                && args
                    .assignee
                    .as_ref()
                    .is_none_or(|login| fields.assignees.contains(login))
                && args
                    .author
                    .as_ref()
                    .is_none_or(|login| issue.author.as_ref() == Some(login))
                && (!args.pending || has_pending(&issue));
            if !keep {
                continue;
            }
            let mut view = summary(&issue);
            if let Some(needle) = &needle {
                let places = text_places(&issue, needle);
                if places.is_empty() {
                    continue;
                }
                view["matched"] = json!(places);
            }
            if asked {
                let via = lookup.via(&tracker.doc.repo, &issue);
                if via.is_empty() {
                    continue;
                }
                view["via"] = json!(via);
            }
            issues.push(view);
        }
    }
    Ok(json!({ "count": issues.len(), "issues": issues }))
}

pub fn show(store: &Store, filter: Option<&str>, key: &str) -> Result<Value> {
    let (tracker, mut issue) = trackers::find(store, filter, key)?;
    let index = Index::load(store, &tracker)?;
    index.normalize(&mut issue);
    let changes = pending_changes(&issue);
    let comments: Vec<Value> = issue
        .comments
        .iter()
        .map(|comment| {
            json!({
                "id": comment.id,
                "author": login(&comment.author),
                "body": comment.body,
                "created_at": comment.created_at,
                "post": comment.delivery.post,
                "remote_id": comment.remote.as_ref().map(|remote| remote.id.clone()),
                "url": comment.remote.as_ref().and_then(|remote| remote.url.clone()),
                "deleted_remote": comment.deleted_remote,
            })
        })
        .collect();
    let mut view = summary(&issue);
    view["sub_issues"] = json!(index.children(&issue.key));
    view["body"] = json!(issue.fields.body);
    view["native_state"] = json!(issue.native_state);
    view["milestone"] = json!(issue.milestone);
    view["due_date"] = json!(issue.due_date);
    view["created_at"] = json!(issue.created_at);
    view["synced_at"] = json!(issue.synced_at);
    view["comments"] = json!(comments);
    view["links"] = json!(issue.links);
    view["notes"] = json!(issue.notes);
    view["queued"] = json!({
        "create": issue.number.is_none(),
        "parent": if issue.base.as_ref().is_some_and(|base| base.parent != issue.fields.parent) {
            json!({"key": issue.fields.parent})
        } else { Value::Null },
        "title": changes.title,
        "body": changes.body,
        "state": changes.state.map(|(state, reason)| json!({"state": state.as_str(), "reason": reason})),
        "labels": changes.labels,
        "assignees": changes.assignees,
        "comments": unsent_comments(&issue).iter().map(|comment| comment.id.clone()).collect::<Vec<_>>(),
    });
    view["conflicts"] = json!(
        issue
            .conflicts
            .iter()
            .map(|conflict| json!({"field": conflict.field, "seen_at": conflict.seen_at}))
            .collect::<Vec<_>>()
    );
    view["errors"] = json!(issue.errors);
    Ok(view)
}

/// `current` is the configured trackers when the configuration loads: a tracker's `push` is
/// whether its mode is `manual` or `auto` now, not as of its last sync.
pub fn status(
    store: &Store,
    filter: Option<&str>,
    current: Option<&[crate::core::config::Tracker]>,
) -> Result<Value> {
    let mut reports = Vec::new();
    for (tracker, issues) in all(store, filter)? {
        let open = issues
            .iter()
            .filter(|issue| issue.fields.state == Category::Open)
            .count();
        let pending: Vec<&str> = issues
            .iter()
            .filter(|issue| has_pending(issue))
            .map(|issue| issue.key.as_str())
            .collect();
        let conflicts: Vec<Value> = issues
            .iter()
            .filter(|issue| !issue.conflicts.is_empty())
            .map(|issue| {
                json!({
                    "issue": issue.key,
                    "fields": issue.conflicts.iter().map(|conflict| conflict.field.clone()).collect::<Vec<_>>(),
                })
            })
            .collect();
        let errors: Vec<Value> = issues
            .iter()
            .flat_map(|issue| {
                issue.errors.iter().map(|error| {
                    json!({"issue": issue.key, "what": error.what, "message": error.message, "refused": error.refused})
                })
            })
            .collect();
        let doc = &tracker.doc;
        let pushes = match current {
            Some(current) => current.iter().any(|configured| {
                configured.is_at(&doc.host, &doc.repo)
                    && configured.push_mode() != crate::core::config::Mode::Off
            }),
            None => doc.push,
        };
        reports.push(json!({
            "tracker": doc.key,
            "forge": doc.forge,
            "host": doc.host,
            "repo": doc.repo,
            "push": pushes,
            "synced_at": doc.synced_at,
            "watermark": doc.watermark,
            "issues": issues.len(),
            "open": open,
            "closed": issues.len() - open,
            "pending": pending,
            "conflicts": conflicts,
            "errors": errors,
        }));
    }
    Ok(json!({ "trackers": reports }))
}
