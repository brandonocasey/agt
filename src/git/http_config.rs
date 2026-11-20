//! The `http.*` settings that agt carries over to libgit2: `sslVerify`, `sslCAInfo`,
//! `sslCAPath` and `extraHeader`, each plain or scoped to a URL (`http.<url>.sslVerify`).
//! Other `http.*` keys are not read, except `http.proxy`, which libgit2 reads itself.

use std::path::PathBuf;

use crate::core::error::{Error, Result, fail};

/// What the `http.*` config asks for one HTTP or HTTPS URL.
pub(super) struct Http {
    pub verify: bool,
    pub ca_file: Option<PathBuf>,
    pub ca_dir: Option<PathBuf>,
    pub headers: Vec<String>,
}

/// A URL split into the parts that `http.<url>.*` matching compares.
struct Parts {
    scheme: String,
    user: Option<String>,
    host: String,
    port: u16,
    path: String,
}

/// How closely a `http.<url>` section matches a URL: the longer path, then a matching user.
type Specificity = (usize, bool);

fn parts(url: &str) -> Option<Parts> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let (authority, path) = rest.find('/').map_or((rest, ""), |at| rest.split_at(at));
    let (user, host_port) = match authority.rsplit_once('@') {
        Some((user, host_port)) => (Some(user.to_owned()), host_port),
        None => (None, authority),
    };
    let default_port = if scheme == "https" { 443 } else { 80 };
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) if !port.contains(']') => (host, port.parse().ok()?),
        _ => (host_port, default_port),
    };
    Some(Parts {
        scheme,
        user,
        host: host.to_ascii_lowercase(),
        port,
        path: path.trim_end_matches('/').to_owned(),
    })
}

/// How well the section `scope` matches `target`; `None` when it names another URL.
fn specificity(scope: &Parts, target: &Parts) -> Option<Specificity> {
    let same_place = scope.scheme == target.scheme
        && scope.host == target.host
        && scope.port == target.port
        && scope
            .user
            .as_ref()
            .is_none_or(|user| Some(user) == target.user.as_ref());
    let below = target.path == scope.path
        || target
            .path
            .strip_prefix(&scope.path)
            .is_some_and(|rest| rest.starts_with('/'));
    (same_place && below).then_some((scope.path.len(), scope.user.is_some()))
}

/// The config prefixes (`http`, or `http.<url>`) that apply to `target`, least specific first.
/// Within one specificity, the order of the config file is kept.
fn scopes(config: &git2::Config, target: &Parts) -> Result<Vec<String>> {
    let mut found: Vec<(Option<Specificity>, String)> = vec![(None, "http".to_owned())];
    let mut entries = config.entries(Some("^http\\."))?;
    while let Some(entry) = entries.next() {
        let name = entry?.name().unwrap_or_default().to_owned();
        let Some((prefix, _)) = name.rsplit_once('.') else {
            continue;
        };
        if prefix == "http" || found.iter().any(|(_, known)| known == prefix) {
            continue;
        }
        let url = parts(&prefix["http.".len()..]);
        if let Some(rank) = url.and_then(|url| specificity(&url, target)) {
            found.push((Some(rank), prefix.to_owned()));
        }
    }
    found.sort_by_key(|(rank, _)| *rank);
    Ok(found.into_iter().map(|(_, prefix)| prefix).collect())
}

/// The value of `key` in the most specific scope that sets it.
fn scoped<T>(
    config: &git2::Config,
    scopes: &[String],
    key: &str,
    read: impl Fn(&git2::Config, &str) -> std::result::Result<T, git2::Error>,
) -> Result<Option<T>> {
    for scope in scopes.iter().rev() {
        let name = format!("{scope}.{key}");
        if config.get_entry(&name).is_ok() {
            return read(config, &name)
                .map(Some)
                .map_err(|error| Error(format!("bad value for '{name}': {}", error.message())));
        }
    }
    Ok(None)
}

/// The `http.*` settings for `url`; the defaults for a URL that is not HTTP or HTTPS.
pub(super) fn settings(config: &git2::Config, url: &str) -> Result<Http> {
    let mut http = Http {
        verify: true,
        ca_file: None,
        ca_dir: None,
        headers: Vec::new(),
    };
    let Some(target) =
        parts(url).filter(|target| matches!(target.scheme.as_str(), "http" | "https"))
    else {
        return Ok(http);
    };
    let scopes = scopes(config, &target)?;
    let verify = scoped(config, &scopes, "sslverify", git2::Config::get_bool)?;
    http.verify = verify.unwrap_or(true);
    http.ca_file = scoped(config, &scopes, "sslcainfo", git2::Config::get_path)?;
    http.ca_dir = scoped(config, &scopes, "sslcapath", git2::Config::get_path)?;
    for scope in &scopes {
        let mut values = config.multivar(&format!("{scope}.extraheader"), None)?;
        while let Some(entry) = values.next() {
            let value = entry?.value().unwrap_or_default().to_owned();
            if value.is_empty() {
                http.headers.clear();
            } else if value.contains(['\r', '\n']) {
                return fail(format!("http.extraHeader '{value}' has a line break"));
            } else {
                http.headers.push(value);
            }
        }
    }
    Ok(http)
}
