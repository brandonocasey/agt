//! The PR, MR and pipeline arguments of `agt ci wait`.

use std::sync::LazyLock;

use regex::Regex;

use crate::core::error::{Error, Result, fail};
use crate::forge::transport::Cli;
use crate::git::Repo;

pub static GITLAB_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https?://([^/]+)/(.+?)/-/(merge_requests|pipelines)/(\d+)")
        .expect("valid pattern")
});
static GITHUB_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https?://([^/]+)/([^/]+/[^/]+)/pull/(\d+)").expect("valid pattern")
});
static GITHUB_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^/\s#!@]+/[^/\s#!@]+)#(\d+)$").expect("valid pattern"));
static GITLAB_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^\s#!@]+/[^\s#!@]+)!(\d+)$").expect("valid pattern"));
static LOCAL_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([#!]?)(\d+)$").expect("valid pattern"));
static SHA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-fA-F]{4,64}$").expect("valid pattern"));

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    Github,
    Gitlab,
}

/// A target as written; `host` and `repo` are `None` when the argument leaves them to the
/// current repository.
#[derive(Debug)]
pub struct Target {
    pub platform: Platform,
    pub host: Option<String>,
    pub repo: Option<String>,
    pub pipeline: bool,
    pub number: u64,
    pub sha: Option<String>,
    /// `--sha`: the commit may not show on the host yet.
    pub awaiting: bool,
}

/// A target with its host and repository known.
#[derive(Clone)]
pub struct Resolved {
    pub platform: Platform,
    pub host: String,
    pub repo: String,
    pub pipeline: bool,
    pub number: u64,
    pub sha: Option<String>,
    pub awaiting: bool,
    /// The name of the target in events: `owner/repo#1`, `group/project!1` or
    /// `group/project/-/pipelines/1`.
    pub key: String,
}

fn number(text: &str) -> Result<u64> {
    match text.parse() {
        Ok(number) if number > 0 => Ok(number),
        _ => fail(format!("invalid target number: {text}")),
    }
}

pub fn parse(arg: &str) -> Result<Target> {
    let (value, sha) = match arg.rsplit_once('@') {
        Some((value, sha)) if SHA.is_match(sha) => (value, Some(sha.to_owned())),
        _ => (arg, None),
    };
    let target = |platform, host: Option<&str>, repo: Option<&str>, pipeline, count: &str| {
        Ok(Target {
            platform,
            host: host.map(str::to_owned),
            repo: repo.map(str::to_owned),
            pipeline,
            number: number(count)?,
            sha: sha.clone(),
            awaiting: false,
        })
    };
    if let Some(found) = GITLAB_URL.captures(value) {
        let pipeline = &found[3] == "pipelines";
        return target(
            Platform::Gitlab,
            Some(&found[1]),
            Some(&found[2]),
            pipeline,
            &found[4],
        );
    }
    if let Some(found) = GITHUB_URL.captures(value) {
        return target(
            Platform::Github,
            Some(&found[1]),
            Some(&found[2]),
            false,
            &found[3],
        );
    }
    if let Some(found) = GITHUB_REF.captures(value) {
        return target(Platform::Github, None, Some(&found[1]), false, &found[2]);
    }
    if let Some(found) = GITLAB_REF.captures(value) {
        return target(Platform::Gitlab, None, Some(&found[1]), false, &found[2]);
    }
    if let Some(found) = LOCAL_REF.captures(value) {
        return match &found[1] {
            "#" => target(Platform::Github, None, None, false, &found[2]),
            sigil => target(Platform::Gitlab, None, None, sigil.is_empty(), &found[2]),
        };
    }
    fail(format!(
        "invalid target: {arg} (expected OWNER/REPO#PR, GROUP/PROJECT!IID, #PR, !IID, a pipeline id or a URL)"
    ))
}

/// The GitLab host: `GITLAB_HOST`, else what `glab config get host` prints.
fn gitlab_host() -> Result<String> {
    let configured = std::env::var("GITLAB_HOST")
        .ok()
        .filter(|host| !host.is_empty())
        .or_else(|| {
            Cli::glab_configured()
                .text(&["config", "get", "host"])
                .filter(|host| !host.is_empty())
        });
    configured
        .ok_or_else(|| Error("no GitLab host: set GITLAB_HOST or configure glab's host".into()))
}

fn origin() -> Result<(String, String)> {
    Repo::discover()?.origin_location()?.ok_or_else(|| {
        Error("the current repository has no origin remote to name the target".into())
    })
}

pub fn resolve(target: Target) -> Result<Resolved> {
    let (host, repo) = match (target.host, target.repo) {
        (Some(host), Some(repo)) => (host, repo),
        (None, Some(repo)) => {
            let host = match target.platform {
                Platform::Github => std::env::var("GH_HOST")
                    .ok()
                    .filter(|host| !host.is_empty())
                    .unwrap_or_else(|| "github.com".to_owned()),
                Platform::Gitlab => gitlab_host()?,
            };
            (host, repo)
        }
        (_, None) => origin()?,
    };
    let number = target.number;
    let key = match (target.platform, target.pipeline) {
        (Platform::Github, _) => format!("{repo}#{number}"),
        (Platform::Gitlab, false) => format!("{repo}!{number}"),
        (Platform::Gitlab, true) => format!("{repo}/-/pipelines/{number}"),
    };
    Ok(Resolved {
        platform: target.platform,
        host,
        repo,
        pipeline: target.pipeline,
        number,
        sha: target.sha,
        awaiting: target.awaiting,
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_target_form() {
        let github = parse("owner/repo#42@abc123").unwrap();
        assert_eq!(github.platform, Platform::Github);
        assert_eq!(github.repo.as_deref(), Some("owner/repo"));
        assert_eq!((github.number, github.sha.as_deref()), (42, Some("abc123")));
        let gitlab = parse("group/sub/project!7").unwrap();
        assert_eq!(gitlab.platform, Platform::Gitlab);
        assert_eq!(gitlab.repo.as_deref(), Some("group/sub/project"));
        assert_eq!(gitlab.sha, None);
        let url = parse("https://gitlab.example.com/g/s/p/-/merge_requests/3@abc123").unwrap();
        assert_eq!(url.host.as_deref(), Some("gitlab.example.com"));
        assert_eq!((url.pipeline, url.number), (false, 3));
        assert!(
            parse("https://gitlab.example.com/g/p/-/pipelines/9")
                .unwrap()
                .pipeline
        );
        let pull = parse("https://github.com/o/r/pull/5").unwrap();
        assert_eq!(pull.platform, Platform::Github);
        assert_eq!(pull.repo.as_deref(), Some("o/r"));
        let local = parse("#8").unwrap();
        assert_eq!((local.platform, local.repo), (Platform::Github, None));
        let local = parse("!8").unwrap();
        assert_eq!((local.platform, local.pipeline), (Platform::Gitlab, false));
        let local = parse("8").unwrap();
        assert_eq!((local.platform, local.pipeline), (Platform::Gitlab, true));
    }

    #[test]
    fn rejects_malformed_targets() {
        for bad in [
            "project!7@abc123",
            "group/project!0",
            "o/r#x",
            "nope",
            "o/r#1@",
            "!0",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
