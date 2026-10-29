//! Bringing a fetched issue together with the local copy: a three-way merge of the writable
//! fields against `base`, and the comments by remote id and marker.

use crate::forge::CommentData;
use crate::forge::issue::{Changes, IssueData, IssueFetched};
use crate::forge::markers::{is_landed_as, strip_agent};
use crate::store::issues::{
    Conflict, Fields, Issue, LinkSource, Links, PrLink, RemoteInfo, VERSION,
};
use crate::store::model::{Comment, PostState};

/// The writable fields of `data`, as the host holds them. A marker `agt` added to the body is
/// not part of what a person wrote.
pub fn remote_fields(data: &IssueData) -> Fields {
    let mut fields = Fields {
        title: data.title.clone(),
        parent: data.parent.clone(),
        body: strip_agent(&data.body),
        state: data.state,
        reason: data.reason.clone(),
        labels: data.labels.clone(),
        assignees: data.assignees.clone(),
    };
    fields.normalize();
    fields
}

/// A new local copy of a remote issue.
pub fn from_remote(tracker: &str, repo: &str, data: &IssueData, now: &str) -> Issue {
    let fields = remote_fields(data);
    Issue {
        version: VERSION,
        key: format!("{repo}#{}", data.number),
        tracker: tracker.to_owned(),
        local_key: None,
        task: data.task,
        sub_issues: Vec::new(),
        number: Some(data.number),
        remote: Some(RemoteInfo {
            id: data.id.clone(),
            url: data.url.clone(),
            // Empty until a sync reads the comments, which a listing does not carry.
            updated_at: String::new(),
        }),
        base: Some(fields.clone()),
        fields,
        author: data.author.clone(),
        native_state: Some(data.native_state.clone()),
        milestone: data.milestone.clone(),
        due_date: data.due_date.clone(),
        created_at: data.created_at.clone(),
        updated_at: now.to_owned(),
        synced_at: None,
        comments: Vec::new(),
        links: Links::default(),
        notes: Vec::new(),
        conflicts: Vec::new(),
        errors: Vec::new(),
        create: None,
    }
}

/// How many fields and comments a merge touched.
#[derive(Default)]
pub struct Merged {
    pub conflicts: usize,
    pub comments_new: usize,
    pub comments_claimed: usize,
}

/// Applies a fetch to `issue`. Local edits stay unless the remote changed the same scalar,
/// where the remote wins and the local value is kept in a conflict for `push --force`; labels
/// and assignees merge as additions and removals. Afterwards `base` is the remote's value, so
/// what differs from `base` is what is still to send.
pub fn apply(issue: &mut Issue, fetched: &IssueFetched, me: &str, now: &str) -> Merged {
    let data = &fetched.issue;
    let remote = remote_fields(data);
    let mut merged = Merged::default();
    let base = issue.base.clone().unwrap_or_else(|| remote.clone());
    let local = issue.fields.clone();
    let mut next = remote.clone();
    let mut conflicted = Vec::new();

    let scalar =
        |name: &str, same: &dyn Fn(&Fields, &Fields) -> bool, conflicted: &mut Vec<String>| {
            if same(&local, &base) || same(&local, &remote) {
                return false;
            }
            if same(&remote, &base) {
                return true;
            }
            conflicted.push(name.to_owned());
            false
        };
    if scalar("title", &|a, b| a.title == b.title, &mut conflicted) {
        next.title = local.title.clone();
    }
    if scalar("parent", &|a, b| a.parent == b.parent, &mut conflicted) {
        next.parent = local.parent.clone();
    }
    if scalar("body", &|a, b| a.body == b.body, &mut conflicted) {
        next.body = local.body.clone();
    }
    if scalar(
        "state",
        &|a, b| a.state == b.state && a.reason == b.reason,
        &mut conflicted,
    ) {
        next.state = local.state;
        next.reason = local.reason.clone();
    }
    next.labels = merge_set(&base.labels, &local.labels, &remote.labels);
    next.assignees = merge_set(&base.assignees, &local.assignees, &remote.assignees);
    next.normalize();

    for field in conflicted {
        issue.conflicts.retain(|known| known.field != field);
        issue.conflicts.push(Conflict {
            field,
            local: local.clone(),
            remote: remote.clone(),
            seen_at: now.to_owned(),
        });
        merged.conflicts += 1;
    }
    issue.task = data.task;
    issue.fields = next;
    issue.base = Some(remote);
    issue.number = Some(data.number);
    issue.remote = Some(RemoteInfo {
        id: data.id.clone(),
        url: data.url.clone(),
        updated_at: data.updated_at.clone(),
    });
    issue.author = data.author.clone();
    issue.native_state = Some(data.native_state.clone());
    issue.milestone = data.milestone.clone();
    issue.due_date = data.due_date.clone();
    issue.synced_at = Some(now.to_owned());
    clear_parent_error(issue);
    merge_links(issue, &fetched.prs);
    merge_comments(issue, &fetched.comments, me, now, &mut merged);
    merged
}

pub fn apply_parent(issue: &mut Issue, parent: &Option<String>, task: bool, now: &str) -> usize {
    let Some(base) = &mut issue.base else {
        return 0;
    };
    let mut conflicts = 0;
    if issue.fields.parent == base.parent || issue.fields.parent == *parent {
        issue.fields.parent.clone_from(parent);
    } else if *parent != base.parent {
        let mut remote_fields = base.clone();
        remote_fields.parent.clone_from(parent);
        issue
            .conflicts
            .retain(|conflict| conflict.field != "parent");
        issue.conflicts.push(Conflict {
            field: "parent".into(),
            local: issue.fields.clone(),
            remote: remote_fields,
            seen_at: now.to_owned(),
        });
        issue.fields.parent.clone_from(parent);
        conflicts = 1;
    }
    base.parent.clone_from(parent);
    issue.task = task;
    clear_parent_error(issue);
    conflicts
}

fn clear_parent_error(issue: &mut Issue) {
    if issue
        .base
        .as_ref()
        .is_some_and(|base| base.parent == issue.fields.parent)
        && !issue
            .conflicts
            .iter()
            .any(|conflict| conflict.field == "parent")
    {
        issue.errors.retain(|error| error.what != "parent");
    }
}

/// The remote's set, minus what the local side removed since `base`, plus what it added.
fn merge_set(base: &[String], local: &[String], remote: &[String]) -> Vec<String> {
    let mut merged: Vec<String> = remote
        .iter()
        .filter(|item| !(base.contains(item) && !local.contains(item)))
        .cloned()
        .collect();
    merged.extend(local.iter().filter(|item| !base.contains(item)).cloned());
    merged.sort();
    merged.dedup();
    merged
}

fn merge_links(issue: &mut Issue, remote: &[PrLink]) {
    let same = |a: &PrLink, b: &PrLink| a.repo == b.repo && a.number == b.number;
    issue.links.prs.retain(|link| {
        link.source == LinkSource::Local && !remote.iter().any(|found| same(found, link))
    });
    issue.links.prs.splice(0..0, remote.iter().cloned());
}

fn merge_comments(
    issue: &mut Issue,
    remote: &[CommentData],
    me: &str,
    now: &str,
    merged: &mut Merged,
) {
    for incoming in remote {
        let body = strip_agent(&incoming.body);
        let known = issue.comments.iter().position(|comment| {
            comment
                .remote
                .as_ref()
                .is_some_and(|remote| remote.id == incoming.id)
        });
        if let Some(index) = known {
            let comment = &mut issue.comments[index];
            // Our own comment keeps the text it was written with.
            if comment.is_imported() {
                comment.body = body;
            }
            comment.deleted_remote = false;
            comment.deleted_seen_at = None;
            if let Some(remote) = comment.remote.as_mut() {
                remote.editor.clone_from(&incoming.editor);
            }
            continue;
        }
        let claimed = issue.comments.iter().position(|comment| {
            comment.remote.is_none()
                && incoming.login.as_deref() == Some(me)
                && is_landed_as(&comment.delivery, &comment.id, &incoming.body)
        });
        if let Some(index) = claimed {
            let comment = &mut issue.comments[index];
            comment.delivery.post = PostState::Posted;
            comment.remote = Some(incoming.remote(me));
            merged.comments_claimed += 1;
            continue;
        }
        issue.comments.push(incoming.imported(me, body, "unknown"));
        merged.comments_new += 1;
    }
    for comment in &mut issue.comments {
        let Some(remote_id) = comment.remote.as_ref().map(|remote| remote.id.clone()) else {
            continue;
        };
        if !remote.iter().any(|incoming| incoming.id == remote_id) && !comment.deleted_remote {
            comment.deleted_remote = true;
            comment.deleted_seen_at = Some(now.to_owned());
        }
    }
}

/// The fields that differ from `base`: what a push has to send. Empty for an issue the host
/// does not know yet.
pub fn pending_changes(issue: &Issue) -> Changes {
    let Some(base) = &issue.base else {
        return Changes::default();
    };
    let fields = &issue.fields;
    Changes {
        title: (fields.title != base.title).then(|| fields.title.clone()),
        body: (fields.body != base.body).then(|| fields.body.clone()),
        state: (fields.state != base.state || fields.reason != base.reason)
            .then(|| (fields.state, fields.reason.clone())),
        labels: (fields.labels != base.labels).then(|| fields.labels.clone()),
        assignees: (fields.assignees != base.assignees).then(|| fields.assignees.clone()),
    }
}

/// Comments that still have to be sent: written here and not yet on the host.
pub fn unsent_comments(issue: &Issue) -> Vec<&Comment> {
    issue
        .comments
        .iter()
        .filter(|comment| comment.remote.is_none() && !comment.local)
        .collect()
}

pub fn has_pending(issue: &Issue) -> bool {
    issue.number.is_none()
        || issue
            .base
            .as_ref()
            .is_some_and(|base| issue.fields.parent != base.parent)
        || !pending_changes(issue).is_empty()
        || !unsent_comments(issue).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::markers;
    use crate::store::issues::Category;
    use crate::store::model::Author;

    fn data(title: &str, labels: &[&str]) -> IssueData {
        IssueData {
            number: 1,
            id: "I_1".into(),
            url: "https://example.com/o/r/issues/1".into(),
            title: title.into(),
            parent: None,
            task: false,
            body: "body".into(),
            state: Category::Open,
            reason: None,
            native_state: "open".into(),
            labels: labels.iter().map(|label| (*label).into()).collect(),
            assignees: Vec::new(),
            author: Some("alice".into()),
            milestone: None,
            due_date: None,
            created_at: "2026-10-01T00:00:00Z".into(),
            updated_at: "2026-10-02T00:00:00Z".into(),
        }
    }

    fn fetched(data: IssueData) -> IssueFetched {
        IssueFetched {
            issue: data,
            comments: Vec::new(),
            prs: Vec::new(),
        }
    }

    fn synced(title: &str, labels: &[&str]) -> Issue {
        from_remote("t", "o/r", &data(title, labels), "2026-10-02T00:00:00Z")
    }

    const NOW: &str = "2026-10-03T00:00:00Z";

    #[test]
    fn only_the_remote_changed_takes_the_remote() {
        let mut issue = synced("Old", &[]);
        let merged = apply(&mut issue, &fetched(data("New", &[])), "me", NOW);
        assert_eq!(merged.conflicts, 0);
        assert_eq!(issue.fields.title, "New");
        assert!(pending_changes(&issue).is_empty());
    }

    #[test]
    fn only_the_local_side_changed_stays_queued() {
        let mut issue = synced("Old", &[]);
        issue.fields.title = "Mine".into();
        let merged = apply(&mut issue, &fetched(data("Old", &[])), "me", NOW);
        assert_eq!(merged.conflicts, 0);
        assert_eq!(issue.fields.title, "Mine");
        assert_eq!(pending_changes(&issue).title.as_deref(), Some("Mine"));
    }

    #[test]
    fn both_changed_a_scalar_the_remote_wins_and_the_local_value_is_kept() {
        let mut issue = synced("Old", &[]);
        issue.fields.title = "Mine".into();
        let merged = apply(&mut issue, &fetched(data("Theirs", &[])), "me", NOW);
        assert_eq!(merged.conflicts, 1);
        assert_eq!(issue.fields.title, "Theirs");
        assert!(pending_changes(&issue).is_empty());
        assert_eq!(issue.conflicts[0].field, "title");
        assert_eq!(issue.conflicts[0].local.title, "Mine");
    }

    #[test]
    fn both_changed_to_the_same_value_is_no_conflict() {
        let mut issue = synced("Old", &[]);
        issue.fields.title = "Same".into();
        let merged = apply(&mut issue, &fetched(data("Same", &[])), "me", NOW);
        assert_eq!(merged.conflicts, 0);
        assert!(pending_changes(&issue).is_empty());
    }

    #[test]
    fn state_conflicts_like_a_scalar() {
        let mut issue = synced("T", &[]);
        issue.fields.state = Category::Closed;
        let mut remote = data("T", &[]);
        remote.state = Category::Closed;
        remote.reason = Some("not_planned".into());
        let merged = apply(&mut issue, &fetched(remote), "me", NOW);
        assert_eq!(merged.conflicts, 1);
        assert_eq!(issue.fields.reason.as_deref(), Some("not_planned"));
    }

    #[test]
    fn labels_merge_as_additions_and_removals() {
        let mut issue = synced("T", &["a", "b"]);
        issue.fields.labels = vec!["a".into(), "c".into()];
        let merged = apply(&mut issue, &fetched(data("T", &["a", "b", "d"])), "me", NOW);
        assert_eq!(merged.conflicts, 0);
        // The local side removed b and added c; the remote added d.
        assert_eq!(issue.fields.labels, ["a", "c", "d"]);
        assert_eq!(issue.base.as_ref().unwrap().labels, ["a", "b", "d"]);
        let changes = pending_changes(&issue);
        assert_eq!(
            changes.labels,
            Some(vec!["a".into(), "c".into(), "d".into()])
        );
    }

    fn comment(id: &str, login: &str, body: &str) -> CommentData {
        CommentData {
            id: id.into(),
            url: None,
            body: body.into(),
            created_at: "2026-10-02T01:00:00Z".into(),
            login: Some(login.into()),
            editor: None,
            review_id: None,
        }
    }

    #[test]
    fn a_comment_is_claimed_by_its_marker_and_attempt_only() {
        let mut issue = synced("T", &[]);
        let mut local = Comment::new(
            "c1".into(),
            Author::Human {
                login: "me".into(),
                git_name: false,
            },
            "Hello".into(),
            NOW.into(),
            PostState::Pending,
            None,
        );
        local.delivery.begin("at-1", Some("Hello".into()));
        issue.comments.push(local);
        let marked = |attempt: &str| format!("Hello\n\n{}", markers::agent("c1", Some(attempt)));
        let mut incoming = fetched(data("T", &[]));
        // Same id but another attempt, and a stranger's copy: neither is claimed.
        incoming.comments = vec![
            comment("10", "me", &marked("at-2")),
            comment("11", "bob", &marked("at-1")),
        ];
        let merged = apply(&mut issue, &incoming, "me", NOW);
        assert_eq!((merged.comments_claimed, merged.comments_new), (0, 2));
        assert!(issue.comments[0].remote.is_none());
        incoming.comments = vec![comment("12", "me", &marked("at-1"))];
        let merged = apply(&mut issue, &incoming, "me", NOW);
        assert_eq!(merged.comments_claimed, 1);
        assert_eq!(issue.comments[0].remote.as_ref().unwrap().id, "12");
        assert!(unsent_comments(&issue).is_empty());
        // The comments that vanished from the host are flagged, not dropped.
        assert!(issue.comments.iter().any(|c| c.deleted_remote));
    }

    #[test]
    fn imported_comments_drop_the_marker_text() {
        let mut issue = synced("T", &[]);
        let mut incoming = fetched(data("T", &[]));
        incoming.comments = vec![comment(
            "5",
            "bob",
            &format!("Hi\n\n{}", markers::agent("c9", None)),
        )];
        apply(&mut issue, &incoming, "me", NOW);
        assert_eq!(issue.comments[0].id, "remote-5");
        assert_eq!(issue.comments[0].body, "Hi");
    }
}
