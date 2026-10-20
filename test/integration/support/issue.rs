//! `agt issue` in the isolated environment, and a fake GitHub and GitLab that answer an issue
//! tracker's calls. A scenario describes the host's state with [`Spec`]s, comments and PR links;
//! the [`IssueFake`] turns it into the argument lists and response bodies the fake `gh`/`glab`
//! replays, so one scenario runs against both services.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

use super::{Env, args, strs};

/// `AGT_NOW` of every issue command: 2026-10-17T12:00:00Z.
pub const NOW: i64 = 1_792_238_400;
/// The first sync lists from 90 days before [`NOW`].
pub const FIRST_SINCE: &str = "2026-07-19T12%3A00%3A00Z";

impl Env {
    pub fn issue_state(&self) -> PathBuf {
        self.repo.join(".agt").join("issues")
    }

    pub fn issue_exec(&self, args: &[&str], stdin: &str, env: &[(&str, &str)]) -> Output {
        let mut command: Command = self.command(super::BIN, &self.repo);
        command
            .arg("issue")
            .args(args)
            .env("AGT_NOW", NOW.to_string())
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), stdin.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }

    pub fn issue_at(&self, now: i64, args: &[&str]) -> Value {
        let output = self.issue_exec(args, "", &[("AGT_NOW", &now.to_string())]);
        assert!(
            output.status.success(),
            "agt issue {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    pub fn issue(&self, args: &[&str]) -> Value {
        self.issue_at(NOW, args)
    }

    pub fn issue_stdin(&self, args: &[&str], stdin: &str) -> Value {
        let output = self.issue_exec(args, stdin, &[]);
        assert!(
            output.status.success(),
            "agt issue {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// Runs a command that must fail with exit 1 and returns its stderr.
    pub fn issue_fail(&self, args: &[&str]) -> String {
        let output = self.issue_exec(args, "", &[]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "agt issue {args:?} should fail"
        );
        String::from_utf8(output.stderr).unwrap()
    }

    /// Runs a command with the forge tools unreachable, so it can only read the store.
    pub fn issue_offline(&self, args: &[&str]) -> Value {
        let output = self.issue_exec(
            args,
            "",
            &[
                ("AGT_OFFLINE", "1"),
                ("AGT_GH_BIN", "/nonexistent/gh"),
                ("AGT_GLAB_BIN", "/nonexistent/glab"),
            ],
        );
        assert!(
            output.status.success(),
            "offline agt issue {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// Every file of the issue store, as text.
    pub fn issue_files(&self) -> String {
        let mut text = String::new();
        let mut pending = vec![self.issue_state()];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else if let Ok(content) = std::fs::read_to_string(&path) {
                    text.push_str(&content);
                }
            }
        }
        text
    }

    pub fn issue_doc(&self, tracker: &str, name: &str) -> Value {
        let path = self
            .issue_state()
            .join(tracker)
            .join(format!("{name}.json"));
        serde_json::from_str(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
        )
        .unwrap()
    }

    /// Forge calls that are not reads.
    pub fn forge_writes(&self, fake: &dyn IssueFake) -> Vec<String> {
        self.forge_calls()
            .into_iter()
            .filter_map(|call| call.split('\t').nth(1).map(str::to_owned))
            .filter(|args| !fake.is_read(args))
            .collect()
    }
}

/// An issue as a scenario states it.
#[derive(Clone)]
pub struct Spec {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub closed: bool,
    pub reason: Option<&'static str>,
    pub labels: Vec<&'static str>,
    pub assignees: Vec<&'static str>,
    pub author: &'static str,
    pub updated: String,
    /// A pull request, which the issues listing of GitHub also returns.
    pub is_pr: bool,
    pub comments: Vec<Cm>,
    pub prs: Vec<Pr>,
}

#[derive(Clone)]
pub struct Cm {
    pub id: u64,
    pub login: &'static str,
    pub body: String,
}

#[derive(Clone)]
pub struct Pr {
    pub number: u64,
    pub branch: &'static str,
}

impl Spec {
    pub fn new(number: u64, title: &str) -> Self {
        Self {
            number,
            title: title.to_owned(),
            body: format!("Body of {title}"),
            closed: false,
            reason: None,
            labels: Vec::new(),
            assignees: Vec::new(),
            author: "alice",
            updated: "2026-10-10T10:00:00Z".to_owned(),
            is_pr: false,
            comments: Vec::new(),
            prs: Vec::new(),
        }
    }

    pub fn updated(mut self, at: &str) -> Self {
        self.updated = at.to_owned();
        self
    }

    pub fn labels(mut self, labels: &[&'static str]) -> Self {
        self.labels = labels.to_vec();
        self
    }

    pub fn assignees(mut self, assignees: &[&'static str]) -> Self {
        self.assignees = assignees.to_vec();
        self
    }

    pub fn closed(mut self) -> Self {
        self.closed = true;
        self
    }

    pub fn comment(mut self, id: u64, login: &'static str, body: &str) -> Self {
        self.comments.push(Cm {
            id,
            login,
            body: body.to_owned(),
        });
        self
    }

    pub fn pr(mut self, number: u64, branch: &'static str) -> Self {
        self.prs.push(Pr { number, branch });
        self
    }

    pub fn pull_request(mut self) -> Self {
        self.is_pr = true;
        self
    }
}

/// What a service's calls look like for one repository on one host.
pub trait IssueFake {
    fn name(&self) -> &'static str;
    /// The tracker's `owner/repo`.
    fn repo(&self) -> &'static str;
    /// The directory of the tracker in the store.
    fn tracker(&self) -> &'static str;
    fn config(&self, push: bool) -> String;
    fn is_read(&self, args: &str) -> bool;

    fn user_call(&self) -> Vec<String>;
    /// The listing of every issue updated since `since` (already URL-encoded).
    fn list_call(&self, since: &str) -> Vec<String>;
    fn issue_call(&self, number: u64) -> Vec<String>;
    fn update_call(&self, number: u64) -> Vec<String>;
    fn comment_call(&self, number: u64) -> Vec<String>;
    fn create_call(&self) -> Vec<String>;
    /// The calls `fetch` makes after reading the issue, with their answers.
    fn fetch_rest(&self, spec: &Spec) -> Vec<(Vec<String>, String)>;
    fn issue_json(&self, spec: &Spec) -> Value;
    /// The body the service expects for the update that sets these fields.
    fn update_body(&self, change: &Update) -> Value;
    /// The body the service expects for a created issue.
    fn create_body(&self, title: &str, body: &str, labels: &[&str], assignees: &[&str]) -> Value;
    /// The response to a comment that got the id `id`.
    fn comment_response(&self, id: u64) -> String;
    /// A comment of the host holding `body`, as the comments of an issue list it.
    fn comment_json(&self, id: u64, login: &str, body: &str) -> Value;
    /// What the host shows as the body of the issue a create just made.
    fn created_body_placeholder(&self) -> &'static str;
    /// Answers the lookups an update with assignees needs.
    fn assignee_lookups(&self, _env: &Env, _logins: &[&str]) {}
}

/// The fields of an update, in the terms a scenario uses.
#[derive(Default, Clone)]
pub struct Update {
    pub title: Option<&'static str>,
    pub body: Option<&'static str>,
    /// `Some(true)` closes.
    pub close: Option<bool>,
    pub labels: Option<Vec<&'static str>>,
    pub assignees: Option<Vec<&'static str>>,
    pub assignee_ids: Option<Vec<u64>>,
}

impl dyn IssueFake + '_ {
    /// Registers the host's answers for a sync that lists `specs` since `since`, and every
    /// call a fetch of each would make.
    pub fn serve_since(&self, env: &Env, since: &str, specs: &[Spec]) {
        env.respond(&strs(&self.user_call()), &self.user_body());
        let listed: Vec<Value> = specs.iter().map(|spec| self.issue_json(spec)).collect();
        // A listing is read with --paginate, so two pages arrive as two JSON arrays.
        let (first, rest) = listed.split_at(listed.len().min(1));
        let mut text = Value::Array(first.to_vec()).to_string();
        if !rest.is_empty() {
            text.push('\n');
            text.push_str(&Value::Array(rest.to_vec()).to_string());
        }
        env.respond(&strs(&self.list_call(since)), &text);
        for spec in specs.iter().filter(|spec| !spec.is_pr) {
            self.serve_issue(env, spec);
        }
    }

    pub fn serve(&self, env: &Env, specs: &[Spec]) {
        self.serve_since(env, FIRST_SINCE, specs);
    }

    /// Answers a fetch of `spec` alone.
    pub fn serve_issue(&self, env: &Env, spec: &Spec) {
        env.respond(
            &strs(&self.issue_call(spec.number)),
            &self.issue_json(spec).to_string(),
        );
        for (call, body) in self.fetch_rest(spec) {
            env.respond(&strs(&call), &body);
        }
    }

    fn user_body(&self) -> String {
        match self.name() {
            "github" => r#"{"login":"me"}"#.to_owned(),
            _ => r#"{"username":"me"}"#.to_owned(),
        }
    }

    pub fn accept_update(&self, env: &Env, number: u64) {
        env.respond(&strs(&self.update_call(number)), "{}");
    }

    pub fn accept_comment(&self, env: &Env, number: u64, id: u64) {
        env.respond(
            &strs(&self.comment_call(number)),
            &self.comment_response(id),
        );
    }

    /// The bodies sent with the calls that match `call`.
    pub fn sent(&self, env: &Env, call: &[String]) -> Vec<Value> {
        let wanted = call.join(" ");
        env.forge_inputs()
            .iter()
            .filter(|(args, _)| *args == wanted)
            .map(|(_, input)| serde_json::from_str(input).unwrap())
            .collect()
    }

    pub fn count(&self, env: &Env, call: &[String]) -> usize {
        let wanted = call.join(" ");
        env.forge_calls()
            .iter()
            .filter(|line| line.split('\t').nth(1) == Some(wanted.as_str()))
            .count()
    }
}

pub struct Github;
pub struct Gitlab;

impl IssueFake for Github {
    fn name(&self) -> &'static str {
        "github"
    }

    fn repo(&self) -> &'static str {
        "o/r"
    }

    fn tracker(&self) -> &'static str {
        "gh-github.com-o-r"
    }

    fn config(&self, push: bool) -> String {
        format!(
            "[[issues.tracker]]\nforge = \"github\"\nhost = \"github.com\"\nrepo = \"o/r\"\nscope = \"all\"\npush = {push}\n"
        )
    }

    fn is_read(&self, args: &str) -> bool {
        args.contains("--method GET") || args.starts_with("api graphql")
    }

    fn user_call(&self) -> Vec<String> {
        args(&["api", "--method", "GET", "user"])
    }

    fn list_call(&self, since: &str) -> Vec<String> {
        args(&[
            "api",
            "--method",
            "GET",
            "--paginate",
            &format!(
                "repos/o/r/issues?state=all&since={since}&sort=updated&direction=asc&per_page=100"
            ),
        ])
    }

    fn issue_call(&self, number: u64) -> Vec<String> {
        args(&[
            "api",
            "--method",
            "GET",
            &format!("repos/o/r/issues/{number}"),
        ])
    }

    fn update_call(&self, number: u64) -> Vec<String> {
        args(&[
            "api",
            "--method",
            "PATCH",
            &format!("repos/o/r/issues/{number}"),
            "--input",
            "-",
        ])
    }

    fn comment_call(&self, number: u64) -> Vec<String> {
        args(&[
            "api",
            "--method",
            "POST",
            &format!("repos/o/r/issues/{number}/comments"),
            "--input",
            "-",
        ])
    }

    fn create_call(&self) -> Vec<String> {
        args(&[
            "api",
            "--method",
            "POST",
            "repos/o/r/issues",
            "--input",
            "-",
        ])
    }

    fn fetch_rest(&self, spec: &Spec) -> Vec<(Vec<String>, String)> {
        let comments: Vec<Value> = spec
            .comments
            .iter()
            .map(|cm| self.comment_json(cm.id, cm.login, &cm.body))
            .collect();
        let node = |pr: &Pr| {
            json!({"number": pr.number, "url": format!("https://github.com/o/r/pull/{}", pr.number),
                "state": "OPEN", "headRefName": pr.branch, "repository": {"nameWithOwner": "o/r"}})
        };
        let links = json!({"data": {"repository": {"issue": {
            "closedByPullRequestsReferences": {"nodes": spec.prs.iter().map(node).collect::<Vec<_>>()},
            "timelineItems": {"nodes": []},
        }}}});
        vec![
            (
                args(&[
                    "api",
                    "--method",
                    "GET",
                    "--paginate",
                    &format!("repos/o/r/issues/{}/comments?per_page=100", spec.number),
                ]),
                Value::Array(comments).to_string(),
            ),
            (
                args(&[
                    "api",
                    "graphql",
                    "-f",
                    "owner=o",
                    "-f",
                    "name=r",
                    "-F",
                    &format!("number={}", spec.number),
                    "-f",
                    "query=IssueLinks",
                ]),
                links.to_string(),
            ),
        ]
    }

    fn issue_json(&self, spec: &Spec) -> Value {
        let mut issue = json!({
            "number": spec.number,
            "node_id": format!("I_{}", spec.number),
            "html_url": format!("https://github.com/o/r/issues/{}", spec.number),
            "title": spec.title,
            "body": spec.body,
            "state": if spec.closed { "closed" } else { "open" },
            "state_reason": if spec.closed { json!(spec.reason.unwrap_or("completed")) } else { Value::Null },
            "labels": spec.labels.iter().map(|name| json!({"name": name})).collect::<Vec<_>>(),
            "assignees": spec.assignees.iter().map(|login| json!({"login": login})).collect::<Vec<_>>(),
            "user": {"login": spec.author},
            "milestone": null,
            "created_at": "2026-10-01T00:00:00Z",
            "updated_at": spec.updated,
        });
        if spec.is_pr {
            issue["pull_request"] = json!({"url": "x"});
        }
        issue
    }

    fn update_body(&self, change: &Update) -> Value {
        let mut body = json!({});
        if let Some(title) = change.title {
            body["title"] = json!(title);
        }
        if let Some(text) = change.body {
            body["body"] = json!(text);
        }
        match change.close {
            Some(true) => {
                body["state"] = json!("closed");
                body["state_reason"] = json!("completed");
            }
            Some(false) => {
                body["state"] = json!("open");
                body["state_reason"] = json!("reopened");
            }
            None => {}
        }
        if let Some(labels) = &change.labels {
            body["labels"] = json!(labels);
        }
        if let Some(assignees) = &change.assignees {
            body["assignees"] = json!(assignees);
        }
        body
    }

    fn create_body(&self, title: &str, body: &str, labels: &[&str], assignees: &[&str]) -> Value {
        json!({"title": title, "body": body, "labels": labels, "assignees": assignees})
    }

    fn comment_response(&self, id: u64) -> String {
        json!({"id": id, "html_url": format!("https://github.com/o/r/issues/1#issuecomment-{id}")})
            .to_string()
    }

    fn comment_json(&self, id: u64, login: &str, body: &str) -> Value {
        json!({"id": id, "html_url": format!("https://github.com/o/r/issues/1#issuecomment-{id}"),
            "body": body, "created_at": "2026-10-05T00:00:00Z", "user": {"login": login}})
    }

    fn created_body_placeholder(&self) -> &'static str {
        "{{LAST_BODY}}"
    }
}

impl IssueFake for Gitlab {
    fn name(&self) -> &'static str {
        "gitlab"
    }

    fn repo(&self) -> &'static str {
        "g/p"
    }

    fn tracker(&self) -> &'static str {
        "gl-gitlab.com-g-p"
    }

    fn config(&self, push: bool) -> String {
        format!(
            "[[issues.tracker]]\nforge = \"gitlab\"\nhost = \"gitlab.com\"\nrepo = \"g/p\"\nscope = \"all\"\npush = {push}\n"
        )
    }

    fn is_read(&self, args: &str) -> bool {
        args.contains("-X GET")
    }

    fn user_call(&self) -> Vec<String> {
        args(&["api", "-X", "GET", "user"])
    }

    fn list_call(&self, since: &str) -> Vec<String> {
        args(&[
            "api",
            "-X",
            "GET",
            "--paginate",
            &format!(
                "projects/g%2Fp/issues?scope=all&state=all&updated_after={since}&order_by=updated_at&sort=asc&per_page=100"
            ),
        ])
    }

    fn issue_call(&self, number: u64) -> Vec<String> {
        args(&[
            "api",
            "-X",
            "GET",
            &format!("projects/g%2Fp/issues/{number}"),
        ])
    }

    fn update_call(&self, number: u64) -> Vec<String> {
        args(&[
            "api",
            "-X",
            "PUT",
            &format!("projects/g%2Fp/issues/{number}"),
            "-H",
            "Content-Type: application/json",
            "--input",
            "-",
        ])
    }

    fn comment_call(&self, number: u64) -> Vec<String> {
        args(&[
            "api",
            "-X",
            "POST",
            &format!("projects/g%2Fp/issues/{number}/notes"),
            "-H",
            "Content-Type: application/json",
            "--input",
            "-",
        ])
    }

    fn create_call(&self) -> Vec<String> {
        args(&[
            "api",
            "-X",
            "POST",
            "projects/g%2Fp/issues",
            "-H",
            "Content-Type: application/json",
            "--input",
            "-",
        ])
    }

    fn fetch_rest(&self, spec: &Spec) -> Vec<(Vec<String>, String)> {
        let discussions: Vec<Value> = spec
            .comments
            .iter()
            .map(|cm| {
                json!({"id": format!("d{}", cm.id), "individual_note": true,
                    "notes": [self.comment_json(cm.id, cm.login, &cm.body)]})
            })
            .collect();
        let mr = |pr: &Pr| {
            json!({"iid": pr.number, "web_url": format!("https://gitlab.com/g/p/-/merge_requests/{}", pr.number),
                "state": "opened", "source_branch": pr.branch, "references": {"full": format!("g/p!{}", pr.number)}})
        };
        let base = format!("projects/g%2Fp/issues/{}", spec.number);
        let get = |path: String| args(&["api", "-X", "GET", "--paginate", &path]);
        vec![
            (
                get(format!("{base}/discussions?per_page=100")),
                Value::Array(discussions).to_string(),
            ),
            (
                get(format!("{base}/closed_by?per_page=100")),
                Value::Array(spec.prs.iter().map(mr).collect()).to_string(),
            ),
            (
                get(format!("{base}/related_merge_requests?per_page=100")),
                "[]".to_owned(),
            ),
        ]
    }

    fn issue_json(&self, spec: &Spec) -> Value {
        json!({
            "iid": spec.number,
            "id": 1000 + spec.number,
            "web_url": format!("https://gitlab.com/g/p/-/issues/{}", spec.number),
            "title": spec.title,
            "description": spec.body,
            "state": if spec.closed { "closed" } else { "opened" },
            "labels": spec.labels,
            "assignees": spec.assignees.iter().map(|login| json!({"username": login})).collect::<Vec<_>>(),
            "author": {"username": spec.author},
            "milestone": null,
            "due_date": null,
            "created_at": "2026-10-01T00:00:00Z",
            "updated_at": spec.updated,
        })
    }

    fn update_body(&self, change: &Update) -> Value {
        let mut body = json!({});
        if let Some(title) = change.title {
            body["title"] = json!(title);
        }
        if let Some(text) = change.body {
            body["description"] = json!(text);
        }
        if let Some(close) = change.close {
            body["state_event"] = json!(if close { "close" } else { "reopen" });
        }
        if let Some(labels) = &change.labels {
            body["labels"] = json!(labels.join(","));
        }
        if let Some(ids) = &change.assignee_ids {
            body["assignee_ids"] = json!(ids);
        }
        body
    }

    fn create_body(&self, title: &str, body: &str, labels: &[&str], _assignees: &[&str]) -> Value {
        json!({"title": title, "description": body, "labels": labels.join(",")})
    }

    fn comment_response(&self, id: u64) -> String {
        json!({"id": id, "body": "x"}).to_string()
    }

    fn comment_json(&self, id: u64, login: &str, body: &str) -> Value {
        json!({"id": id, "body": body, "created_at": "2026-10-05T00:00:00Z",
            "author": {"username": login}, "system": false})
    }

    fn created_body_placeholder(&self) -> &'static str {
        "{{LAST_DESCRIPTION}}"
    }

    fn assignee_lookups(&self, env: &Env, logins: &[&str]) {
        for (index, login) in logins.iter().enumerate() {
            env.respond(
                &["api", "-X", "GET", &format!("users?username={login}")],
                &format!(r#"[{{"id": {}}}]"#, 11 + index),
            );
        }
    }
}
