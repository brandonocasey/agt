use std::ffi::OsString;
use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::error::{Error, Result, ctx, fail};
use crate::model::{RemoteThread, ReviewState};

/// Runs `gh` or `glab` and parses their JSON output. Callers MUST pass only read requests:
/// `--method GET` / `-X GET` REST calls and GraphQL queries, never mutations.
pub struct Client {
    program: OsString,
    name: &'static str,
    env: Vec<(&'static str, String)>,
}

impl Client {
    pub fn gh() -> Self {
        Self {
            program: std::env::var_os("AGENT_REVIEW_GH_BIN").unwrap_or_else(|| "gh".into()),
            name: "gh",
            env: Vec::new(),
        }
    }

    pub fn glab(host: &str) -> Self {
        Self {
            program: std::env::var_os("AGENT_REVIEW_GLAB_BIN").unwrap_or_else(|| "glab".into()),
            name: "glab",
            env: vec![("GITLAB_HOST", host.to_owned())],
        }
    }

    fn output(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.run(args, None)
    }

    fn run(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
        let mut child = Command::new(&self.program)
            .args(args)
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(ctx(format!("run {}", self.name)))?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin
                .write_all(input)
                .map_err(ctx(format!("write to {}", self.name)))?;
        }
        let output = child
            .wait_with_output()
            .map_err(ctx(format!("run {}", self.name)))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return fail(format!(
                "{} {} failed: {}",
                self.name,
                args.first().copied().unwrap_or_default(),
                stderr.trim()
            ));
        }
        Ok(output.stdout)
    }

    pub fn json(&self, args: &[&str]) -> Result<Value> {
        let value: Value = serde_json::from_slice(&self.output(args)?)
            .map_err(ctx(format!("parse {} output", self.name)))?;
        if let Some(errors) = value.get("errors").filter(|errors| !errors.is_null()) {
            return fail(format!("{} returned errors: {errors}", self.name));
        }
        Ok(value)
    }

    /// A write call, with the JSON body on stdin when given (`--input -`); an empty reply is
    /// `null`.
    pub fn send(&self, args: &[&str], body: Option<&Value>) -> Result<Value> {
        let input = body
            .map(|body| serde_json::to_vec(body).map_err(ctx("serialize request")))
            .transpose()?;
        let output = self.run(args, input.as_deref())?;
        if output.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        let value: Value =
            serde_json::from_slice(&output).map_err(ctx(format!("parse {} output", self.name)))?;
        if let Some(errors) = value.get("errors").filter(|errors| !errors.is_null()) {
            return fail(format!("{} returned errors: {errors}", self.name));
        }
        Ok(value)
    }

    /// Items of a `--paginate` call, which prints one JSON array per page.
    pub fn items(&self, args: &[&str]) -> Result<Vec<Value>> {
        let output = self.output(args)?;
        let mut items = Vec::new();
        for page in serde_json::Deserializer::from_slice(&output).into_iter::<Value>() {
            match page.map_err(ctx(format!("parse {} output", self.name)))? {
                Value::Array(page) => items.extend(page),
                other => return fail(format!("{} printed a non-array page: {other}", self.name)),
            }
        }
        Ok(items)
    }
}

/// A `--pr` argument: a number, or a PR/MR URL that also names the host and repository.
pub struct PrRef {
    pub number: u64,
    pub location: Option<(String, String)>,
}

pub fn parse_pr(arg: &str) -> Result<PrRef> {
    if let Ok(number) = arg.trim_start_matches(['#', '!']).parse() {
        return Ok(PrRef {
            number,
            location: None,
        });
    }
    let invalid = || Error(format!("--pr must be a number or a PR/MR URL: {arg}"));
    let (_, rest) = arg.split_once("://").ok_or_else(invalid)?;
    let (host, path) = rest.split_once('/').ok_or_else(invalid)?;
    let (repo, number) = path
        .split_once("/-/merge_requests/")
        .or_else(|| path.split_once("/pull/"))
        .ok_or_else(invalid)?;
    let number = number
        .split(['/', '#', '?'])
        .next()
        .and_then(|number| number.parse().ok())
        .ok_or_else(invalid)?;
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    Ok(PrRef {
        number,
        location: Some((host.to_ascii_lowercase(), repo.trim_matches('/').to_owned())),
    })
}

pub fn encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub struct PrInfo {
    pub repo: String,
    pub url: String,
    pub state: ReviewState,
    pub closed_at: Option<String>,
    pub head_branch: String,
    pub head_sha: String,
    pub base_sha: Option<String>,
    pub start_sha: Option<String>,
    pub author: Option<String>,
}

pub struct CommentData {
    pub id: String,
    pub url: Option<String>,
    pub body: String,
    pub created_at: String,
    pub login: Option<String>,
    pub editor: Option<String>,
    pub review_id: Option<String>,
}

pub struct ThreadData {
    pub info: RemoteThread,
    pub resolved: bool,
    pub comments: Vec<CommentData>,
    /// GitLab position head, used to decide whether the line still maps to the latest diff.
    pub position_head: Option<String>,
}

pub struct Draft {
    pub id: String,
    pub body: String,
}

pub struct Fetched {
    pub pr: PrInfo,
    pub threads: Vec<ThreadData>,
    pub drafts: Vec<Draft>,
}

pub fn text(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

pub fn number(value: &Value, pointer: &str) -> Option<u32> {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .and_then(|number| u32::try_from(number).ok())
}

/// A JSON id that may be a number or a string.
pub fn id(value: &Value, pointer: &str) -> Option<String> {
    match value.pointer(pointer)? {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) => Some(text.clone()),
        _ => None,
    }
}

pub fn required(value: &Value, pointer: &str, what: &str) -> Result<String> {
    text(value, pointer).ok_or_else(|| Error(format!("{what} is missing {pointer}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pr_arguments() {
        assert_eq!(parse_pr("42").unwrap().number, 42);
        let github = parse_pr("https://github.com/o/r/pull/1235/files").unwrap();
        assert_eq!(github.number, 1235);
        assert_eq!(github.location, Some(("github.com".into(), "o/r".into())));
        let gitlab =
            parse_pr("https://gitlab.example.com/g/s/p/-/merge_requests/3#note_1").unwrap();
        assert_eq!(gitlab.number, 3);
        assert_eq!(
            gitlab.location,
            Some(("gitlab.example.com".into(), "g/s/p".into()))
        );
        assert!(parse_pr("nope").is_err());
        assert_eq!(encode("feat/a b"), "feat%2Fa%20b");
    }
}
