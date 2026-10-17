//! GitLab issues over REST. Comments are the user notes of the issue's discussions; the MRs that
//! close or relate to an issue come from `closed_by` and `related_merge_requests`.

use serde_json::{Value, json};

use super::{CAPS, Gitlab, discussions};
use crate::core::clock;
use crate::core::config::Scope;
use crate::core::error::{Error, Result};
use crate::forge::issue::{
    Changes, IssueCaps, IssueData, IssueFetched, ListQuery, NewIssue, field, names,
};
use crate::forge::service::{IssueService, IssueSubject, Method, Request, Service};
use crate::forge::{self, CommentData};
use crate::store::issues::{Category, LinkSource, PrLink};
use crate::store::model::Forge;

/// Premium and Ultimate take several assignees; Free takes one, and a host that is not
/// asked cannot say which it is.
pub(super) const ISSUE_CAPS: IssueCaps = IssueCaps {
    multi_assignee: false,
    close_reasons: false,
};

fn project(repo: &str) -> String {
    format!("projects/{}", forge::encode(repo))
}

fn base(subject: IssueSubject) -> String {
    format!("{}/issues/{}", project(subject.repo), subject.number)
}

impl IssueService for Gitlab {
    fn list_changed(&self, query: &ListQuery) -> Result<Vec<IssueData>> {
        // GitLab has no "mentioned" filter, so involved is what the user wrote or holds.
        let scopes: &[&str] = match query.scope {
            Scope::All => &["all"],
            Scope::Assigned => &["assigned_to_me"],
            Scope::Involved => &["created_by_me", "assigned_to_me"],
        };
        let mut found: Vec<IssueData> = Vec::new();
        for scope in scopes {
            let path = format!(
                "{}/issues?scope={scope}&state=all&updated_after={}&order_by=updated_at&sort=asc&per_page=100",
                project(query.repo),
                forge::encode(query.since)
            );
            for item in self
                .transport()
                .items(&Request::get(path), &CAPS.pagination)?
            {
                let issue = parse_issue(&item)?;
                if !found.iter().any(|known| known.number == issue.number) {
                    found.push(issue);
                }
            }
        }
        found.sort_by_key(|issue| issue.number);
        Ok(found)
    }

    fn fetch_issue(&self, subject: IssueSubject) -> Result<IssueFetched> {
        let transport = self.transport();
        let base = base(subject);
        let issue = parse_issue(&transport.json(&Request::get(base.clone()))?)?;
        let threads = discussions(transport, &base, &issue.url)?;
        let mut comments: Vec<CommentData> = threads
            .into_iter()
            .flat_map(|thread| thread.comments)
            .collect();
        comments.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        let mut prs: Vec<PrLink> = Vec::new();
        for route in ["closed_by", "related_merge_requests"] {
            let items = match transport.items(
                &Request::get(format!("{base}/{route}?per_page=100")),
                &CAPS.pagination,
            ) {
                Ok(items) => items,
                // Older servers lack one of the routes; the issue itself was just read.
                Err(error) if error.0.contains("(HTTP 404)") => Vec::new(),
                Err(error) => return Err(error),
            };
            for item in &items {
                let link = parse_mr(item, subject.repo);
                if let Some(link) = link
                    && !prs
                        .iter()
                        .any(|known| known.number == link.number && known.repo == link.repo)
                {
                    prs.push(link);
                }
            }
        }
        Ok(IssueFetched {
            issue,
            comments,
            prs,
        })
    }

    fn comment(&self, subject: IssueSubject, text: &str) -> Request {
        Request::rest(
            Method::Post,
            format!("{}/notes", base(subject)),
            Some(json!({ "body": text })),
        )
    }

    fn update(&self, subject: IssueSubject, changes: &Changes) -> Result<Request> {
        let mut body = json!({});
        if let Some(title) = &changes.title {
            body["title"] = json!(title);
        }
        if let Some(text) = &changes.body {
            body["description"] = json!(text);
        }
        if let Some((state, _)) = &changes.state {
            body["state_event"] = json!(match state {
                Category::Open => "reopen",
                Category::Closed => "close",
            });
        }
        if let Some(labels) = &changes.labels {
            body["labels"] = json!(labels.join(","));
        }
        if let Some(assignees) = &changes.assignees {
            body["assignee_ids"] = json!(self.user_ids(assignees)?);
        }
        Ok(Request::rest(Method::Put, base(subject), Some(body)))
    }

    fn create(&self, repo: &str, new: &NewIssue) -> Result<Request> {
        let mut body = json!({
            "title": new.title,
            "description": new.body,
            "labels": new.labels.join(","),
        });
        if !new.assignees.is_empty() {
            body["assignee_ids"] = json!(self.user_ids(new.assignees)?);
        }
        Ok(Request::rest(
            Method::Post,
            format!("{}/issues", project(repo)),
            Some(body),
        ))
    }

    fn created(&self, response: &Value) -> Result<IssueData> {
        parse_issue(response)
    }

    fn issue_comment_created(&self, response: &Value) -> Option<(String, Option<String>)> {
        Some((forge::id(response, "/id")?, None))
    }
}

impl Gitlab {
    /// The ids of `logins`; `[0]` for none, which GitLab reads as "unassign everyone".
    fn user_ids(&self, logins: &[String]) -> Result<Vec<u64>> {
        if logins.is_empty() {
            return Ok(vec![0]);
        }
        logins
            .iter()
            .map(|login| {
                let path = format!("users?username={}", forge::encode(login));
                let users = self.transport().json(&Request::get(path))?;
                users
                    .as_array()
                    .and_then(|users| users.first())
                    .and_then(|user| user["id"].as_u64())
                    .ok_or_else(|| Error(format!("GitLab has no user {login}")))
            })
            .collect()
    }
}

fn parse_issue(item: &Value) -> Result<IssueData> {
    let what = "GitLab issue";
    let closed = item["state"] == "closed";
    Ok(IssueData {
        number: item["iid"]
            .as_u64()
            .ok_or_else(|| Error("GitLab issue has no iid".into()))?,
        id: field(item, "/id", what)?,
        url: field(item, "/web_url", what)?,
        title: forge::text(item, "/title").unwrap_or_default(),
        body: forge::text(item, "/description").unwrap_or_default(),
        state: if closed {
            Category::Closed
        } else {
            Category::Open
        },
        reason: None,
        native_state: forge::text(item, "/state").unwrap_or_default(),
        labels: item["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|label| label.as_str().map(str::to_owned))
            .collect(),
        assignees: names(item, "/assignees", "username"),
        author: forge::text(item, "/author/username"),
        milestone: forge::text(item, "/milestone/title"),
        due_date: forge::text(item, "/due_date"),
        created_at: clock::normalize(&field(item, "/created_at", what)?)?,
        updated_at: clock::normalize(&field(item, "/updated_at", what)?)?,
    })
}

/// A merge request of a `closed_by` or `related_merge_requests` list. Its project is the one
/// before `!` in its full reference, else `fallback`.
fn parse_mr(item: &Value, fallback: &str) -> Option<PrLink> {
    let repo = forge::text(item, "/references/full")
        .and_then(|full| full.split_once('!').map(|(repo, _)| repo.to_owned()))
        .unwrap_or_else(|| fallback.to_owned());
    Some(PrLink {
        forge: Forge::Gitlab,
        repo,
        number: item["iid"].as_u64()?,
        url: forge::text(item, "/web_url"),
        branch: forge::text(item, "/source_branch"),
        state: forge::text(item, "/state"),
        source: LinkSource::Remote,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::transport::{Response, Transport, TransportError};

    struct Users;

    impl Transport for Users {
        fn send(&self, request: &Request) -> std::result::Result<Response, TransportError> {
            let body = match &request.endpoint {
                crate::forge::service::Endpoint::Rest(path) if path.ends_with("username=ann") => {
                    json!([{"id": 11}])
                }
                _ => json!([]),
            };
            Ok(Response {
                status: 200,
                headers: Vec::new(),
                body,
            })
        }

        fn render(&self, _request: &Request) -> Vec<String> {
            Vec::new()
        }
    }

    fn subject() -> IssueSubject<'static> {
        IssueSubject {
            repo: "g/p",
            number: 3,
        }
    }

    #[test]
    fn update_maps_fields_to_gitlab_names_and_looks_up_assignees() {
        let service = Gitlab::new(Box::new(Users));
        let changes = Changes {
            body: Some("B".into()),
            state: Some((Category::Closed, None)),
            labels: Some(vec!["a".into(), "b".into()]),
            assignees: Some(vec!["ann".into()]),
            ..Changes::default()
        };
        let request = service.update(subject(), &changes).unwrap();
        assert_eq!(request.method, Method::Put);
        assert_eq!(
            request.body.unwrap(),
            json!({"description": "B", "state_event": "close", "labels": "a,b", "assignee_ids": [11]})
        );
        let none = Changes {
            assignees: Some(Vec::new()),
            ..Changes::default()
        };
        let body = service.update(subject(), &none).unwrap().body.unwrap();
        assert_eq!(body, json!({"assignee_ids": [0]}));
        let unknown = Changes {
            assignees: Some(vec!["zed".into()]),
            ..Changes::default()
        };
        assert!(service.update(subject(), &unknown).is_err());
    }

    #[test]
    fn a_merge_reference_names_its_project() {
        let mr = json!({"iid": 4, "web_url": "u", "source_branch": "b", "state": "opened",
            "references": {"full": "g/other!4"}});
        let link = parse_mr(&mr, "g/p").unwrap();
        assert_eq!(link.repo, "g/other");
        assert_eq!(link.branch.as_deref(), Some("b"));
        let plain = parse_mr(&json!({"iid": 5}), "g/p").unwrap();
        assert_eq!(plain.repo, "g/p");
    }
}
