//! What a service tells about issues and what it is asked to change, in the services' common
//! terms. The persisted shapes are in `store::issues`.

use serde_json::Value;

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

/// A required, non-empty id or text field.
pub fn field(value: &Value, pointer: &str, what: &str) -> Result<String> {
    forge::id(value, pointer).ok_or_else(|| Error(format!("{what} is missing {pointer}")))
}
