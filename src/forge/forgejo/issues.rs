//! Forgejo issues over REST. The host has no parent/child links between issues and no list of the
//! PRs that close one, so neither is read or written.

use serde_json::{Value, json};

use super::{CAPS, Forgejo};
use crate::core::clock;
use crate::core::config::Scope;
use crate::core::error::{Error, Result};
use crate::forge::issue;
use crate::forge::issue::{
    Changes, IssueCaps, IssueData, IssueFetched, IssueHierarchy, IssuePointers, ListQuery,
    NewIssue, field, merge_listing, names,
};
use crate::forge::service::{IssueService, IssueSubject, Method, Request, Service};
use crate::forge::{self, CommentData};
use crate::store::issues::Category;

pub(super) const ISSUE_CAPS: IssueCaps = IssueCaps {
    multi_assignee: true,
    close_reasons: false,
};

fn base(subject: IssueSubject) -> String {
    format!("repos/{}/issues/{}", subject.repo, subject.number)
}

impl IssueService for Forgejo {
    fn list_changed(&self, query: &ListQuery) -> Result<Vec<IssueData>> {
        let filters: Vec<String> = match query.scope {
            Scope::All => vec![String::new()],
            Scope::Assigned => vec![format!("&assigned_by={}", forge::encode(query.login))],
            Scope::Involved => ["created_by", "assigned_by", "mentioned_by"]
                .iter()
                .map(|filter| format!("&{filter}={}", forge::encode(query.login)))
                .collect(),
        };
        let mut found: Vec<IssueData> = Vec::new();
        for filter in filters {
            let path = format!(
                "repos/{}/issues?state=all&type=issues&since={}{filter}",
                query.repo,
                forge::encode(query.since)
            );
            for item in self
                .transport()
                .items(&Request::get(path), &CAPS.pagination)?
            {
                // A host that ignores `type` lists pull requests too.
                if item.get("pull_request").is_some_and(|pull| !pull.is_null()) {
                    continue;
                }
                found.push(parse_issue(&item)?);
            }
        }
        Ok(merge_listing(found))
    }

    fn fetch_issue(&self, subject: IssueSubject) -> Result<IssueFetched> {
        let transport = self.transport();
        let issue = transport.json(&Request::get(base(subject)))?;
        if issue
            .get("pull_request")
            .is_some_and(|pull| !pull.is_null())
        {
            return Err(Error(format!(
                "{}#{} is a pull request, not an issue",
                subject.repo, subject.number
            )));
        }
        let items = transport.items(
            &Request::get(format!("{}/comments", base(subject))),
            &CAPS.pagination,
        )?;
        let comments = items
            .iter()
            .map(parse_comment)
            .collect::<Result<Vec<_>>>()?;
        Ok(IssueFetched {
            issue: parse_issue(&issue)?,
            comments,
            prs: Vec::new(),
        })
    }

    // Every issue is a root with no children; only a missing issue is `None`.
    fn hierarchy(&self, subject: IssueSubject) -> Result<Option<IssueHierarchy>> {
        match self.transport().json(&Request::get(base(subject))) {
            Ok(_) => Ok(Some(IssueHierarchy::default())),
            Err(error) if matches!(error.status, Some(404 | 410)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn set_parent(
        &self,
        _subject: IssueSubject,
        _parent: Option<&str>,
        _previous: Option<&str>,
    ) -> Result<Request> {
        Err(Error(
            "Forgejo has no parent for an issue; the parent stays local".into(),
        ))
    }

    fn parent_updated(&self, _response: &Value) -> Result<()> {
        Ok(())
    }

    fn comment(&self, subject: IssueSubject, text: &str) -> Request {
        Request::rest(
            Method::Post,
            format!("{}/comments", base(subject)),
            Some(json!({ "body": text })),
        )
    }

    // Labels are not a field of an issue edit: they are set with a request of their own, so a
    // label change cannot go out with another change.
    fn update(&self, subject: IssueSubject, changes: &Changes) -> Result<Request> {
        if let Some(labels) = &changes.labels {
            let with_others = changes.title.is_some()
                || changes.body.is_some()
                || changes.state.is_some()
                || changes.assignees.is_some();
            if with_others {
                return Err(Error(
                    "Forgejo sets labels apart from other fields: push the label change on its own"
                        .into(),
                ));
            }
            return Ok(Request::rest(
                Method::Put,
                format!("{}/labels", base(subject)),
                Some(json!({ "labels": labels })),
            ));
        }
        let mut body = json!({});
        if let Some(title) = &changes.title {
            body["title"] = json!(title);
        }
        if let Some(text) = &changes.body {
            body["body"] = json!(text);
        }
        if let Some((state, _)) = &changes.state {
            body["state"] = json!(match state {
                Category::Open => "open",
                Category::Closed => "closed",
            });
        }
        if let Some(assignees) = &changes.assignees {
            body["assignees"] = json!(assignees);
        }
        Ok(Request::rest(Method::Patch, base(subject), Some(body)))
    }

    // Labels of a new issue are ids, so the names are looked up.
    fn create(&self, repo: &str, new: &NewIssue) -> Result<Request> {
        let ids = self.label_ids(repo, new.labels)?;
        Ok(Request::rest(
            Method::Post,
            format!("repos/{repo}/issues"),
            Some(json!({
                "title": new.title,
                "body": new.body,
                "labels": ids,
                "assignees": new.assignees,
            })),
        ))
    }

    fn created(&self, response: &Value) -> Result<IssueData> {
        parse_issue(response)
    }

    fn issue_comment_created(&self, response: &Value) -> Option<(String, Option<String>)> {
        Some((
            forge::id(response, "/id")?,
            forge::text(response, "/html_url"),
        ))
    }
}

impl Forgejo {
    /// The ids of the labels named `wanted` in `repo`; an unknown name is an error.
    fn label_ids(&self, repo: &str, wanted: &[String]) -> Result<Vec<u64>> {
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let known = self.transport().items(
            &Request::get(format!("repos/{repo}/labels")),
            &CAPS.pagination,
        )?;
        wanted
            .iter()
            .map(|name| {
                known
                    .iter()
                    .find(|label| label["name"] == name.as_str())
                    .and_then(|label| label["id"].as_u64())
                    .ok_or_else(|| Error(format!("{repo} has no label {name}")))
            })
            .collect()
    }
}

fn parse_issue(item: &Value) -> Result<IssueData> {
    let mut issue = issue::parse_issue(
        item,
        "Forgejo issue",
        &IssuePointers {
            number: "number",
            id: "/id",
            url: "/html_url",
            body: "/body",
            author: "/user/login",
        },
    )?;
    issue.labels = names(item, "/labels", "name");
    issue.assignees = names(item, "/assignees", "login");
    Ok(issue)
}

fn parse_comment(item: &Value) -> Result<CommentData> {
    let what = "Forgejo issue comment";
    Ok(CommentData {
        id: field(item, "/id", what)?,
        url: forge::text(item, "/html_url"),
        body: forge::text(item, "/body").unwrap_or_default(),
        created_at: clock::normalize(&field(item, "/created_at", what)?)?,
        login: forge::text(item, "/user/login"),
        editor: None,
        review_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::transport::{Response, Transport, TransportError};

    struct Labels;

    impl Transport for Labels {
        fn send(&self, request: &Request) -> std::result::Result<Response, TransportError> {
            assert_eq!(request.method, Method::Get);
            Ok(Response {
                headers: Vec::new(),
                body: json!([{"id": 4, "name": "bug"}, {"id": 9, "name": "docs"}]),
            })
        }

        fn render(&self, _request: &Request) -> Vec<String> {
            Vec::new()
        }
    }

    fn forgejo() -> Forgejo {
        Forgejo::new(Box::new(Labels))
    }

    fn subject() -> IssueSubject<'static> {
        IssueSubject {
            repo: "o/r",
            number: 7,
        }
    }

    #[test]
    fn update_sends_only_the_changed_fields() {
        let changes = Changes {
            title: Some("T".into()),
            state: Some((Category::Closed, None)),
            assignees: Some(vec!["a".into()]),
            ..Changes::default()
        };
        let request = forgejo().update(subject(), &changes).unwrap();
        assert_eq!(request.method, Method::Patch);
        assert_eq!(
            request.body.unwrap(),
            json!({"title": "T", "state": "closed", "assignees": ["a"]})
        );
    }

    #[test]
    fn labels_go_out_alone_and_with_other_fields_are_refused() {
        let labels = Changes {
            labels: Some(vec!["bug".into()]),
            ..Changes::default()
        };
        let request = forgejo().update(subject(), &labels).unwrap();
        assert_eq!(request.method, Method::Put);
        assert_eq!(request.body.unwrap(), json!({"labels": ["bug"]}));
        let both = Changes {
            title: Some("T".into()),
            ..labels
        };
        assert!(forgejo().update(subject(), &both).is_err());
    }

    fn new_issue(labels: &[String]) -> NewIssue<'_> {
        NewIssue {
            task: false,
            title: "t",
            body: "b",
            labels,
            assignees: &[],
        }
    }

    #[test]
    fn a_new_issue_names_labels_by_id_and_refuses_an_unknown_one() {
        let request = forgejo()
            .create("o/r", &new_issue(&["docs".into(), "bug".into()]))
            .unwrap();
        assert_eq!(request.body.unwrap()["labels"], json!([9, 4]));
        assert!(
            forgejo()
                .create("o/r", &new_issue(&["nope".into()]))
                .is_err()
        );
    }

    #[test]
    fn an_issue_parses_with_its_labels_and_assignees() {
        let issue = parse_issue(&json!({
            "number": 3, "id": 77, "html_url": "https://h/o/r/issues/3", "title": "t",
            "body": null, "state": "closed", "labels": [{"name": "bug"}],
            "assignees": [{"login": "a"}], "user": {"login": "u"},
            "created_at": "2026-10-01T00:00:00Z", "updated_at": "2026-10-02T00:00:00Z"
        }))
        .unwrap();
        assert_eq!((issue.number, issue.id.as_str()), (3, "77"));
        assert_eq!(issue.state, Category::Closed);
        assert_eq!(issue.labels, ["bug"]);
        assert_eq!(issue.assignees, ["a"]);
        assert_eq!(issue.body, "");
    }
}
