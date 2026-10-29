use serde_json::json;

use super::Github;
use crate::core::error::{Error, Result};
use crate::forge::issue::{IssueHierarchy, parent_subject, related_key};
use crate::forge::service::{IssueSubject, Method, Pagination, Request, Service};

impl Github {
    pub(super) fn fetch_hierarchy(&self, subject: IssueSubject) -> Result<IssueHierarchy> {
        let path = format!("repos/{}/issues/{}", subject.repo, subject.number);
        let parent = match self
            .transport()
            .json(&Request::get(format!("{path}/parent")))
        {
            Ok(item) => Some(related_key(&item, "/html_url", "number", "/issues/")?),
            Err(error) if error.status == Some(404) => None,
            Err(error) => return Err(error.into()),
        };
        let children = self
            .transport()
            .items(
                &Request::get(format!("{path}/sub_issues?per_page=100")),
                &Pagination::CliPaginate,
            )?
            .iter()
            .map(|item| related_key(item, "/html_url", "number", "/issues/"))
            .collect::<Result<Vec<_>>>()?;
        Ok(IssueHierarchy {
            parent,
            children,
            task: false,
        })
    }

    pub(super) fn parent_request(
        &self,
        subject: IssueSubject,
        parent: Option<&str>,
        previous: Option<&str>,
    ) -> Result<Request> {
        let removing = parent.is_none();
        let parent = parent
            .or(previous)
            .ok_or_else(|| Error("issue has no parent to remove".into()))?;
        let target = parent_subject(parent, subject.repo)?;
        let child = self.transport().json(&Request::get(format!(
            "repos/{}/issues/{}",
            subject.repo, subject.number
        )))?;
        let id = child["id"]
            .as_u64()
            .ok_or_else(|| Error("GitHub issue has no numeric id".into()))?;
        let mut body = json!({"sub_issue_id": id});
        let method = if removing {
            Method::Delete
        } else {
            Method::Post
        };
        if method == Method::Post {
            body["replace_parent"] = json!(true);
        }
        Ok(Request::rest(
            method,
            format!("repos/{}/issues/{}/sub_issues", target.repo, target.number),
            Some(body),
        ))
    }
}
