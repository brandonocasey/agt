//! How a [`Request`] reaches a host: a CLI (`gh`, `glab`) that holds its own login, or HTTP with
//! a token. Services build requests; a transport carries them out and reports the status and
//! headers, which the CLIs' own output hides.

use std::ffi::OsString;
use std::fmt;
use std::io::{ErrorKind, Write};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::error::{Error, Result, ctx, fail};
use crate::service::{Endpoint, Pagination, Request, Var};

/// A successful reply (status 2xx); a non-2xx status is a [`TransportError`].
pub struct Response {
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "read by the token transport's pagers")
    )]
    pub status: u16,
    /// Lowercase names. Empty for a CLI, which prints no headers.
    pub headers: Vec<(String, String)>,
    /// `null` for an empty reply; every item of all pages for a paginated request.
    pub body: Value,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[derive(Debug)]
pub struct TransportError {
    /// The HTTP status when the host answered, else `None` (the tool or network failed).
    pub status: Option<u16>,
    pub message: String,
}

impl TransportError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
        }
    }

    /// Whether the host answered with a refusal that repeating will not change (a 4xx other
    /// than a timeout or rate limit), as opposed to a failure worth retrying.
    pub fn refused(&self) -> bool {
        self.status.is_some_and(|status| {
            (400..500).contains(&status) && !matches!(status, 408 | 425 | 429)
        })
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<TransportError> for Error {
    fn from(error: TransportError) -> Self {
        Self(error.message)
    }
}

pub trait Transport {
    fn send(&self, request: &Request) -> std::result::Result<Response, TransportError>;
    /// What a dry run shows for a request.
    fn render(&self, request: &Request) -> Vec<String>;
}

impl dyn Transport + '_ {
    /// The body of a read that must answer with JSON.
    pub fn json(&self, request: &Request) -> Result<Value> {
        match self.send(request)?.body {
            Value::Null => fail("the host sent an empty reply"),
            body => Ok(body),
        }
    }

    /// Every item of a paginated read, page by page as `pagination` says. `GraphqlCursor` is the
    /// service's own loop.
    pub fn items(&self, request: &Request, pagination: &Pagination) -> Result<Vec<Value>> {
        let mut items = Vec::new();
        let mut request = request.clone();
        if *pagination == Pagination::CliPaginate {
            request.paginate = true;
        }
        loop {
            let response = self.send(&request)?;
            let next = next_page(pagination, &request, &response);
            match response.body {
                Value::Array(page) => items.extend(page),
                other => return fail(format!("a page is not an array: {other}")),
            }
            match next {
                Some(next) => request = next,
                None => return Ok(items),
            }
        }
    }
}

/// The request for the page after `response`, if the strategy says there is one.
fn next_page(pagination: &Pagination, request: &Request, response: &Response) -> Option<Request> {
    let Endpoint::Rest(path) = &request.endpoint else {
        return None;
    };
    let url = match pagination {
        Pagination::LinkHeader => next_link(response.header("link")?)?,
        Pagination::NextUrlInBody(pointer) => response.body.pointer(pointer)?.as_str()?.to_owned(),
        Pagination::ContinuationHeader(name) => {
            let token = response.header(name).filter(|token| !token.is_empty())?;
            let base = path.split("&continuationToken=").next().unwrap_or(path);
            let sep = if base.contains('?') { '&' } else { '?' };
            format!("{base}{sep}continuationToken={token}")
        }
        Pagination::GraphqlCursor | Pagination::CliPaginate => return None,
    };
    Some(Request {
        endpoint: Endpoint::Rest(url),
        ..request.clone()
    })
}

/// The `rel="next"` URL of a `Link` header.
fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        let is_next = params
            .split(';')
            .any(|param| param.trim().replace(' ', "") == "rel=\"next\"");
        let url = url.trim().strip_prefix('<')?.strip_suffix('>')?;
        is_next.then(|| url.to_owned())
    })
}

/// The tool a [`Cli`] runs; each spells the `api` subcommand's flags differently.
#[derive(Clone, Copy)]
pub enum Tool {
    Gh,
    Glab,
}

/// A CLI process: `gh` or `glab` with its environment.
pub struct Cli {
    program: OsString,
    name: &'static str,
    env: Vec<(&'static str, String)>,
}

impl Cli {
    pub fn gh(host: &str) -> Self {
        Self {
            program: std::env::var_os("AGENT_REVIEW_GH_BIN").unwrap_or_else(|| "gh".into()),
            name: "gh",
            env: vec![("GH_HOST", host.to_owned())],
        }
    }

    pub fn glab(host: &str) -> Self {
        Self {
            env: vec![("GITLAB_HOST", host.to_owned())],
            ..Self::glab_configured()
        }
    }

    /// glab on the host its own configuration names.
    pub fn glab_configured() -> Self {
        Self {
            program: std::env::var_os("AGENT_REVIEW_GLAB_BIN").unwrap_or_else(|| "glab".into()),
            name: "glab",
            env: Vec::new(),
        }
    }

    /// The trimmed output of a read-only command, such as `glab config get host`; `None` when
    /// it fails or the tool is missing, which callers read as "not configured".
    pub fn text(&self, args: &[&str]) -> Option<String> {
        let output = self.run(args, None).ok()?;
        Some(String::from_utf8_lossy(&output).trim().to_owned())
    }

    fn run(
        &self,
        args: &[&str],
        input: Option<&[u8]>,
    ) -> std::result::Result<Vec<u8>, TransportError> {
        let name = self.name;
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
            .map_err(ctx(format!("run {name}")))?;
        // Input is written from another thread so a tool that exits before reading it shows
        // its own error, not a broken pipe, and a large reply cannot fill the pipe.
        let writer = match (input, child.stdin.take()) {
            (Some(input), Some(mut stdin)) => {
                let input = input.to_vec();
                Some(std::thread::spawn(move || stdin.write_all(&input)))
            }
            _ => None,
        };
        let output = child
            .wait_with_output()
            .map_err(ctx(format!("run {name}")))?;
        let wrote = writer.map(std::thread::JoinHandle::join);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(TransportError {
                status: status_in(&stderr),
                message: format!(
                    "{name} {} failed: {}",
                    args.first().copied().unwrap_or_default(),
                    stderr.trim()
                ),
            });
        }
        match wrote {
            None | Some(Ok(Ok(()))) => {}
            Some(Ok(Err(error))) if error.kind() == ErrorKind::BrokenPipe => {}
            Some(Ok(Err(error))) => {
                return Err(TransportError::new(format!("write to {name}: {error}")));
            }
            Some(Err(_)) => {
                return Err(TransportError::new(format!("{name} input writer panicked")));
            }
        }
        Ok(output.stdout)
    }
}

impl From<Error> for TransportError {
    fn from(error: Error) -> Self {
        Self::new(error.0)
    }
}

/// `gh` or `glab` running `api` requests.
pub struct CliTransport {
    cli: Cli,
    tool: Tool,
}

impl CliTransport {
    pub fn gh(host: &str) -> Self {
        Self {
            cli: Cli::gh(host),
            tool: Tool::Gh,
        }
    }

    pub fn glab(host: &str) -> Self {
        Self {
            cli: Cli::glab(host),
            tool: Tool::Glab,
        }
    }
}

impl Transport for CliTransport {
    fn send(&self, request: &Request) -> std::result::Result<Response, TransportError> {
        let args = self.render(request);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let input = request
            .body
            .as_ref()
            .map(|body| serde_json::to_vec(body).map_err(ctx("serialize request")))
            .transpose()?;
        let output = self.cli.run(&args, input.as_deref())?;
        let name = self.cli.name;
        let body = if request.paginate {
            // One JSON array per page.
            let mut items = Vec::new();
            for page in serde_json::Deserializer::from_slice(&output).into_iter::<Value>() {
                match page.map_err(ctx(format!("parse {name} output")))? {
                    Value::Array(page) => items.extend(page),
                    other => {
                        return Err(TransportError::new(format!(
                            "{name} printed a non-array page: {other}"
                        )));
                    }
                }
            }
            Value::Array(items)
        } else if output.iter().all(u8::is_ascii_whitespace) {
            Value::Null
        } else {
            let value: Value =
                serde_json::from_slice(&output).map_err(ctx(format!("parse {name} output")))?;
            if let Some(errors) = value.get("errors").filter(|errors| !errors.is_null()) {
                return Err(TransportError::new(format!(
                    "{name} returned errors: {errors}"
                )));
            }
            value
        };
        Ok(Response {
            status: 200,
            headers: Vec::new(),
            body,
        })
    }

    fn render(&self, request: &Request) -> Vec<String> {
        match self.tool {
            Tool::Gh => gh_args(request),
            Tool::Glab => glab_args(request),
        }
    }
}

fn graphql_args(vars: &[Var]) -> impl Iterator<Item = String> + '_ {
    vars.iter().flat_map(|var| {
        [
            if var.typed { "-F" } else { "-f" }.to_owned(),
            format!("{}={}", var.name, var.value),
        ]
    })
}

/// The `gh api` arguments of a request.
pub fn gh_args(request: &Request) -> Vec<String> {
    let mut args = vec!["api".to_owned()];
    match &request.endpoint {
        Endpoint::Rest(path) => {
            args.extend(["--method".to_owned(), request.method.as_str().to_owned()]);
            if request.paginate {
                args.push("--paginate".to_owned());
            }
            args.push(path.clone());
            if request.body.is_some() {
                args.extend(["--input".to_owned(), "-".to_owned()]);
            }
        }
        Endpoint::Graphql(vars) => {
            args.push("graphql".to_owned());
            args.extend(graphql_args(vars));
        }
    }
    args
}

/// The `glab api` arguments of a request.
pub fn glab_args(request: &Request) -> Vec<String> {
    let mut args = vec!["api".to_owned()];
    match &request.endpoint {
        Endpoint::Rest(path) => {
            args.extend(["-X".to_owned(), request.method.as_str().to_owned()]);
            if request.paginate {
                args.push("--paginate".to_owned());
            }
            args.push(path.clone());
            if request.body.is_some() {
                args.extend(
                    ["-H", "Content-Type: application/json", "--input", "-"].map(str::to_owned),
                );
            }
        }
        Endpoint::Graphql(vars) => {
            args.push("graphql".to_owned());
            args.extend(graphql_args(vars));
        }
    }
    args
}

/// The HTTP status a CLI reports in its error text, such as `gh: Not Found (HTTP 404)` or
/// `glab: 403 Forbidden`.
fn status_in(stderr: &str) -> Option<u16> {
    let status = |digits: &str| {
        let status: u16 = digits.get(..3)?.parse().ok()?;
        ((400..600).contains(&status)
            && !digits.get(3..)?.starts_with(|c: char| c.is_ascii_digit()))
        .then_some(status)
    };
    if let Some((_, after)) = stderr.split_once("HTTP ")
        && let Some(found) = status(after)
    {
        return Some(found);
    }
    let words: Vec<&str> = stderr.split_whitespace().collect();
    words.windows(2).find_map(|pair| {
        let named = pair[1].starts_with(|c: char| c.is_ascii_uppercase());
        status(pair[0].trim_matches(|c: char| !c.is_ascii_digit())).filter(|_| named)
    })
}

mod token;

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;
    use crate::service::Method;

    fn rest(method: Method, path: &str, body: Option<Value>) -> Request {
        Request::rest(method, path, body)
    }

    #[test]
    fn gh_renders_its_api_flags() {
        let get = rest(Method::Get, "repos/o/r", None);
        assert_eq!(gh_args(&get), ["api", "--method", "GET", "repos/o/r"]);
        let post = rest(Method::Post, "repos/o/r/x", Some(json!({})));
        assert_eq!(
            gh_args(&post),
            ["api", "--method", "POST", "repos/o/r/x", "--input", "-"]
        );
        let graphql = Request::graphql(vec![
            Var::text("owner", "o"),
            Var::typed("number", "7"),
            Var::text("query", "query Q{x}"),
        ]);
        assert_eq!(
            gh_args(&graphql),
            [
                "api",
                "graphql",
                "-f",
                "owner=o",
                "-F",
                "number=7",
                "-f",
                "query=query Q{x}"
            ]
        );
    }

    #[test]
    fn glab_renders_its_api_flags() {
        let get = Request {
            paginate: true,
            ..rest(Method::Get, "projects/1", None)
        };
        assert_eq!(
            glab_args(&get),
            ["api", "-X", "GET", "--paginate", "projects/1"]
        );
        let put = rest(Method::Put, "p/draft_notes/1/publish", None);
        assert_eq!(
            glab_args(&put),
            ["api", "-X", "PUT", "p/draft_notes/1/publish"]
        );
        let post = rest(Method::Post, "p/notes", Some(json!({})));
        assert_eq!(
            glab_args(&post),
            [
                "api",
                "-X",
                "POST",
                "p/notes",
                "-H",
                "Content-Type: application/json",
                "--input",
                "-"
            ]
        );
    }

    #[test]
    fn statuses_come_from_cli_error_text() {
        assert_eq!(status_in("gh: Not Found (HTTP 404)"), Some(404));
        assert_eq!(
            status_in("gh: HTTP 403: Resource not accessible"),
            Some(403)
        );
        assert_eq!(status_in("glab: 403 Forbidden"), Some(403));
        assert_eq!(status_in("glab api failed: 401 Unauthorized"), Some(401));
        assert_eq!(
            status_in("fake-forge: simulated failure: api -X POST"),
            None
        );
        assert_eq!(status_in("dial tcp: i/o timeout"), None);
        assert_eq!(status_in("HTTP 200 OK"), None);
    }

    #[test]
    fn only_real_refusals_are_refused() {
        let status = |status| TransportError {
            status,
            message: String::new(),
        };
        for refused in [400, 401, 403, 404, 405, 409, 422] {
            assert!(status(Some(refused)).refused(), "{refused}");
        }
        for transient in [None, Some(408), Some(429), Some(500), Some(502), Some(503)] {
            assert!(!status(transient).refused(), "{transient:?}");
        }
    }

    struct Pages {
        replies: RefCell<Vec<Response>>,
        seen: RefCell<Vec<String>>,
    }

    impl Transport for Pages {
        fn send(&self, request: &Request) -> std::result::Result<Response, TransportError> {
            let Endpoint::Rest(path) = &request.endpoint else {
                unreachable!()
            };
            self.seen.borrow_mut().push(path.clone());
            Ok(self.replies.borrow_mut().remove(0))
        }

        fn render(&self, _request: &Request) -> Vec<String> {
            Vec::new()
        }
    }

    fn page(headers: &[(&str, &str)], body: Value) -> Response {
        Response {
            status: 200,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body,
        }
    }

    fn pages(replies: Vec<Response>) -> Pages {
        Pages {
            replies: RefCell::new(replies),
            seen: RefCell::new(Vec::new()),
        }
    }

    #[test]
    fn link_headers_are_followed() {
        let transport = pages(vec![
            page(
                &[(
                    "link",
                    "<https://h/a?page=2>; rel=\"next\", <https://h/a?page=9>; rel=\"last\"",
                )],
                json!([1, 2]),
            ),
            page(
                &[("link", "<https://h/a?page=1>; rel=\"prev\"")],
                json!([3]),
            ),
        ]);
        let transport: &dyn Transport = &transport;
        let items = transport
            .items(&rest(Method::Get, "a", None), &Pagination::LinkHeader)
            .unwrap();
        assert_eq!(items, [json!(1), json!(2), json!(3)]);
    }

    #[test]
    fn next_urls_in_the_body_are_followed() {
        let strategy = Pagination::NextUrlInBody("/next");
        let request = rest(Method::Get, "a", None);
        let reply = page(&[], json!({ "values": [1], "next": "https://h/a?p=2" }));
        assert_eq!(
            next_page(&strategy, &request, &reply).map(|next| next.endpoint),
            Some(Endpoint::Rest("https://h/a?p=2".into()))
        );
        assert!(next_page(&strategy, &request, &page(&[], json!({ "values": [] }))).is_none());
    }

    #[test]
    fn continuation_tokens_are_appended_once() {
        let strategy = Pagination::ContinuationHeader("x-ms-continuationtoken");
        let request = rest(Method::Get, "a?top=100", None);
        let reply = page(&[("x-ms-continuationtoken", "t1")], json!([]));
        let next = next_page(&strategy, &request, &reply).unwrap();
        assert_eq!(
            next.endpoint,
            Endpoint::Rest("a?top=100&continuationToken=t1".into())
        );
        let reply = page(&[("x-ms-continuationtoken", "t2")], json!([]));
        let again = next_page(&strategy, &next, &reply).unwrap();
        assert_eq!(
            again.endpoint,
            Endpoint::Rest("a?top=100&continuationToken=t2".into())
        );
        assert!(next_page(&strategy, &again, &page(&[], json!([]))).is_none());
    }
}
