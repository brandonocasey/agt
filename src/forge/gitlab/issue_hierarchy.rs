use std::collections::HashSet;

use serde_json::Value;

use super::Gitlab;
use crate::core::error::{Error, Result, fail};
use crate::forge::issue::{IssueHierarchy, parent_subject};
use crate::forge::service::{IssueSubject, Request, Service, Var};

const QUERY: &str = "query IssueHierarchy($path:ID!,$iid:String!,$after:String){project(fullPath:$path){workItems(first:1,iid:$iid){nodes{id iid webUrl workItemType{name} widgets{type ... on WorkItemWidgetHierarchy{parent{id iid project{fullPath}} children(first:100,after:$after){nodes{id iid project{fullPath}} pageInfo{hasNextPage endCursor}}}}}}}}";

impl Gitlab {
    fn hierarchy_page(&self, subject: IssueSubject, cursor: Option<&str>) -> Result<Value> {
        let mut vars = vec![
            Var::text("path", subject.repo),
            Var::text("iid", subject.number.to_string()),
        ];
        if let Some(cursor) = cursor {
            vars.push(Var::text("after", cursor));
        }
        vars.push(Var::text("query", QUERY));
        let response = self.transport().json(&Request::graphql(vars))?;
        response
            .pointer("/data/project/workItems/nodes/0")
            .cloned()
            .ok_or_else(|| {
                Error(format!(
                    "GitLab returned no work item {}#{}",
                    subject.repo, subject.number
                ))
            })
    }

    pub(super) fn fetch_hierarchy(&self, subject: IssueSubject) -> Result<IssueHierarchy> {
        let mut result = IssueHierarchy::default();
        let mut cursor = None;
        let mut seen = HashSet::new();
        loop {
            let item = self.hierarchy_page(subject, cursor.as_deref())?;
            result.task = item
                .pointer("/workItemType/name")
                .and_then(Value::as_str)
                .ok_or_else(|| Error("GitLab work item has no type".into()))?
                == "Task";
            let widget = item["widgets"]
                .as_array()
                .and_then(|widgets| widgets.iter().find(|widget| widget["type"] == "HIERARCHY"))
                .ok_or_else(|| Error("GitLab work item has no hierarchy widget".into()))?;
            result.parent = if !result.task || widget["parent"].is_null() {
                None
            } else {
                Some(work_item_key(&widget["parent"])?)
            };
            let children = widget
                .pointer("/children/nodes")
                .and_then(Value::as_array)
                .ok_or_else(|| Error("GitLab hierarchy has no children list".into()))?;
            for child in children {
                result.children.push(work_item_key(child)?);
            }
            match widget
                .pointer("/children/pageInfo/hasNextPage")
                .and_then(Value::as_bool)
            {
                Some(false) => return Ok(result),
                Some(true) => {
                    let next = widget
                        .pointer("/children/pageInfo/endCursor")
                        .and_then(Value::as_str)
                        .filter(|next| !next.is_empty())
                        .ok_or_else(|| Error("GitLab hierarchy has no next cursor".into()))?;
                    if !seen.insert(next.to_owned()) {
                        return fail("GitLab hierarchy repeated a page cursor");
                    }
                    cursor = Some(next.to_owned());
                }
                None => return fail("GitLab hierarchy has no pagination information"),
            }
        }
    }

    pub(super) fn parent_request(
        &self,
        subject: IssueSubject,
        parent: Option<&str>,
        _previous: Option<&str>,
    ) -> Result<Request> {
        let child = self.hierarchy_page(subject, None)?;
        if child.pointer("/workItemType/name").and_then(Value::as_str) != Some("Task") {
            return fail(
                "GitLab children must be Tasks; convert the Issue to a Task on GitLab, then run `agt issue sync`",
            );
        }
        let id = child["id"]
            .as_str()
            .ok_or_else(|| Error("GitLab work item has no id".into()))?;
        let mut vars = vec![Var::text("child", id)];
        if let Some(parent) = parent {
            let parent = self.hierarchy_page(parent_subject(parent, subject.repo)?, None)?;
            if parent.pointer("/workItemType/name").and_then(Value::as_str) != Some("Issue") {
                return fail("a GitLab Task needs an Issue as its parent");
            }
            vars.push(Var::text(
                "parent",
                parent["id"]
                    .as_str()
                    .ok_or_else(|| Error("GitLab parent has no id".into()))?,
            ));
        } else {
            vars.push(Var::typed("parent", "null"));
        }
        vars.push(Var::text("query", "mutation IssueParent($child:WorkItemID!,$parent:WorkItemID){workItemUpdate(input:{id:$child,hierarchyWidget:{parentId:$parent}}){errors workItem{id iid}}}"));
        Ok(Request::graphql(vars))
    }
}

fn work_item_key(item: &Value) -> Result<String> {
    let repo = item
        .pointer("/project/fullPath")
        .and_then(Value::as_str)
        .filter(|repo| !repo.is_empty())
        .ok_or_else(|| Error("GitLab related work item has no project".into()))?;
    let iid = item["iid"]
        .as_str()
        .and_then(|iid| iid.parse::<u64>().ok())
        .or_else(|| item["iid"].as_u64())
        .filter(|iid| *iid > 0)
        .ok_or_else(|| Error("GitLab related work item has no iid".into()))?;
    Ok(format!("{}#{iid}", repo.to_ascii_lowercase()))
}
