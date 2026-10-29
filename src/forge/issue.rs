//! What a service tells about issues and what it is asked to change, in the services' common
//! terms. The persisted shapes are in `store::issues`.

use serde_json::Value;

use crate::core::clock;
use crate::core::config::Scope;
use crate::core::error::{Error, Result};
use crate::forge::{self, CommentData};
use crate::store::issues::{Category, PrLink};

/// One issue as the host lists or returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueData {
    pub number: u64,
    pub id: String,
    pub url: String,
    pub title: String,
    pub parent: Option<String>,
    pub task: bool,
    pub body: String,
    pub state: Category,
    pub reason: Option<String>,
    /// The host's own state word.
    pub native_state: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub author: Option<String>,
    pub milestone: Option<String>,
    pub due_date: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Default)]
pub struct IssueHierarchy {
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub task: bool,
}

pub struct IssueFetched {
    pub issue: IssueData,
    /// Oldest first, without the host's own system notes.
    pub comments: Vec<CommentData>,
    pub prs: Vec<PrLink>,
}

/// Which issues a listing asks for.
pub struct ListQuery<'a> {
    pub repo: &'a str,
    pub scope: Scope,
    /// RFC 3339; only issues updated at or after it.
    pub since: &'a str,
    /// The authenticated user, for the scopes that name one.
    pub login: &'a str,
}

/// The fields of an issue to change; `None` leaves a field alone. `labels` and `assignees` are
/// the whole new set.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Changes {
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<(Category, Option<String>)>,
    pub labels: Option<Vec<String>>,
    pub assignees: Option<Vec<String>>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

pub struct NewIssue<'a> {
    pub task: bool,
    pub title: &'a str,
    pub body: &'a str,
    pub labels: &'a [String],
    pub assignees: &'a [String],
}

pub struct IssueCaps {
    /// An issue takes more than one assignee.
    pub multi_assignee: bool,
    /// Closing takes a reason (`completed`, `not_planned`).
    pub close_reasons: bool,
}

/// Names of the `key` field of each item of the array at `pointer`.
pub fn names(value: &Value, pointer: &str, key: &str) -> Vec<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get(key)?.as_str().map(str::to_owned))
        .collect()
}

/// Strings of the array at `pointer`.
pub fn strings(value: &Value, pointer: &str) -> Vec<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_str().map(str::to_owned))
        .collect()
}

/// `issues` from one or several listings, once per number and in number order.
pub fn merge_listing(mut issues: Vec<IssueData>) -> Vec<IssueData> {
    issues.sort_by_key(|issue| issue.number);
    issues.dedup_by_key(|issue| issue.number);
    issues
}

/// A required, non-empty id or text field.
pub fn field(value: &Value, pointer: &str, what: &str) -> Result<String> {
    forge::id(value, pointer).ok_or_else(|| forge::missing(pointer, what))
}

/// The fields a GitHub and a GitLab issue share, read from the JSON pointers of the host.
/// The caller sets the rest: `reason`, `labels`, `assignees` and `due_date`.
pub struct IssuePointers {
    /// The issue number, `number` on GitHub and `iid` on GitLab.
    pub number: &'static str,
    pub id: &'static str,
    pub url: &'static str,
    pub body: &'static str,
    pub author: &'static str,
}

pub fn parse_issue(item: &Value, what: &str, at: &IssuePointers) -> Result<IssueData> {
    let closed = item["state"] == "closed";
    Ok(IssueData {
        number: item[at.number]
            .as_u64()
            .ok_or_else(|| Error(format!("{what} has no {}", at.number)))?,
        id: field(item, at.id, what)?,
        url: field(item, at.url, what)?,
        title: forge::text(item, "/title").unwrap_or_default(),
        parent: None,
        task: item["issue_type"] == "task",
        body: forge::text(item, at.body).unwrap_or_default(),
        state: if closed {
            Category::Closed
        } else {
            Category::Open
        },
        reason: None,
        native_state: forge::text(item, "/state").unwrap_or_default(),
        labels: Vec::new(),
        assignees: Vec::new(),
        author: forge::text(item, at.author),
        milestone: forge::text(item, "/milestone/title"),
        due_date: None,
        created_at: clock::normalize(&field(item, "/created_at", what)?)?,
        updated_at: clock::normalize(&field(item, "/updated_at", what)?)?,
    })
}

pub fn related_key(
    item: &Value,
    url_field: &str,
    number_field: &str,
    marker: &str,
) -> Result<String> {
    let url = field(item, url_field, "related issue")?;
    let path = url
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, path)| path);
    let repo = path
        .and_then(|path| path.rsplit_once(marker).map(|(repo, _)| repo))
        .ok_or_else(|| Error(format!("invalid related issue URL: {url}")))?;
    let number = item[number_field]
        .as_u64()
        .or_else(|| item[number_field].as_str()?.parse().ok())
        .filter(|number| *number > 0)
        .ok_or_else(|| Error("related issue has no number".into()))?;
    Ok(format!("{}#{number}", repo.to_ascii_lowercase()))
}

pub fn parent_subject<'a>(
    key: &'a str,
    fallback: &'a str,
) -> Result<crate::forge::service::IssueSubject<'a>> {
    let (repo, number) = key.rsplit_once('#').unwrap_or((fallback, key));
    let number = number
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(|| Error(format!("parent {key} has not been published yet")))?;
    Ok(crate::forge::service::IssueSubject { repo, number })
}
