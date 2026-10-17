//! The reading commands: `list`, `show`, `search`, `status` and `find`. They never touch a host.

use serde_json::{Value, json};

use super::cli::{FindArgs, ListArgs, StateFilter};
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

pub fn list(store: &Store, filter: Option<&str>, args: &ListArgs) -> Result<Value> {
    let mut issues = Vec::new();
    for (_, found) in all(store, filter)? {
        for issue in found {
            let fields = &issue.fields;
            let keep = match args.state {
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
            if keep {
                issues.push(summary(&issue));
            }
        }
    }
    Ok(json!({ "count": issues.len(), "issues": issues }))
}

pub fn show(store: &Store, filter: Option<&str>, key: &str) -> Result<Value> {
    let (_, issue) = trackers::find(store, filter, key)?;
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

pub fn search(store: &Store, filter: Option<&str>, text: &str) -> Result<Value> {
    let needle = text.to_lowercase();
    let has = |haystack: &str| haystack.to_lowercase().contains(&needle);
    let mut found = Vec::new();
    for (_, issues) in all(store, filter)? {
        for issue in issues {
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
            if !places.is_empty() {
                let mut view = summary(&issue);
                view["matched"] = json!(places);
                found.push(view);
            }
        }
    }
    Ok(json!({ "count": found.len(), "issues": found }))
}

pub fn status(store: &Store, filter: Option<&str>) -> Result<Value> {
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
        reports.push(json!({
            "tracker": doc.key,
            "forge": doc.forge,
            "host": doc.host,
            "repo": doc.repo,
            "push": doc.push,
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

pub fn find(store: &Store, filter: Option<&str>, args: &FindArgs) -> Result<Value> {
    let pr = args.pr.as_deref().map(forge::parse_pr).transpose()?;
    let named = args
        .title
        .as_deref()
        .map(refs::references)
        .unwrap_or_default();
    let by_branch = args
        .branch
        .as_deref()
        .map(refs::branch_numbers)
        .unwrap_or_default();
    let mut found = Vec::new();
    for (tracker, issues) in all(store, filter)? {
        let repo = tracker.doc.repo.as_str();
        for issue in issues {
            let mut via = Vec::new();
            if let Some(pr) = &pr {
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
            if let Some(branch) = &args.branch {
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
                    .is_some_and(|number| by_branch.contains(&number))
                {
                    via.push("branch-name");
                }
            }
            let titled = named.iter().any(|(named_repo, number)| {
                issue.number == Some(*number)
                    && named_repo
                        .as_ref()
                        .is_none_or(|named| named.eq_ignore_ascii_case(repo))
            });
            if titled {
                via.push("title");
            }
            if !via.is_empty() {
                let mut view = summary(&issue);
                view["via"] = json!(via);
                found.push(view);
            }
        }
    }
    Ok(json!({ "count": found.len(), "issues": found }))
}
