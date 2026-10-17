//! HTTP with a token, for services that have no CLI. Not used by GitHub or GitLab; it is the
//! seam a service without one connects through.

use std::ffi::OsString;
use std::fmt;
use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use super::{Response, Transport, TransportError};
use crate::error::{Error, Result, ctx, fail};
use crate::service::{Endpoint, Request};

/// Where a token transport gets its token.
pub enum Auth {
    /// An environment variable.
    Env(String),
    /// What `git credential fill` knows for the host, so the login of `git push` is reused.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "no service uses it yet; a unit test builds it")
    )]
    GitCredential,
    /// The first line a command prints (`pass show x`, `op read ...`), run without a shell.
    Command(Vec<String>),
}

/// A token that never prints.
struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

impl Auth {
    fn token(&self, host: &str) -> Result<Secret> {
        self.token_with(host, &[])
    }

    /// [`Auth::token`] with extra environment for `git credential fill`.
    fn token_with(&self, host: &str, git_env: &[(&str, &str)]) -> Result<Secret> {
        match self {
            Self::Env(var) => match std::env::var(var) {
                Ok(token) if !token.is_empty() => Ok(Secret(token)),
                _ => fail(format!("{var} is not set")),
            },
            Self::GitCredential => credential_fill(host, git_env),
            Self::Command(argv) => {
                let (program, args) = argv
                    .split_first()
                    .ok_or_else(|| Error("the token command is empty".into()))?;
                let output = Command::new(program)
                    .args(args)
                    .stdin(Stdio::null())
                    .output()
                    .map_err(ctx(format!("run {program}")))?;
                let text = String::from_utf8_lossy(&output.stdout);
                match text.lines().next().filter(|token| !token.is_empty()) {
                    Some(token) if output.status.success() => Ok(Secret(token.to_owned())),
                    _ => fail(format!("{program} printed no token")),
                }
            }
        }
    }
}

/// Runs `git credential fill` for `host`, which reads the request from stdin and so never
/// shows the secret in argv. `env` is for the child only.
fn credential_fill(host: &str, env: &[(&str, &str)]) -> Result<Secret> {
    let mut child = Command::new("git")
        .args(["credential", "fill"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(ctx("run git credential fill"))?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let request = format!("protocol=https\nhost={host}\n\n");
    let writer = std::thread::spawn(move || stdin.write_all(request.as_bytes()));
    let output = child
        .wait_with_output()
        .map_err(ctx("run git credential fill"))?;
    let _ = writer.join();
    if !output.status.success() {
        return fail(format!(
            "git has no credential for {host}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("password="))
        .filter(|token| !token.is_empty())
        .map(|token| Secret(token.to_owned()))
        .ok_or_else(|| Error(format!("git has no credential for {host}")))
}

/// HTTP through `curl`, with a token. The URL, the token header and the body go to curl as
/// a config on stdin (`-K -`), never as arguments, which `ps` would show.
pub struct TokenTransport {
    program: OsString,
    base_url: String,
    host: String,
    auth: Auth,
    /// `Bearer`, `token`: the word before the token in the `Authorization` header.
    scheme: &'static str,
}

impl TokenTransport {
    pub fn new(base_url: &str, host: &str, auth: Auth, scheme: &'static str) -> Self {
        Self {
            program: std::env::var_os("AGENT_REVIEW_CURL_BIN").unwrap_or_else(|| "curl".into()),
            base_url: base_url.trim_end_matches('/').to_owned(),
            host: host.to_owned(),
            auth,
            scheme,
        }
    }

    /// The URL for `path`. An absolute URL (a next-page link) is only followed when it is https
    /// on the host of the base URL, so the token is never sent anywhere else.
    fn url(&self, path: &str) -> std::result::Result<String, TransportError> {
        if path.starts_with("https://") || path.starts_with("http://") {
            let authority = |url: &str| {
                url.split_once("://")
                    .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default())
                    .map(str::to_ascii_lowercase)
            };
            let same_host = path.starts_with("https://")
                && authority(path).is_some_and(|host| {
                    !host.contains('@') && Some(host) == authority(&self.base_url)
                });
            if !same_host {
                return Err(TransportError::new(format!(
                    "refusing to send the token to {path}: not https on {}",
                    self.base_url
                )));
            }
            Ok(path.to_owned())
        } else {
            Ok(format!(
                "{}/{}",
                self.base_url,
                path.trim_start_matches('/')
            ))
        }
    }

    /// The curl config for `request`.
    fn config(
        &self,
        request: &Request,
        token: &Secret,
    ) -> std::result::Result<String, TransportError> {
        let (path, body) = match &request.endpoint {
            Endpoint::Rest(path) => (path.clone(), request.body.clone()),
            Endpoint::Graphql(vars) => {
                let query = vars
                    .iter()
                    .find(|var| var.name == "query")
                    .map_or("", |var| var.value.as_str());
                let variables: serde_json::Map<String, Value> = vars
                    .iter()
                    .filter(|var| var.name != "query")
                    .map(|var| {
                        let value = if var.typed {
                            serde_json::from_str(&var.value).unwrap_or(json!(var.value))
                        } else {
                            json!(var.value)
                        };
                        (var.name.clone(), value)
                    })
                    .collect();
                (
                    "graphql".to_owned(),
                    Some(json!({ "query": query, "variables": variables })),
                )
            }
        };
        let method = if matches!(request.endpoint, Endpoint::Graphql(_)) {
            "POST"
        } else {
            request.method.as_str()
        };
        let mut config = format!(
            "url = {}\nrequest = {}\nheader = {}\nheader = \"Accept: application/json\"\n",
            curl_quote(&self.url(&path)?),
            curl_quote(method),
            curl_quote(&format!("Authorization: {} {}", self.scheme, token.0)),
        );
        if let Some(body) = body {
            config.push_str("header = \"Content-Type: application/json\"\n");
            config.push_str(&format!("data-raw = {}\n", curl_quote(&body.to_string())));
        }
        Ok(config)
    }
}

impl Transport for TokenTransport {
    fn send(&self, request: &Request) -> std::result::Result<Response, TransportError> {
        let token = self.auth.token(&self.host)?;
        let config = self.config(request, &token)?;
        let mut child = Command::new(&self.program)
            // `-q` first, or curl reads ~/.curlrc, which could redirect the request or log it.
            .args([
                "-q",
                "--silent",
                "--show-error",
                "--include",
                "--config",
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(ctx("run curl"))?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let writer = std::thread::spawn(move || stdin.write_all(config.as_bytes()));
        let output = child.wait_with_output().map_err(ctx("run curl"))?;
        let _ = writer.join();
        if !output.status.success() {
            return Err(TransportError::new(format!(
                "curl failed: {}",
                redact(String::from_utf8_lossy(&output.stderr).trim(), &token)
            )));
        }
        let Parsed {
            status,
            headers,
            body,
        } = parse_http(&output.stdout)?;
        let text = String::from_utf8_lossy(body);
        if !(200..300).contains(&status) {
            return Err(TransportError {
                status: Some(super::as_seen_by_retries(status, &text)),
                message: format!(
                    "{} {} failed: HTTP {status}: {}",
                    request.method.as_str(),
                    self.host,
                    redact(text.trim(), &token)
                ),
            });
        }
        let body = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).map_err(ctx("parse reply"))?
        };
        Ok(Response {
            status,
            headers,
            body,
        })
    }

    fn render(&self, request: &Request) -> Vec<String> {
        let path = match &request.endpoint {
            Endpoint::Rest(path) => path.as_str(),
            Endpoint::Graphql(_) => "graphql",
        };
        let url = self.url(path).unwrap_or_else(|error| error.message);
        vec![request.method.as_str().to_owned(), url]
    }
}

/// `text` without the token, in case a tool or server echoes it back.
fn redact(text: &str, token: &Secret) -> String {
    if token.0.is_empty() {
        text.to_owned()
    } else {
        text.replace(&token.0, "[redacted]")
    }
}

/// A parsed `curl --include` reply.
struct Parsed<'a> {
    status: u16,
    headers: Vec<(String, String)>,
    body: &'a [u8],
}

/// A string for a curl config file: quoted, with its escapes.
fn curl_quote(text: &str) -> String {
    let mut quoted = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// The final status, headers (lowercase names) and body of `curl --include` output, which
/// starts with a block for each interim (1xx) reply and for a proxy's `200 Connection
/// established`.
fn parse_http(raw: &[u8]) -> Result<Parsed<'_>> {
    let mut rest = raw;
    loop {
        let (head, body) =
            split_head(rest).ok_or_else(|| Error("curl printed no headers".into()))?;
        let text = String::from_utf8_lossy(head);
        let mut lines = text.lines();
        let status_line = lines.next().unwrap_or_default();
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse().ok())
            .ok_or_else(|| Error("curl printed no status".into()))?;
        let tunnel = status == 200
            && status_line
                .to_ascii_lowercase()
                .contains("connection established");
        if (100..200).contains(&status) || tunnel {
            rest = body;
            continue;
        }
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        return Ok(Parsed {
            status,
            headers,
            body,
        });
    }
}

fn split_head(raw: &[u8]) -> Option<(&[u8], &[u8])> {
    [&b"\r\n\r\n"[..], b"\n\n"].iter().find_map(|gap| {
        let at = raw.windows(gap.len()).position(|window| window == *gap)?;
        Some((&raw[..at], &raw[at + gap.len()..]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::Method;

    #[test]
    fn http_output_splits_into_status_headers_and_body() {
        let raw = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/2 201\r\nLink: <u>; rel=\"next\"\r\nX-A: b\r\n\r\n{\"id\":1}";
        let Parsed {
            status,
            headers,
            body,
        } = parse_http(raw).unwrap();
        assert_eq!(status, 201);
        assert!(headers.contains(&("link".to_owned(), "<u>; rel=\"next\"".to_owned())));
        assert_eq!(body, b"{\"id\":1}");
        assert!(parse_http(b"garbage").is_err());
    }

    #[test]
    fn proxy_tunnel_and_interim_blocks_are_skipped() {
        let raw = b"HTTP/1.1 200 Connection established\r\nProxy-Agent: p\r\n\r\nHTTP/1.1 103 Early Hints\r\nlink: <x>\r\n\r\nHTTP/2 404\r\nx-a: b\r\n\r\nnope";
        let parsed = parse_http(raw).unwrap();
        assert_eq!(parsed.status, 404);
        assert_eq!(parsed.body, b"nope");
        assert_eq!(parsed.headers, [("x-a".to_owned(), "b".to_owned())]);
    }

    fn transport(base: &str) -> TokenTransport {
        let auth = Auth::Command(vec!["echo".into(), "t".into()]);
        TokenTransport::new(base, "h", auth, "Bearer")
    }

    #[test]
    fn the_token_only_goes_to_https_on_the_base_host() {
        let transport = transport("https://h/api/v1");
        for ok in ["https://h/api/v1/x?page=2", "https://H/x"] {
            assert_eq!(transport.url(ok).unwrap(), ok);
        }
        assert_eq!(transport.url("/x").unwrap(), "https://h/api/v1/x");
        for bad in [
            "http://h/x",
            "https://evil.example/x",
            "https://h.evil.example/x",
            "https://h@evil.example/x",
            "https://h:8443/x",
        ] {
            assert!(transport.url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_token_in_error_text_is_redacted() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let curl = dir.path().join("curl");
        std::fs::write(
            &curl,
            "#!/bin/sh\ncat > /dev/null\necho \"curl: bad header Bearer s3cret-token\" >&2\nexit 2\n",
        )
        .unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut transport = transport("https://h");
        transport.auth = Auth::Command(vec!["echo".into(), "s3cret-token".into()]);
        transport.program = curl.into_os_string();
        let error = transport
            .send(&Request::rest(Method::Get, "x", None))
            .err()
            .expect("curl failed");
        assert!(!error.message.contains("s3cret-token"), "{error}");
        assert!(error.message.contains("[redacted]"), "{error}");
    }

    #[test]
    fn curl_config_strings_are_escaped() {
        assert_eq!(curl_quote("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
    }

    #[test]
    fn an_unset_token_variable_is_an_error() {
        let error = Auth::Env("AGENT_REVIEW_SURELY_UNSET_TOKEN".into())
            .token("h")
            .unwrap_err();
        assert!(error.to_string().contains("is not set"));
    }

    #[test]
    fn secrets_do_not_print() {
        assert_eq!(format!("{:?}", Secret("hunter2".into())), "Secret(..)");
    }

    #[test]
    fn git_credential_fill_returns_the_helper_password() {
        let secret = credential_fill(
            "git.example.com",
            &[
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "credential.helper"),
                (
                    "GIT_CONFIG_VALUE_0",
                    "!f() { echo username=u; echo password=tok-123; }; f",
                ),
                ("GIT_CONFIG_GLOBAL", "/dev/null"),
                ("GIT_CONFIG_SYSTEM", "/dev/null"),
            ],
        )
        .unwrap();
        assert_eq!(secret.0, "tok-123");
    }

    #[test]
    fn the_git_credential_auth_asks_git_for_the_host() {
        let secret = Auth::GitCredential
            .token_with(
                "git.example.com",
                &[
                    ("GIT_CONFIG_COUNT", "1"),
                    ("GIT_CONFIG_KEY_0", "credential.helper"),
                    (
                        "GIT_CONFIG_VALUE_0",
                        "!f() { echo username=u; echo password=from-git; }; f",
                    ),
                    ("GIT_CONFIG_GLOBAL", "/dev/null"),
                    ("GIT_CONFIG_SYSTEM", "/dev/null"),
                ],
            )
            .unwrap();
        assert_eq!(secret.0, "from-git");
    }

    #[test]
    fn the_token_reaches_curl_on_stdin_and_never_in_argv() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let curl = dir.path().join("curl");
        std::fs::write(
            &curl,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{0}/argv'\ncat > '{0}/stdin'\n\
                 printf 'HTTP/2 201\\r\\nlink: <https://h/a?page=2>; rel=\"next\"\\r\\n\\r\\n{{\"id\":7}}'\n",
                dir.path().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let auth = Auth::Command(vec!["echo".into(), "s3cret-token".into()]);
        let mut transport = TokenTransport::new("https://h/api/v1/", "h", auth, "Bearer");
        transport.program = curl.into_os_string();
        let request = Request::rest(
            Method::Post,
            "repos/o/r/issues/1/comments",
            Some(json!({"body": "a \"q\"\nb"})),
        );
        let response = transport.send(&request).unwrap();
        assert_eq!(response.status, 201);
        assert_eq!(response.body, json!({"id": 7}));
        assert_eq!(
            response.header("link"),
            Some("<https://h/a?page=2>; rel=\"next\"")
        );
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert_eq!(argv.lines().next(), Some("-q"), "{argv}");
        assert!(!argv.contains("s3cret-token"), "{argv}");
        assert!(!argv.contains("issues"), "{argv}");
        let stdin = std::fs::read_to_string(dir.path().join("stdin")).unwrap();
        assert!(
            stdin.contains("Authorization: Bearer s3cret-token"),
            "{stdin}"
        );
        assert!(
            stdin.contains("url = \"https://h/api/v1/repos/o/r/issues/1/comments\""),
            "{stdin}"
        );
        assert!(stdin.contains("request = \"POST\""), "{stdin}");
        assert!(
            stdin.contains(r#"data-raw = "{\"body\":\"a \\\"q\\\"\\nb\"}""#),
            "{stdin}"
        );
    }

    #[test]
    fn a_rate_limited_403_is_not_a_refusal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let curl = dir.path().join("curl");
        std::fs::write(
            &curl,
            "#!/bin/sh\ncat > /dev/null\nprintf 'HTTP/2 403\\r\\n\\r\\n{\"message\":\"API rate limit exceeded\"}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut transport = transport("https://h");
        transport.program = curl.into_os_string();
        let error = transport
            .send(&Request::rest(Method::Get, "x", None))
            .err()
            .expect("403 is an error");
        assert!(!error.refused(), "{error}");
    }

    #[test]
    fn a_refusal_carries_its_status() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let curl = dir.path().join("curl");
        std::fs::write(
            &curl,
            "#!/bin/sh\ncat > /dev/null\nprintf 'HTTP/2 403\\r\\n\\r\\n{\"message\":\"no\"}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let auth = Auth::Command(vec!["echo".into(), "t".into()]);
        let mut transport = TokenTransport::new("https://h", "h", auth, "Bearer");
        transport.program = curl.into_os_string();
        let error = transport
            .send(&Request::rest(Method::Get, "x", None))
            .err()
            .expect("403 is an error");
        assert_eq!(error.status, Some(403));
        assert!(error.refused());
    }
}
