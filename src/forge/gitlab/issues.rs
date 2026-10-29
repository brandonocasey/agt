//! GitLab issues over REST. Comments are the user notes of the issue's discussions; the MRs that
//! close or relate to an issue come from `closed_by` and `related_merge_requests`.

use serde_json::{Value, json};

use super::{CAPS, Gitlab, discussions, project_path};
use crate::core::config::Scope;
use crate::core::error::{Error, Result};
use crate::forge::issue;
use crate::forge::issue::{
    Changes, IssueCaps, IssueData, IssueFetched, IssuePointers, ListQuery, NewIssue, merge_listing,
    names, strings,
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

fn base(subject: IssueSubject) -> String {
    format!("{}/issues/{}", project_path(subject.repo), subject.number)
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
                project_path(query.repo),
                forge::encode(query.since)
            );
            for item in self
                .transport()
                .items(&Request::get(path), &CAPS.pagination)?
            {
                found.push(parse_issue(&item)?);
            }
        }
        Ok(merge_listing(found))
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
                Err(error) if error.status == Some(404) => Vec::new(),
                Err(error) => return Err(error.into()),
            };
            for item in &items {
                if let Some(link) = parse_mr(item, subject.repo) {
                    link.push_unique(&mut prs);
                }
            }
        }
        Ok(IssueFetched {
            issue,
            comments,
            prs,
        })
    }

    fn hierarchy(&self, subject: IssueSubject) -> Result<crate::forge::issue::IssueHierarchy> {
        self.fetch_hierarchy(subject)
    }

    fn set_parent(
        &self,
        subject: IssueSubject,
        parent: Option<&str>,
        previous: Option<&str>,
    ) -> Result<Request> {
        self.parent_request(subject, parent, previous)
    }

    fn parent_updated(&self, response: &Value) -> Result<()> {
        let payload = response
            .pointer("/data/workItemUpdate")
            .ok_or_else(|| Error("GitLab returned no workItemUpdate result".into()))?;
        let errors = payload["errors"]
            .as_array()
            .ok_or_else(|| Error("GitLab returned no workItemUpdate errors list".into()))?;
        if !errors.is_empty() {
            return Err(Error(format!(
                "GitLab refused parent update: {}",
                payload["errors"]
            )));
        }
        if payload["workItem"].is_null() {
            return Err(Error("GitLab returned no updated work item".into()));
        }
        Ok(())
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
        if new.task {
            body["issue_type"] = json!("task");
        }
        if !new.assignees.is_empty() {
            body["assignee_ids"] = json!(self.user_ids(new.assignees)?);
        }
        Ok(Request::rest(
            Method::Post,
            format!("{}/issues", project_path(repo)),
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
    let mut issue = issue::parse_issue(
        item,
        "GitLab issue",
        &IssuePointers {
            number: "iid",
            id: "/id",
            url: "/web_url",
            body: "/description",
            author: "/author/username",
        },
    )?;
    issue.labels = strings(item, "/labels");
    issue.assignees = names(item, "/assignees", "username");
    issue.due_date = forge::text(item, "/due_date");
    Ok(issue)
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

    struct MissingRoutes {
        status: Option<u16>,
        message: &'static str,
    }

    impl Transport for MissingRoutes {
        fn send(&self, request: &Request) -> std::result::Result<Response, TransportError> {
            let crate::forge::service::Endpoint::Rest(path) = &request.endpoint else {
                unreachable!()
            };
            if path.contains("closed_by") || path.contains("related_merge_requests") {
                return Err(TransportError {
                    status: self.status,
                    message: self.message.into(),
                });
            }
            let body = if path.contains("discussions") {
                json!([])
            } else {
                json!({"iid":3,"id":30,"web_url":"https://gitlab.test/g/p/-/issues/3", "created_at":"2026-01-01T00:00:00Z", "updated_at":"2026-01-01T00:00:00Z"})
            };
            Ok(Response {
                headers: Vec::new(),
                body,
            })
        }

        fn render(&self, _request: &Request) -> Vec<String> {
            Vec::new()
        }
    }

    #[test]
    fn optional_issue_routes_use_status_instead_of_message() {
        for (status, message, succeeds) in [
            (Some(404), "route unavailable", true),
            (Some(500), "proxy mentioned (HTTP 404)", false),
            (None, "tool mentioned (HTTP 404)", false),
            (Some(429), "rate limited", false),
        ] {
            let service = Gitlab::new(Box::new(MissingRoutes { status, message }));
            assert_eq!(
                service.fetch_issue(subject()).is_ok(),
                succeeds,
                "{status:?}"
            );
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
