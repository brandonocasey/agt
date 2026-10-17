//! Test support for a service's contract tests: replay a service's recorded host responses
//! (`test/fixtures/<service>/`), and hold what the service makes of them, the requests it builds
//! and its capabilities equal to golden files next to those fixtures. A new service adds its
//! fixtures and one `#[cfg(test)]` module that calls these.
//!
//! Set `UPDATE_GOLDEN=1` to rewrite the golden files from the current output.

use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::forge::Fetched;
use crate::model::RemoteKind;
use crate::service::{ApproveCap, Capabilities, Inline, Pagination, ReplyCap, Request};
use crate::transport::{Response, Transport, TransportError};

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test/fixtures")
        .join(name)
}

/// A response of the host, from a fixture (JSON, or the pages of a paginated list) whose `{{NAME}}` placeholders are replaced.
pub fn fixture(name: &str, vars: &[(&str, &str)]) -> Value {
    let mut text = fs::read_to_string(fixture_path(name)).unwrap();
    for (name, value) in vars {
        text = text.replace(&format!("{{{{{name}}}}}"), value);
    }
    assert!(!text.contains("{{"), "unfilled placeholder in {name}");
    // `glab api --paginate` prints each page as its own array; the pages are one list.
    let mut values = serde_json::Deserializer::from_str(&text)
        .into_iter::<Value>()
        .map(Result::unwrap);
    let first = values.next().unwrap();
    let Value::Array(mut items) = first else {
        return first;
    };
    for page in values {
        items.extend(page.as_array().unwrap().iter().cloned());
    }
    Value::Array(items)
}

/// Holds `actual` equal to the golden file `name`.
pub fn golden(name: &str, actual: &Value) {
    let path = fixture_path(name);
    let text = format!("{}\n", serde_json::to_string_pretty(actual).unwrap());
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::write(&path, &text).unwrap();
        return;
    }
    let expected: Value = serde_json::from_str(
        &fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("{name} is missing; run with UPDATE_GOLDEN=1")),
    )
    .unwrap();
    assert_eq!(
        *actual, expected,
        "{name} differs; run with UPDATE_GOLDEN=1"
    );
}

/// What a service made of the host's responses, as plain JSON.
pub fn dump(fetched: &Fetched) -> Value {
    let pr = &fetched.pr;
    let threads: Vec<Value> = fetched
        .threads
        .iter()
        .map(|thread| {
            let comments: Vec<Value> = thread
                .comments
                .iter()
                .map(|comment| {
                    json!({
                        "id": comment.id,
                        "url": comment.url,
                        "body": comment.body,
                        "created_at": comment.created_at,
                        "login": comment.login,
                        "editor": comment.editor,
                        "review_id": comment.review_id,
                    })
                })
                .collect();
            json!({
                "remote": thread.info,
                "resolved": thread.resolved,
                "position_head": thread.position_head,
                "comments": comments,
            })
        })
        .collect();
    let drafts: Vec<Value> = fetched
        .drafts
        .iter()
        .map(|draft| json!({ "id": draft.id, "body": draft.body }))
        .collect();
    json!({
        "pr": {
            "repo": pr.repo,
            "url": pr.url,
            "state": pr.state,
            "closed_at": pr.closed_at,
            "head_branch": pr.head_branch,
            "head_sha": pr.head_sha,
            "base_sha": pr.base_sha,
            "start_sha": pr.start_sha,
            "author": pr.author,
        },
        "threads": threads,
        "drafts": drafts,
    })
}

/// The requests of a service as the CLI would be given them, with their JSON bodies.
pub fn requests(render: fn(&Request) -> Vec<String>, list: &[(&str, Request)]) -> Value {
    let mut out = serde_json::Map::new();
    for (name, request) in list {
        out.insert(
            (*name).to_owned(),
            json!({ "args": render(request), "body": request.body }),
        );
    }
    Value::Object(out)
}

/// The capabilities as `test/fixtures/<service>/capabilities.json` states them, which the
/// integration scenarios read to know what to expect of the service.
pub fn capabilities(caps: &Capabilities) -> Value {
    let (approve, self_allowed) = match caps.approve {
        ApproveCap::None => ("none", Value::Null),
        ApproveCap::Review { self_allowed } => ("review", json!(self_allowed)),
        ApproveCap::Call => ("call", Value::Null),
    };
    let kind = |kind: RemoteKind| match kind {
        RemoteKind::ReviewThread => "review_thread",
        RemoteKind::Review => "review",
        RemoteKind::General => "general",
        RemoteKind::Discussion => "discussion",
    };
    json!({
        "pagination": match caps.pagination {
            Pagination::GraphqlCursor => "graphql_cursor",
            Pagination::CliPaginate => "cli_paginate",
            Pagination::LinkHeader => "link_header",
            Pagination::NextUrlInBody(_) => "next_url_in_body",
            Pagination::ContinuationHeader(_) => "continuation_header",
        },
        "inline": match caps.inline {
            Inline::ReviewBatch => "review_batch",
            Inline::DraftThenPublish => "draft_then_publish",
        },
        "ranges": caps.ranges,
        "reply": match caps.reply {
            ReplyCap::All => "all",
            ReplyCap::ReviewThreads => "review_threads",
            ReplyCap::None => "none",
        },
        "resolve": caps.resolve,
        "unresolve": caps.unresolve,
        "general_kind": kind(caps.general_kind),
        "general_resolvable": caps.general_resolvable,
        "approve": approve,
        "approve_self_allowed": self_allowed,
        "edits_attributed": caps.edits_attributed,
        "notes_become_discussions": caps.notes_become_discussions,
    })
}

/// A transport that answers a request by the arguments the service's CLI would be given,
/// keyed as the integration tests' fake `gh`/`glab` keys them: a GraphQL `query=query Name(...)`
/// argument stands for `query=Name`.
pub struct Replay {
    render: fn(&Request) -> Vec<String>,
    answers: Vec<(String, Value)>,
}

impl Replay {
    pub fn new(render: fn(&Request) -> Vec<String>) -> Self {
        Self {
            render,
            answers: Vec::new(),
        }
    }

    pub fn answer(mut self, args: &[&str], body: Value) -> Self {
        self.answers.push((args.join(" "), body));
        self
    }

    fn key(&self, request: &Request) -> String {
        (self.render)(request)
            .into_iter()
            .map(|arg| {
                let Some(query) = arg.strip_prefix("query=") else {
                    return arg;
                };
                let name = query
                    .strip_prefix("query ")
                    .or_else(|| query.strip_prefix("mutation "))
                    .and_then(|rest| rest.split('(').next());
                name.map_or(arg.clone(), |name| format!("query={name}"))
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl Transport for Replay {
    fn send(&self, request: &Request) -> Result<Response, TransportError> {
        let key = self.key(request);
        let body = self
            .answers
            .iter()
            .find(|(args, _)| *args == key)
            .map(|(_, body)| body.clone())
            .ok_or_else(|| TransportError::new(format!("unexpected request: {key}")))?;
        Ok(Response {
            status: 200,
            headers: Vec::new(),
            body,
        })
    }

    fn render(&self, request: &Request) -> Vec<String> {
        (self.render)(request)
    }
}
