//! How a [`Request`] reaches a host through a CLI (`gh`, `glab`) that holds its own login.
//! Services build requests; the transport parses replies and extracts failure status codes.

use std::ffi::OsString;
use std::fmt;
use std::io::ErrorKind;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::core::env;
use crate::core::error::{Error, Result, ctx, fail};
use crate::core::process;
use crate::forge::service::{self, Endpoint, Pagination, Request, Var};
use crate::store::model::Forge;

/// A successful reply (status 2xx); a non-2xx status is a [`TransportError`].
pub struct Response {
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

/// When the forge calls of the current command must be done. Only the auto-sync of a read sets
/// it; explicit commands run without a cap.
static DEADLINE: Mutex<Option<(Instant, Duration)>> = Mutex::new(None);

/// Caps the forge calls started from now on at `limit` in total, until [`clear_deadline`].
pub fn set_deadline(limit: Duration) {
    if let Ok(mut deadline) = DEADLINE.lock() {
        *deadline = Some((Instant::now() + limit, limit));
    }
}

pub fn clear_deadline() {
    if let Ok(mut deadline) = DEADLINE.lock() {
        *deadline = None;
    }
}

fn deadline() -> Option<(Instant, Duration)> {
    DEADLINE.lock().ok().and_then(|deadline| *deadline)
}

fn timed_out(name: &str, limit: Duration) -> TransportError {
    TransportError::new(format!("{name} timed out after {limit:?}"))
}

/// A CLI process: `gh` or `glab` with its environment.
pub struct Cli {
    program: OsString,
    name: &'static str,
    env: Vec<(&'static str, String)>,
}

impl Cli {
    pub fn gh(host: &str) -> Self {
        let name = service::kind(Forge::Github).tool;
        Self {
            program: env::value_or("AGT_GH_BIN", name),
            name,
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
        let name = service::kind(Forge::Gitlab).tool;
        Self {
            program: env::value_or("AGT_GLAB_BIN", name),
            name,
            env: Vec::new(),
        }
    }

    /// A tool other than gh and glab, such as `git` or `mise`; the `AGT_*` variable `over`
    /// overrides the program.
    pub fn other(name: &'static str, over: &str) -> Self {
        Self {
            program: env::value_or(over, name),
            name,
            env: Vec::new(),
        }
    }

    /// Whether a read-only command succeeded, with its stdout and stderr; `None` when the tool
    /// is missing or the deadline passed.
    pub fn probe(&self, args: &[&str]) -> Option<(bool, String, String)> {
        let output = self.capture(args, None).ok()?.output;
        Some((
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
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
        let captured = self.capture(args, input)?;
        let output = captured.output;
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
        captured.input.map_err(ctx(format!("write to {name}")))?;
        Ok(output.stdout)
    }

    fn capture(
        &self,
        args: &[&str],
        input: Option<&[u8]>,
    ) -> std::result::Result<process::Captured, TransportError> {
        let deadline = deadline();
        process::output(
            Command::new(&self.program)
                .args(args)
                .envs(self.env.iter().map(|(key, value)| (key, value))),
            input,
            deadline.map(|(end, _)| end),
        )
        .map_err(|error| match deadline {
            Some((_, limit)) if error.kind() == ErrorKind::TimedOut => timed_out(self.name, limit),
            _ => TransportError::new(format!("run {}: {error}", self.name)),
        })
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
    /// Which tool the requests are for; each spells the `api` subcommand's flags differently.
    forge: Forge,
}

impl CliTransport {
    pub fn gh(host: &str) -> Self {
        Self {
            cli: Cli::gh(host),
            forge: Forge::Github,
        }
    }

    pub fn glab(host: &str) -> Self {
        Self {
            cli: Cli::glab(host),
            forge: Forge::Gitlab,
        }
    }

    /// glab on the host it is configured for, or `GITLAB_HOST` when that is set.
    pub fn glab_configured() -> Self {
        Self {
            cli: Cli::glab_configured(),
            forge: Forge::Gitlab,
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
        let body = if request.raw {
            Value::String(String::from_utf8_lossy(&output).into_owned())
        } else if request.paginate {
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
            headers: Vec::new(),
            body,
        })
    }

    fn render(&self, request: &Request) -> Vec<String> {
        match self.forge {
            Forge::Github => gh_args(request),
            Forge::Gitlab => glab_args(request),
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

/// The HTTP status a CLI reports in its error text: the `(HTTP nnn)` suffix, as in
/// `gh: Not Found (HTTP 404)` and `glab: 404 Project Not Found (HTTP 404)`, or a bare
/// `HTTP nnn` that starts a line or directly follows the `gh: `/`glab: ` prefix. Other numbers
/// in the text (a URL's `pulls/412/reviews`, a port) are not statuses.
fn status_in(stderr: &str) -> Option<u16> {
    let three = |digits: &str| -> Option<u16> {
        let digits = digits.get(..3)?;
        let status: u16 = digits.parse().ok()?;
        (400..600).contains(&status).then_some(status)
    };
    let suffix = stderr.match_indices("(HTTP ").find_map(|(at, marker)| {
        let (digits, _) = stderr[at + marker.len()..].split_once(')')?;
        (digits.len() == 3).then(|| three(digits)).flatten()
    });
    let bare = || {
        stderr.lines().find_map(|line| {
            let line = line.trim_start();
            let line = ["gh: ", "glab: "]
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
                .unwrap_or(line);
            let digits = line.strip_prefix("HTTP ")?;
            let after = digits.get(3..)?;
            (!after.starts_with(|c: char| c.is_ascii_digit()))
                .then(|| three(digits))
                .flatten()
        })
    };
    Some(as_seen_by_retries(suffix.or_else(bare)?, stderr))
}

/// GitHub answers a rate limit or abuse detection with 403, which is worth retrying later,
/// unlike a real 403.
pub(crate) fn as_seen_by_retries(status: u16, text: &str) -> u16 {
    let text = text.to_ascii_lowercase();
    if status == 403
        && ["rate limit", "abuse detection", "secondary rate limit"]
            .iter()
            .any(|marker| text.contains(marker))
    {
        429
    } else {
        status
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;
    use crate::core::testing::Sandbox;
    use crate::forge::service::Method;

    fn rest(method: Method, path: &str, body: Option<Value>) -> Request {
        Request::rest(method, path, body)
    }

    #[test]
    fn consecutive_forge_calls_share_the_remaining_deadline() {
        let sandbox = Sandbox::new();
        let tool = Cli {
            program: "sh".into(),
            name: "test forge",
            env: [
                "HOME",
                "XDG_STATE_HOME",
                "XDG_CONFIG_HOME",
                "XDG_CACHE_HOME",
            ]
            .into_iter()
            .map(|key| (key, sandbox.path().display().to_string()))
            .collect(),
        };
        set_deadline(Duration::from_millis(1500));
        let started = Instant::now();
        let first = tool.run(&["-c", "sleep 1"], None);
        let second = tool.run(&["-c", "sleep 3"], None);
        let elapsed = started.elapsed();
        let exhausted = tool.run(&["-c", "exit 0"], None);
        clear_deadline();
        assert!(first.is_ok(), "{first:?}");
        assert!(second.unwrap_err().message.contains("timed out after 1.5s"));
        assert!(exhausted.is_err());
        assert!(elapsed < Duration::from_millis(2500), "{elapsed:?}");
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
            status_in("glab: 404 Project Not Found (HTTP 404)"),
            Some(404)
        );
        assert_eq!(
            status_in("gh: Resource not accessible by integration (HTTP 403)"),
            Some(403)
        );
        assert_eq!(
            status_in("fake-forge: simulated failure: api -X POST"),
            None
        );
        assert_eq!(status_in("dial tcp: i/o timeout"), None);
        assert_eq!(status_in("gh: ok (HTTP 200)"), None);
    }

    #[test]
    fn numbers_in_urls_and_ports_are_not_statuses() {
        for text in [
            "Get https://api.github.com/repos/o/r/pulls/412/reviews: unexpected EOF",
            "dial tcp 140.82.112.5:443: connection reset",
            "glab: 404 Not Found",
            "gh: 403 Forbidden",
            "gh: see https://x/HTTP 404 and :443",
            "curl: HTTP 404 in the middle",
            "gh: HTTP 4041",
        ] {
            assert_eq!(status_in(text), None, "{text}");
        }
        let eof = TransportError {
            status: status_in("Post https://x/pulls/412/reviews: EOF"),
            message: String::new(),
        };
        assert!(!eof.refused());
    }

    #[test]
    fn bare_http_statuses_are_read_when_anchored() {
        assert_eq!(status_in("gh: HTTP 404"), Some(404));
        assert_eq!(status_in("glab: HTTP 422: Unprocessable"), Some(422));
        assert_eq!(status_in("HTTP 404: Not Found"), Some(404));
        assert_eq!(
            status_in("note\nHTTP 403: Resource not accessible"),
            Some(403)
        );
    }

    #[test]
    fn abuse_detection_is_transient() {
        for text in [
            "gh: You have triggered an abuse detection mechanism (HTTP 403)",
            "gh: HTTP 403: You have exceeded a secondary rate limit",
        ] {
            assert_eq!(status_in(text), Some(429), "{text}");
        }
    }

    #[test]
    fn a_rate_limit_is_transient() {
        let limited = status_in("gh: API rate limit exceeded for user (HTTP 403)");
        assert_eq!(limited, Some(429));
        let error = TransportError {
            status: limited,
            message: String::new(),
        };
        assert!(!error.refused());
        let forbidden = status_in("gh: Resource not accessible (HTTP 403)");
        assert_eq!(forbidden, Some(403));
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
