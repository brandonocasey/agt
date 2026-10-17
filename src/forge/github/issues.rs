//! GitHub issues over REST, with the PRs that close or mention an issue read through GraphQL.

use serde_json::{Value, json};

use super::Github;
use crate::core::clock;
use crate::core::config::Scope;
use crate::core::error::{Error, Result};
use crate::forge::issue::{
    Changes, IssueCaps, IssueData, IssueFetched, ListQuery, NewIssue, field, names,
};
use crate::forge::service::{
    IssueService, IssueSubject, Method, Pagination, Request, Service, Var,
};
use crate::forge::{self, CommentData};
use crate::store::issues::{Category, LinkSource, PrLink};
use crate::store::model::Forge;

const PR_FIELDS: &str = "number url state headRefName repository{nameWithOwner}";

pub(super) const ISSUE_CAPS: IssueCaps = IssueCaps {
    multi_assignee: true,
    close_reasons: true,
};

/// PRs that close the issue, and PRs that mention it from the timeline (a cross reference whose
/// source is an issue yields an empty node).
const LINKS_QUERY: &str = "query IssueLinks($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){issue(number:$number){closedByPullRequestsReferences(first:50,includeClosedPrs:true){nodes{PR_FIELDS}} timelineItems(first:100,itemTypes:[CROSS_REFERENCED_EVENT]){nodes{... on CrossReferencedEvent{source{... on PullRequest{PR_FIELDS}}}}}}}}";

fn base(subject: IssueSubject) -> String {
    format!("repos/{}/issues/{}", subject.repo, subject.number)
}

impl IssueService for Github {
    fn list_changed(&self, query: &ListQuery) -> Result<Vec<IssueData>> {
        let filters: Vec<String> = match query.scope {
            Scope::All => vec![String::new()],
            Scope::Assigned => vec![format!("&assignee={}", forge::encode(query.login))],
            Scope::Involved => ["creator", "assignee", "mentioned"]
                .iter()
                .map(|filter| format!("&{filter}={}", forge::encode(query.login)))
                .collect(),
        };
        let mut found: Vec<IssueData> = Vec::new();
        for filter in filters {
            let path = format!(
                "repos/{}/issues?state=all&since={}&sort=updated&direction=asc&per_page=100{filter}",
                query.repo,
                forge::encode(query.since)
            );
            for item in self
                .transport()
                .items(&Request::get(path), &Pagination::CliPaginate)?
            {
                // The issues endpoint lists pull requests too.
                if item.get("pull_request").is_some() {
                    continue;
                }
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
        let issue = transport.json(&Request::get(base(subject)))?;
        if issue.get("pull_request").is_some() {
            return Err(Error(format!(
                "{}#{} is a pull request, not an issue",
                subject.repo, subject.number
            )));
        }
        let items = transport.items(
            &Request::get(format!("{}/comments?per_page=100", base(subject))),
            &Pagination::CliPaginate,
        )?;
        let comments = items
            .iter()
            .map(parse_comment)
            .collect::<Result<Vec<_>>>()?;
        let (owner, name) = subject.repo.split_once('/').unwrap_or((subject.repo, ""));
        let links = transport.json(&Request::graphql(vec![
            Var::text("owner", owner),
            Var::text("name", name),
            Var::typed("number", subject.number.to_string()),
            Var::text("query", LINKS_QUERY.replace("PR_FIELDS", PR_FIELDS)),
        ]))?;
        Ok(IssueFetched {
            issue: parse_issue(&issue)?,
            comments,
            prs: parse_links(&links),
        })
    }

    fn comment(&self, subject: IssueSubject, text: &str) -> Request {
        Request::rest(
            Method::Post,
            format!("{}/comments", base(subject)),
            Some(json!({ "body": text })),
        )
    }

    fn update(&self, subject: IssueSubject, changes: &Changes) -> Result<Request> {
        let mut body = json!({});
        if let Some(title) = &changes.title {
            body["title"] = json!(title);
        }
        if let Some(text) = &changes.body {
            body["body"] = json!(text);
        }
        if let Some((state, reason)) = &changes.state {
            match state {
                Category::Open => {
                    body["state"] = json!("open");
                    body["state_reason"] = json!("reopened");
                }
                Category::Closed => {
                    body["state"] = json!("closed");
                    body["state_reason"] = json!(reason.as_deref().unwrap_or("completed"));
                }
            }
        }
        if let Some(labels) = &changes.labels {
            body["labels"] = json!(labels);
        }
        if let Some(assignees) = &changes.assignees {
            body["assignees"] = json!(assignees);
        }
        Ok(Request::rest(Method::Patch, base(subject), Some(body)))
    }

    fn create(&self, repo: &str, new: &NewIssue) -> Result<Request> {
        Ok(Request::rest(
            Method::Post,
            format!("repos/{repo}/issues"),
            Some(json!({
                "title": new.title,
                "body": new.body,
                "labels": new.labels,
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

fn parse_issue(item: &Value) -> Result<IssueData> {
    let what = "GitHub issue";
    let closed = item["state"] == "closed";
    Ok(IssueData {
        number: item["number"]
            .as_u64()
            .ok_or_else(|| Error("GitHub issue has no number".into()))?,
        id: field(item, "/node_id", what)?,
        url: field(item, "/html_url", what)?,
        title: forge::text(item, "/title").unwrap_or_default(),
        body: forge::text(item, "/body").unwrap_or_default(),
        state: if closed {
            Category::Closed
        } else {
            Category::Open
        },
        reason: forge::text(item, "/state_reason").filter(|_| closed),
        native_state: forge::text(item, "/state").unwrap_or_default(),
        labels: names(item, "/labels", "name"),
        assignees: names(item, "/assignees", "login"),
        author: forge::text(item, "/user/login"),
        milestone: forge::text(item, "/milestone/title"),
        due_date: None,
        created_at: clock::normalize(&field(item, "/created_at", what)?)?,
        updated_at: clock::normalize(&field(item, "/updated_at", what)?)?,
    })
}

fn parse_comment(item: &Value) -> Result<CommentData> {
    let what = "GitHub issue comment";
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

/// The PRs of an `IssueLinks` response, once each.
fn parse_links(response: &Value) -> Vec<PrLink> {
    let issue = &response["data"]["repository"]["issue"];
    let closing = issue
        .pointer("/closedByPullRequestsReferences/nodes")
        .and_then(Value::as_array);
    let mentioning = issue
        .pointer("/timelineItems/nodes")
        .and_then(Value::as_array)
        .map(|nodes| nodes.iter().map(|node| &node["source"]));
    let mut links: Vec<PrLink> = Vec::new();
    let nodes = closing
        .into_iter()
        .flatten()
        .chain(mentioning.into_iter().flatten());
    for node in nodes {
        let (Some(number), Some(repo)) = (
            node["number"].as_u64(),
            forge::text(node, "/repository/nameWithOwner"),
        ) else {
            continue;
        };
        if links
            .iter()
            .any(|link| link.number == number && link.repo == repo)
        {
            continue;
        }
        links.push(PrLink {
            forge: Forge::Github,
            repo,
            number,
            url: forge::text(node, "/url"),
            branch: forge::text(node, "/headRefName"),
            state: forge::text(node, "/state").map(|state| state.to_ascii_lowercase()),
            source: LinkSource::Remote,
        });
    }
    links
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::transport::{Response, Transport, TransportError};
    use std::cell::RefCell;

    struct Fake {
        calls: RefCell<Vec<Request>>,
        body: Value,
    }

    impl Transport for Fake {
        fn send(&self, request: &Request) -> std::result::Result<Response, TransportError> {
            self.calls.borrow_mut().push(request.clone());
            Ok(Response {
                status: 200,
                headers: Vec::new(),
                body: self.body.clone(),
            })
        }

        fn render(&self, _request: &Request) -> Vec<String> {
            Vec::new()
        }
    }

    fn github(body: Value) -> Github {
        Github::new(Box::new(Fake {
            calls: RefCell::new(Vec::new()),
            body,
        }))
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
            state: Some((Category::Closed, Some("not_planned".into()))),
            labels: Some(vec!["bug".into()]),
            ..Changes::default()
        };
        let request = github(json!(null)).update(subject(), &changes).unwrap();
        assert_eq!(request.method, Method::Patch);
        assert_eq!(
            request.body.unwrap(),
            json!({"title": "T", "state": "closed", "state_reason": "not_planned", "labels": ["bug"]})
        );
        let reopen = Changes {
            state: Some((Category::Open, None)),
            ..Changes::default()
        };
        let body = github(json!(null))
            .update(subject(), &reopen)
            .unwrap()
            .body
            .unwrap();
        assert_eq!(body, json!({"state": "open", "state_reason": "reopened"}));
    }

    fn raw(number: u64, pull: bool) -> Value {
        let mut item = json!({
            "number": number, "node_id": format!("I_{number}"),
            "html_url": format!("https://github.com/o/r/issues/{number}"),
            "title": "t", "body": null, "state": "closed", "state_reason": "completed",
            "labels": [{"name": "bug"}], "assignees": [{"login": "a"}], "user": {"login": "u"},
            "created_at": "2026-10-01T00:00:00Z", "updated_at": "2026-10-02T00:00:00Z"
        });
        if pull {
            item["pull_request"] = json!({});
        }
        item
    }

    #[test]
    fn a_listing_skips_pull_requests_and_repeats_across_involved_filters() {
        let service = github(json!([raw(2, false), raw(3, true)]));
        let query = ListQuery {
            repo: "o/r",
            scope: Scope::Involved,
            since: "2026-10-01T00:00:00Z",
            login: "me",
        };
        let listed = service.list_changed(&query).unwrap();
        let numbers: Vec<u64> = listed.iter().map(|issue| issue.number).collect();
        assert_eq!(numbers, [2]);
        assert_eq!(listed[0].body, "");
        assert_eq!(listed[0].reason.as_deref(), Some("completed"));
        assert_eq!(listed[0].labels, ["bug"]);
    }

    #[test]
    fn the_links_query_is_well_formed() {
        let query = LINKS_QUERY.replace("PR_FIELDS", PR_FIELDS);
        assert!(
            !query.contains("{{") && !query.contains("PR_FIELDS"),
            "{query}"
        );
        assert_eq!(
            query.matches('{').count(),
            query.matches('}').count(),
            "{query}"
        );
        assert!(query.contains("nodes{number url state headRefName repository{nameWithOwner}}"));
    }

    #[test]
    fn links_come_from_closing_and_mentioning_prs_once() {
        let response = json!({"data": {"repository": {"issue": {
            "closedByPullRequestsReferences": {"nodes": [
                {"number": 5, "url": "u5", "state": "OPEN", "headRefName": "fix", "repository": {"nameWithOwner": "o/r"}}
            ]},
            "timelineItems": {"nodes": [
                {"source": {"number": 5, "url": "u5", "state": "OPEN", "headRefName": "fix", "repository": {"nameWithOwner": "o/r"}}},
                {"source": {}},
                {"source": {"number": 9, "url": "u9", "state": "MERGED", "headRefName": "b", "repository": {"nameWithOwner": "o/other"}}}
            ]}
        }}}});
        let links = parse_links(&response);
        let numbers: Vec<(String, u64)> = links
            .iter()
            .map(|link| (link.repo.clone(), link.number))
            .collect();
        assert_eq!(numbers, [("o/r".into(), 5), ("o/other".into(), 9)]);
        assert_eq!(links[1].state.as_deref(), Some("merged"));
    }
}
