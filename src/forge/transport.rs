//! How a [`Request`] reaches a host through a CLI (`gh`, `glab`, `tea`) that holds its own login.
//! Services build requests; the transport parses replies and extracts failure status codes.

use std::ffi::OsString;
use std::fmt;
use std::io::ErrorKind;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::core::env;
use crate::core::error::{Error, ctx};
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

#[derive(Debug, Clone)]
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
    pub fn json(&self, request: &Request) -> std::result::Result<Value, TransportError> {
        match self.send(request)?.body {
            Value::Null => Err(TransportError::new("the host sent an empty reply")),
            body => Ok(body),
        }
    }

    /// Every item of a paginated read, page by page as `pagination` says. `GraphqlCursor` is the
    /// service's own loop.
    pub fn items(
        &self,
        request: &Request,
        pagination: &Pagination,
    ) -> std::result::Result<Vec<Value>, TransportError> {
        let mut items = Vec::new();
        let mut request = request.clone();
        match pagination {
            Pagination::CliPaginate => request.paginate = true,
            Pagination::PageNumbers(size) => request = first_page(request, *size),
            _ => {}
        }
        loop {
            let response = self.send(&request)?;
            let next = next_page(pagination, &request, &response);
            match response.body {
                Value::Array(page) => items.extend(page),
                other => {
                    return Err(TransportError::new(format!(
                        "a page is not an array: {other}"
                    )));
                }
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
        Pagination::PageNumbers(size) => {
            // A page shorter than the size is the last one.
            if response.body.as_array()?.len() < *size {
                return None;
            }
            let (before, page) = path.rsplit_once("page=")?;
            format!("{before}page={}", page.parse::<usize>().ok()? + 1)
        }
        Pagination::GraphqlCursor | Pagination::CliPaginate => return None,
    };
    Some(Request {
        endpoint: Endpoint::Rest(url),
        ..request.clone()
    })
}

/// `path` asking for page `page` of `size` items; `page` is the last query parameter, which
/// [`next_page`] relies on.
pub fn with_page(path: &str, page: usize, size: usize) -> String {
    let sep = if path.contains('?') { '&' } else { '?' };
    format!("{path}{sep}limit={size}&page={page}")
}

fn first_page(request: Request, size: usize) -> Request {
    match &request.endpoint {
        Endpoint::Rest(path) => Request {
            endpoint: Endpoint::Rest(with_page(path, 1, size)),
            ..request
        },
        Endpoint::Graphql(_) => request,
    }
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

/// A CLI process: `gh`, `glab` or `tea` with its environment.
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

    /// tea, which holds a login per host and takes none from the environment.
    pub fn tea() -> Self {
        let name = service::kind(Forge::Forgejo).tool;
        Self {
            program: env::value_or("AGT_TEA_BIN", name),
            name,
            env: Vec::new(),
        }
    }

    /// A tool other than gh, glab and tea, such as `git` or `mise`; the `AGT_*` variable `over`
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
        self.run_with_stderr(args, input).map(|(stdout, _)| stdout)
    }

    /// Like [`Cli::run`], and also the stderr of a successful run.
    fn run_with_stderr(
        &self,
        args: &[&str],
        input: Option<&[u8]>,
    ) -> std::result::Result<(Vec<u8>, String), TransportError> {
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
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        Ok((output.stdout, stderr))
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

/// `gh`, `glab` or `tea` running `api` requests.
pub struct CliTransport {
    cli: Cli,
    /// Which tool the requests are for; each spells the `api` subcommand's flags differently.
    forge: Forge,
    /// The host a `tea api` call is for; only tea needs it.
    tea: Option<TeaHost>,
}

/// The host a `tea api` call is for, and the tea login that reaches it, found on first use.
struct TeaHost {
    host: String,
    login: OnceLock<std::result::Result<String, TransportError>>,
}

/// The logins `tea logins list` shows, read once per process. A failed read is not kept, so the
/// next call tries again.
fn cached_tea_logins(cli: &Cli) -> std::result::Result<&'static [TeaLogin], TransportError> {
    static LOGINS: OnceLock<Vec<TeaLogin>> = OnceLock::new();
    if let Some(logins) = LOGINS.get() {
        return Ok(logins);
    }
    let out = cli.run(TEA_LOGINS_LIST, None)?;
    Ok(LOGINS.get_or_init(|| parse_tea_logins(&String::from_utf8_lossy(&out))))
}

impl TeaHost {
    fn login(&self, cli: &Cli) -> std::result::Result<&str, TransportError> {
        self.login
            .get_or_init(|| {
                let logins = cached_tea_logins(cli)?;
                tea_login_for(logins, &self.host)
                    .map(|login| login.name.clone())
                    .ok_or_else(|| {
                        TransportError::new(format!(
                            "tea has no login for {host}: run `tea login add --url https://{host}`",
                            host = self.host
                        ))
                    })
            })
            .as_deref()
            .map_err(Clone::clone)
    }
}

impl CliTransport {
    pub fn gh(host: &str) -> Self {
        Self {
            cli: Cli::gh(host),
            forge: Forge::Github,
            tea: None,
        }
    }

    pub fn glab(host: &str) -> Self {
        Self {
            cli: Cli::glab(host),
            forge: Forge::Gitlab,
            tea: None,
        }
    }

    pub fn tea(host: &str) -> Self {
        Self {
            cli: Cli::tea(),
            forge: Forge::Forgejo,
            tea: Some(TeaHost {
                host: host.to_owned(),
                login: OnceLock::new(),
            }),
        }
    }

    /// glab on the host it is configured for, or `GITLAB_HOST` when that is set.
    pub fn glab_configured() -> Self {
        Self {
            cli: Cli::glab_configured(),
            forge: Forge::Gitlab,
            tea: None,
        }
    }
}

impl Transport for CliTransport {
    fn send(&self, request: &Request) -> std::result::Result<Response, TransportError> {
        if let Some(tea) = &self.tea {
            if matches!(request.endpoint, Endpoint::Graphql(_)) {
                return Err(TransportError::new("tea has no GraphQL endpoint"));
            }
            tea.login(&self.cli)?;
        }
        let args = self.render(request);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let input = request
            .body
            .as_ref()
            .map(|body| serde_json::to_vec(body).map_err(ctx("serialize request")))
            .transpose()?;
        let (output, stderr) = self.cli.run_with_stderr(&args, input.as_deref())?;
        if self.tea.is_some()
            && let Some(refusal) = tea_refusal(&stderr, &output)
        {
            return Err(refusal);
        }
        Ok(Response {
            headers: Vec::new(),
            body: read_body(self.cli.name, request, &output)?,
        })
    }

    fn render(&self, request: &Request) -> Vec<String> {
        match self.forge {
            Forge::Github => gh_args(request),
            Forge::Gitlab => glab_args(request),
            Forge::Forgejo => {
                // A dry run names the host where no login was found.
                let login = self
                    .tea
                    .as_ref()
                    .map_or("", |tea| tea.login(&self.cli).unwrap_or(tea.host.as_str()));
                tea_args(request, login)
            }
        }
    }
}

/// The JSON a CLI printed for `request`: one value, or for a paginated read the items of every
/// page; the text itself for a raw read, and `Null` for no output.
fn read_body(
    name: &str,
    request: &Request,
    output: &[u8],
) -> std::result::Result<Value, TransportError> {
    if request.raw {
        return Ok(Value::String(String::from_utf8_lossy(output).into_owned()));
    }
    if request.paginate {
        // One JSON array per page.
        let mut items = Vec::new();
        for page in serde_json::Deserializer::from_slice(output).into_iter::<Value>() {
            match page.map_err(ctx(format!("parse {name} output")))? {
                Value::Array(page) => items.extend(page),
                other => {
                    return Err(TransportError::new(format!(
                        "{name} printed a non-array page: {other}"
                    )));
                }
            }
        }
        return Ok(Value::Array(items));
    }
    if output.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null);
    }
    let value: Value =
        serde_json::from_slice(output).map_err(ctx(format!("parse {name} output")))?;
    if let Some(errors) = value.get("errors").filter(|errors| !errors.is_null()) {
        return Err(TransportError::new(format!(
            "{name} returned errors: {errors}"
        )));
    }
    Ok(value)
}

fn graphql_args(vars: &[Var]) -> impl Iterator<Item = String> + '_ {
    vars.iter().flat_map(|var| {
        [
            if var.typed { "-F" } else { "-f" }.to_owned(),
            format!("{}={}", var.name, var.value),
        ]
    })
}

/// How a CLI spells the flags of its `api` subcommand.
struct ApiFlags {
    method: &'static str,
    /// What follows the path to send the request body from stdin.
    json_input: &'static [&'static str],
    /// What precedes the file in a file upload, and what the file path follows in its argument.
    file_input: (&'static [&'static str], &'static str),
}

const GH_FLAGS: ApiFlags = ApiFlags {
    method: "--method",
    json_input: &["--input", "-"],
    file_input: (
        &["-H", "Content-Type: application/octet-stream", "--input"],
        "",
    ),
};

const GLAB_FLAGS: ApiFlags = ApiFlags {
    method: "-X",
    json_input: &["-H", "Content-Type: application/json", "--input", "-"],
    file_input: (&["--form"], "file=@"),
};

fn api_args(request: &Request, flags: &ApiFlags) -> Vec<String> {
    let mut args = vec!["api".to_owned()];
    match &request.endpoint {
        Endpoint::Rest(path) => {
            args.extend([flags.method.to_owned(), request.method.as_str().to_owned()]);
            if request.paginate {
                args.push("--paginate".to_owned());
            }
            args.push(path.clone());
            if request.body.is_some() {
                args.extend(flags.json_input.iter().map(|arg| (*arg).to_owned()));
            }
            if let Some(file) = &request.file {
                let (before, prefix) = flags.file_input;
                args.extend(before.iter().map(|arg| (*arg).to_owned()));
                args.push(format!("{prefix}{}", file.display()));
            }
        }
        Endpoint::Graphql(vars) => {
            args.push("graphql".to_owned());
            args.extend(graphql_args(vars));
        }
    }
    args
}

/// The `gh api` arguments of a request.
pub fn gh_args(request: &Request) -> Vec<String> {
    api_args(request, &GH_FLAGS)
}

/// The `glab api` arguments of a request.
pub fn glab_args(request: &Request) -> Vec<String> {
    api_args(request, &GLAB_FLAGS)
}

/// The `tea api` arguments of a request, run with the tea login `login`.
///
/// `tea api` takes `--login <name>` to pick the login (it has no host flag), `-X` for the
/// method, `-d @-` for a body read from stdin and `-i` to print the HTTP status line to stderr
/// (tea exits 0 whatever the host answered), and its flags come before the endpoint.
pub fn tea_args(request: &Request, login: &str) -> Vec<String> {
    let mut args = ["api", "--login", login, "-i"].map(str::to_owned).to_vec();
    if let Endpoint::Rest(path) = &request.endpoint {
        args.extend(["-X".to_owned(), request.method.as_str().to_owned()]);
        if request.body.is_some() {
            args.extend(["-H", "Content-Type: application/json", "-d", "@-"].map(str::to_owned));
        }
        args.push(path.clone());
    }
    args
}

/// The arguments that make `tea` print its logins as JSON.
///
/// unverified: `tea logins list --output json` prints an array of objects whose `name` and `url`
/// keys (any case) name the login and its server, and no token.
pub const TEA_LOGINS_LIST: &[&str] = &["logins", "list", "--output", "json"];

/// One login of `tea logins list`.
#[derive(Debug, PartialEq, Eq)]
pub struct TeaLogin {
    pub name: String,
    /// The lowercase host of the login's URL.
    pub host: String,
    pub user: Option<String>,
    pub default: bool,
}

/// The logins in the JSON `tea logins list` printed; none for output that is not a list.
pub fn parse_tea_logins(json: &str) -> Vec<TeaLogin> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let field = |name: &str| {
                item.as_object()?
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value)
            };
            let text = |name: &str| field(name)?.as_str().map(str::to_owned);
            Some(TeaLogin {
                name: text("name")?,
                host: host_of_url(&text("url")?),
                user: text("user"),
                default: field("default") == Some(&Value::Bool(true))
                    || text("default").is_some_and(|flag| flag == "true"),
            })
        })
        .collect()
}

/// The lowercase host (with a port, if any) of a server URL.
fn host_of_url(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split('/').next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    host.to_ascii_lowercase()
}

/// The port of a login's host (`git.example.com:3000`), if it has one.
fn port_of(host: &str) -> Option<&str> {
    let (_, port) = host.rsplit_once(':')?;
    (!port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())).then_some(port)
}

/// The login that reaches `host`: one with exactly that host, else, for a `host` without a port,
/// one on that host name at any port (a git remote's host has no web port); the default one when
/// several do.
pub fn tea_login_for<'a>(logins: &'a [TeaLogin], host: &str) -> Option<&'a TeaLogin> {
    let name_only = port_of(host).is_none();
    logins
        .iter()
        .filter_map(|login| {
            let exact = login.host.eq_ignore_ascii_case(host);
            let same_name = name_only
                && port_of(&login.host).is_some()
                && login
                    .host
                    .rsplit_once(':')
                    .is_some_and(|(name, _)| name.eq_ignore_ascii_case(host));
            (exact || same_name).then_some((login, !exact))
        })
        .min_by_key(|(login, inexact)| (*inexact, !login.default))
        .map(|(login, _)| login)
}

/// The error for an HTTP status of 400 or more in the `HTTP/1.1 <code> <text>` line `tea api -i`
/// prints to stderr; `None` for any other answer.
fn tea_refusal(stderr: &str, body: &[u8]) -> Option<TransportError> {
    let line = stderr.lines().find(|line| line.starts_with("HTTP/"))?;
    let answer = line.split_once(' ')?.1;
    let (code, text) = answer.split_once(' ').unwrap_or((answer, ""));
    let status = code.parse::<u16>().ok().filter(|status| *status >= 400)?;
    let body = String::from_utf8_lossy(body);
    Some(TransportError {
        status: Some(as_seen_by_retries(status, &body)),
        message: format!("tea api failed: {text} (HTTP {status}): {}", body.trim()),
    })
}

/// The tea logins, or `None` when tea is missing or `tea logins list` fails.
pub fn tea_logins() -> Option<Vec<TeaLogin>> {
    Cli::tea()
        .text(TEA_LOGINS_LIST)
        .map(|out| parse_tea_logins(&out))
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
    fn tea_renders_its_api_flags_before_the_endpoint() {
        let get = rest(Method::Get, "repos/o/r/pulls?limit=50&page=1", None);
        assert_eq!(
            tea_args(&get, "codeberg"),
            [
                "api",
                "--login",
                "codeberg",
                "-i",
                "-X",
                "GET",
                "repos/o/r/pulls?limit=50&page=1"
            ]
        );
        let post = rest(Method::Post, "repos/o/r/issues", Some(json!({})));
        assert_eq!(
            tea_args(&post, "work"),
            [
                "api",
                "--login",
                "work",
                "-i",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "-d",
                "@-",
                "repos/o/r/issues"
            ]
        );
    }

    const LOGINS: &str = r#"[
        {"name": "codeberg.org", "url": "https://codeberg.org", "user": "me", "default": true},
        {"Name": "work", "URL": "https://Git.Example.com:3000/sub/", "User": "u2", "Default": false},
        {"name": "work2", "url": "https://git.example.com:3000", "default": "true"},
        {"url": "https://nameless.example.com"}
    ]"#;

    #[test]
    fn tea_logins_name_their_host_and_prefer_the_default() {
        let logins = parse_tea_logins(LOGINS);
        let names: Vec<&str> = logins.iter().map(|login| login.name.as_str()).collect();
        assert_eq!(names, ["codeberg.org", "work", "work2"]);
        assert_eq!(logins[1].host, "git.example.com:3000");
        assert_eq!(logins[0].user.as_deref(), Some("me"));
        let named = |host: &str| tea_login_for(&logins, host).map(|login| login.name.as_str());
        assert_eq!(named("codeberg.org"), Some("codeberg.org"));
        assert_eq!(named("GIT.example.com:3000"), Some("work2"));
        assert_eq!(named("git.example.com"), Some("work2"));
        assert_eq!(named("git.example.com:3001"), None);
        assert_eq!(named("nameless.example.com"), None);
    }

    #[test]
    fn a_login_on_the_exact_host_wins_over_one_with_a_port() {
        let logins = parse_tea_logins(
            r#"[{"name": "ported", "url": "https://git.example.com:3000", "default": true},
                {"name": "plain", "url": "https://git.example.com"}]"#,
        );
        let named = |host: &str| tea_login_for(&logins, host).map(|login| login.name.as_str());
        assert_eq!(named("git.example.com"), Some("plain"));
        assert_eq!(named("git.example.com:3000"), Some("ported"));
    }

    #[test]
    fn output_that_is_not_a_login_list_has_no_logins() {
        for text in ["", "not json", "{}", "[1, \"a\"]"] {
            assert!(parse_tea_logins(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn tea_turns_an_http_status_of_400_or_more_into_an_error_with_that_status() {
        let refused = |stderr: &str| tea_refusal(stderr, br#"{"message":"no"}"#);
        let error = refused("HTTP/1.1 405 Method Not Allowed\nContent-Type: x\n\n").unwrap();
        assert_eq!(error.status, Some(405));
        assert_eq!(
            error.message,
            r#"tea api failed: Method Not Allowed (HTTP 405): {"message":"no"}"#
        );
        assert_eq!(refused("HTTP/2.0 404\n").unwrap().status, Some(404));
        for ok in [
            "HTTP/1.1 200 OK\n\n",
            "HTTP/1.1 399 Odd\n",
            "",
            "warning: slow\n",
        ] {
            assert!(refused(ok).is_none(), "{ok}");
        }
    }

    /// A host that serves `total` numbered items by the `limit` and `page` query parameters.
    struct Numbered {
        total: usize,
        asked: RefCell<Vec<String>>,
    }

    impl Transport for Numbered {
        fn send(&self, request: &Request) -> std::result::Result<Response, TransportError> {
            let Endpoint::Rest(path) = &request.endpoint else {
                unreachable!()
            };
            self.asked.borrow_mut().push(path.clone());
            let (_, query) = path.split_once('?').unwrap();
            let param = |name: &str| -> usize {
                query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
                    .unwrap()
                    .parse()
                    .unwrap()
            };
            let (limit, page) = (param("limit"), param("page"));
            let first = ((page - 1) * limit).min(self.total);
            let last = (first + limit).min(self.total);
            Ok(Response {
                headers: Vec::new(),
                body: Value::Array((first + 1..=last).map(|number| json!(number)).collect()),
            })
        }

        fn render(&self, _request: &Request) -> Vec<String> {
            Vec::new()
        }
    }

    fn read_numbered(total: usize) -> (Vec<Value>, Vec<String>) {
        let source = Numbered {
            total,
            asked: RefCell::new(Vec::new()),
        };
        let transport: &dyn Transport = &source;
        let items = transport
            .items(
                &rest(Method::Get, "repos/o/r/pulls?state=all", None),
                &Pagination::PageNumbers(50),
            )
            .unwrap();
        (items, source.asked.into_inner())
    }

    #[test]
    fn page_numbers_are_followed_until_a_short_page() {
        let (items, asked) = read_numbered(120);
        assert_eq!(
            items,
            (1..=120).map(|number| json!(number)).collect::<Vec<_>>()
        );
        assert_eq!(
            asked,
            [
                "repos/o/r/pulls?state=all&limit=50&page=1",
                "repos/o/r/pulls?state=all&limit=50&page=2",
                "repos/o/r/pulls?state=all&limit=50&page=3"
            ]
        );
    }

    #[test]
    fn a_full_last_page_costs_one_more_read_and_no_items_one_read() {
        let (items, asked) = read_numbered(100);
        assert_eq!((items.len(), asked.len()), (100, 3));
        let (items, asked) = read_numbered(0);
        assert_eq!((items.len(), asked.len()), (0, 1));
        let (items, asked) = read_numbered(50);
        assert_eq!((items.len(), asked.len()), (50, 2));
        let (items, asked) = read_numbered(1);
        assert_eq!((items.len(), asked.len()), (1, 1));
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
