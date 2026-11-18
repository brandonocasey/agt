//! The PR, MR, pipeline, commit and Actions run arguments of `agt ci wait`.

use std::sync::LazyLock;

use regex::Regex;

use crate::core::config::{Config, Kind, forge_of};
use crate::core::error::{Error, Result, fail};
use crate::forge::landed::Source;
use crate::forge::select::require_tracker;
use crate::forge::{self, PrRef, github, gitlab, service};
use crate::git::Repo;
use crate::store::model::Forge;

static GITHUB_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^/\s#!@]+/[^/\s#!@]+)#(\d+)$").expect("valid pattern"));
static GITLAB_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^\s#!@]+/[^\s#!@]+)!(\d+)$").expect("valid pattern"));
static LOCAL_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([#!]?)(\d+)$").expect("valid pattern"));
static REPO_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[^\s#!@/:]+(/[^\s#!@/:]+)+$").expect("valid pattern"));
// Anything else after the run id names more than one run or job.
static ACTIONS_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^https?://([^/\s]+)/([^\s?#]+?)/actions/runs/(\d+)(?:/job/(\d+)|(/jobs/\d+))?(?:/attempts/\d+)?/?(?:[?#]\S*)?$",
    )
    .expect("valid pattern")
});
static SHA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-fA-F]{4,64}$").expect("valid pattern"));

/// A target as written; `host` and `repo` are `None` when the argument leaves them to the
/// current repository.
#[derive(Clone, Debug)]
pub struct Target {
    pub platform: Forge,
    pub host: Option<String>,
    pub repo: Option<String>,
    /// A GitLab pipeline or a GitHub Actions run.
    pub pipeline: bool,
    /// The PR, MR, pipeline or run number; 0 for a commit.
    pub number: u64,
    pub sha: Option<String>,
    /// `@SHA` on a PR or MR: the commit may not show on the host yet.
    pub awaiting: bool,
    /// The target is the commit `sha` itself, not a PR, MR or pipeline.
    pub commit: bool,
}

/// A GitHub or Forgejo Actions URL: a run, or one job of it.
pub struct ActionsUrl {
    pub forge: Forge,
    pub host: String,
    pub repo: String,
    pub run: u64,
    /// The job of a GitHub URL; Forgejo's `/jobs/` URLs name none here.
    pub job: Option<u64>,
}

/// The run and job an Actions URL names, else `None`. An unknown host is GitHub for a `/job/` URL
/// or the default host, else Forgejo.
pub fn actions_url(value: &str) -> Option<ActionsUrl> {
    let found = ACTIONS_URL.captures(value)?;
    let host = found[1].to_ascii_lowercase();
    let job = found
        .get(4)
        .map(|job| job.as_str().parse::<u64>())
        .transpose()
        .ok()?;
    let run: u64 = found[3].parse().ok()?;
    if run == 0 || job == Some(0) {
        return None;
    }
    let forge = match forge_of(&host) {
        Some(Forge::Gitlab) => return None,
        Some(forge) => forge,
        None if job.is_some() || host == github::default_host() => Forge::Github,
        None => Forge::Forgejo,
    };
    if found.get(5).is_some() && forge != Forge::Forgejo {
        return None;
    }
    Some(ActionsUrl {
        forge,
        host,
        repo: found[2].to_owned(),
        run,
        job,
    })
}

/// A target with its host and repository known.
#[derive(Clone)]
pub struct Resolved {
    pub target: Target,
    pub host: String,
    pub repo: String,
    /// The name of the target in events: `owner/repo#1`, `group/project!1`,
    /// `group/project/-/pipelines/1`, `owner/repo@sha` or `owner/repo/actions/runs/1`.
    pub key: String,
    /// The job or check names to wait for; empty waits for every checked job.
    pub jobs: Vec<String>,
}

fn number(text: &str) -> Result<u64> {
    match text.parse() {
        Ok(number) if number > 0 => Ok(number),
        _ => fail(format!("invalid target number: {text}")),
    }
}

pub fn parse(arg: &str) -> Result<Target> {
    parse_as(arg, true)
}

/// Like [`parse`]; `bare_pipeline` is whether a bare id is a GitLab pipeline, not an MR.
pub fn parse_as(arg: &str, bare_pipeline: bool) -> Result<Target> {
    let (value, sha) = match arg.rsplit_once('@') {
        Some((value, sha)) if SHA.is_match(sha) => (value, Some(sha)),
        _ => (arg, None),
    };
    if let Some((found, count)) = numbered_target(value, bare_pipeline) {
        if sha.is_some() && found.platform == Forge::Github && found.pipeline {
            return fail("an Actions run takes no @SHA");
        }
        return found.numbered(&count, sha);
    }
    if let Some(sha) = sha
        && (value.is_empty() || REPO_PATH.is_match(value))
    {
        return Ok(commit_target(value, sha));
    }
    fail(format!(
        "invalid target: {arg} (expected OWNER/REPO#PR, GROUP/PROJECT!IID, OWNER/REPO@SHA, @SHA, #PR, !IID, a pipeline id or a URL)"
    ))
}

impl Target {
    /// A target on `platform` that has no number or commit yet.
    fn at(platform: Forge, host: Option<&str>, repo: Option<&str>, pipeline: bool) -> Self {
        Self {
            platform,
            host: host.map(str::to_owned),
            repo: repo.map(str::to_owned),
            pipeline,
            number: 0,
            sha: None,
            awaiting: false,
            commit: false,
        }
    }

    /// The target with its number (`count`, as written) and the commit it is pinned to.
    fn numbered(mut self, count: &str, sha: Option<&str>) -> Result<Self> {
        self.number = number(count)?;
        self.sha = sha.map(str::to_owned);
        self.awaiting = sha.is_some() && !self.pipeline;
        Ok(self)
    }
}

/// The PR, MR, pipeline or run `value` names, with its number as written.
fn numbered_target(value: &str, bare_pipeline: bool) -> Option<(Target, String)> {
    actions_run(value)
        .or_else(|| url_target(value))
        .or_else(|| repo_ref(value))
        .or_else(|| local_ref(value, bare_pipeline))
}

fn actions_run(value: &str) -> Option<(Target, String)> {
    let run = actions_url(value).filter(|run| run.forge == Forge::Github)?;
    let found = Target::at(Forge::Github, Some(&run.host), Some(&run.repo), true);
    Some((found, run.run.to_string()))
}

fn url_target(value: &str) -> Option<(Target, String)> {
    let PrRef {
        number,
        location: Some((host, repo)),
        forge: Some(platform),
        pipeline,
    } = forge::parse_ref(value, true).ok()?
    else {
        return None;
    };
    let found = Target::at(platform, Some(&host), Some(&repo), pipeline);
    Some((found, number.to_string()))
}

/// `OWNER/REPO#PR` or `GROUP/PROJECT!IID`.
fn repo_ref(value: &str) -> Option<(Target, String)> {
    if let Some(found) = GITHUB_REF.captures(value) {
        let target = Target::at(Forge::Github, None, Some(&found[1]), false);
        return Some((target, found[2].to_owned()));
    }
    let found = GITLAB_REF.captures(value)?;
    let target = Target::at(Forge::Gitlab, None, Some(&found[1]), false);
    Some((target, found[2].to_owned()))
}

/// `#PR`, `!IID` or a bare id, in the current repository.
fn local_ref(value: &str, bare_pipeline: bool) -> Option<(Target, String)> {
    let found = LOCAL_REF.captures(value)?;
    let target = match &found[1] {
        "#" => Target::at(Forge::Github, None, None, false),
        sigil => Target::at(Forge::Gitlab, None, None, bare_pipeline && sigil.is_empty()),
    };
    Some((target, found[2].to_owned()))
}

/// The commit `sha` of `repo`. A path of three or more parts can only be GitLab's; [`resolve`]
/// settles two.
fn commit_target(repo: &str, sha: &str) -> Target {
    let platform = if repo.matches('/').count() >= 2 {
        Forge::Gitlab
    } else {
        Forge::Github
    };
    Target {
        platform,
        host: None,
        repo: (!repo.is_empty()).then(|| repo.to_owned()),
        pipeline: false,
        number: 0,
        sha: Some(sha.to_owned()),
        awaiting: false,
        commit: true,
    }
}

fn gitlab_host() -> Result<String> {
    gitlab::configured_host()
        .ok_or_else(|| Error("no GitLab host: set GITLAB_HOST or configure glab's host".into()))
}

/// Whether a target written for `platform` can name a pull request on `found`: a bare `#N`
/// parses as a GitHub reference, but means a PR of a Forgejo tracker just as well.
fn names_same_forge(platform: Forge, found: Forge) -> bool {
    platform == found || (platform == Forge::Github && found == Forge::Forgejo)
}

/// The host, repository and forge of the tracker `agt pr` picks (see `--remote`), which must be
/// on the forge the target names, when it names one.
pub fn tracker(platform: Option<Forge>, remote: Option<&str>) -> Result<(String, String, Forge)> {
    let repo = Repo::discover()?;
    let config = Config::load_for_repo(&repo)?;
    let source = Source::of(&require_tracker(
        &repo,
        &config,
        Kind::PullRequests,
        remote,
        None,
    )?)?;
    if let Some(platform) = platform
        && !names_same_forge(platform, source.forge)
    {
        let hint = if source.forge == Forge::Forgejo {
            "; name a Forgejo pull request as #PR, for example #7"
        } else {
            ""
        };
        return fail(format!(
            "{}/{} is {}, but the target is a {} reference{hint}",
            source.host,
            source.repo,
            service::kind(source.forge).name,
            service::kind(platform).name
        ));
    }
    Ok((source.host, source.repo, source.forge))
}

/// A tracker of the current repository for `repo` wins over `guess`'s default host.
fn commit_host(guess: Forge, repo: &str) -> Result<(String, Forge)> {
    if let Ok(checkout) = Repo::discover() {
        let config = Config::load_for_repo(&checkout)?;
        if let Some(found) = config
            .trackers_for(&checkout, Kind::PullRequests)?
            .into_iter()
            .find(|tracker| tracker.repo.eq_ignore_ascii_case(repo))
        {
            return Ok((found.host, found.forge));
        }
    }
    let host = match guess {
        Forge::Gitlab => gitlab_host()?,
        _ => github::default_host(),
    };
    Ok((host, guess))
}

/// Fills in the host and repository a target leaves out; `remote` picks the tracker for a target
/// that names no repository.
pub fn resolve(mut target: Target, remote: Option<&str>) -> Result<Resolved> {
    if target.platform == Forge::Forgejo && target.pipeline {
        return fail("agt ci wait takes a Forgejo pull request, not an Actions run URL");
    }
    let (host, repo) = match (target.host.clone(), target.repo.clone()) {
        (Some(host), Some(repo)) => (host, repo),
        (None, Some(repo)) if target.commit => {
            let (host, platform) = commit_host(target.platform, &repo)?;
            target.platform = platform;
            (host, repo)
        }
        (None, Some(repo)) => {
            let host = match target.platform {
                Forge::Github => github::default_host(),
                Forge::Gitlab => gitlab_host()?,
                Forge::Forgejo => return fail("a Forgejo target needs a pull request URL"),
            };
            (host, repo)
        }
        (_, None) => {
            let platform = (!target.commit).then_some(target.platform);
            let (host, repo, platform) = tracker(platform, remote)?;
            target.platform = platform;
            (host, repo)
        }
    };
    let number = target.number;
    let key = match (target.platform, target.pipeline) {
        _ if target.commit => format!("{repo}@{}", target.sha.as_deref().unwrap_or_default()),
        (Forge::Github, true) => format!("{repo}/actions/runs/{number}"),
        (Forge::Github | Forge::Forgejo, _) => format!("{repo}#{number}"),
        (Forge::Gitlab, false) => format!("{repo}!{number}"),
        (Forge::Gitlab, true) => format!("{repo}/-/pipelines/{number}"),
    };
    Ok(Resolved {
        target,
        host,
        repo,
        key,
        jobs: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forgejo_url_is_a_forgejo_target_and_a_bare_hash_may_name_one() {
        let url = parse("https://codeberg.org/o/r/pulls/9@abc123").unwrap();
        assert_eq!(url.platform, Forge::Forgejo);
        assert_eq!(url.host.as_deref(), Some("codeberg.org"));
        assert_eq!((url.number, url.awaiting), (9, true));
        assert!(names_same_forge(Forge::Github, Forge::Forgejo));
        assert!(!names_same_forge(Forge::Github, Forge::Gitlab));
        assert!(!names_same_forge(Forge::Forgejo, Forge::Github));
    }

    #[test]
    fn a_forgejo_actions_run_cannot_be_waited_on() {
        let run = parse("https://codeberg.org/o/r/actions/runs/4").unwrap();
        assert!(run.pipeline);
        let error = resolve(run, None).err().unwrap();
        assert!(error.0.contains("Actions run"), "{}", error.0);
    }

    #[test]
    fn parses_every_target_form() {
        let github = parse("owner/repo#42@abc123").unwrap();
        assert_eq!(github.platform, Forge::Github);
        assert_eq!(github.repo.as_deref(), Some("owner/repo"));
        assert_eq!((github.number, github.sha.as_deref()), (42, Some("abc123")));
        let gitlab = parse("group/sub/project!7").unwrap();
        assert_eq!(gitlab.platform, Forge::Gitlab);
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
        assert_eq!(pull.platform, Forge::Github);
        assert_eq!(pull.repo.as_deref(), Some("o/r"));
        let local = parse("#8").unwrap();
        assert_eq!((local.platform, local.repo), (Forge::Github, None));
        let local = parse("!8").unwrap();
        assert_eq!((local.platform, local.pipeline), (Forge::Gitlab, false));
        let local = parse("8").unwrap();
        assert_eq!((local.platform, local.pipeline), (Forge::Gitlab, true));
    }

    #[test]
    fn parses_commit_and_actions_run_targets() {
        let commit = parse("owner/repo@abc123").unwrap();
        assert_eq!((commit.platform, commit.commit), (Forge::Github, true));
        assert_eq!(commit.repo.as_deref(), Some("owner/repo"));
        assert_eq!(
            (commit.sha.as_deref(), commit.awaiting),
            (Some("abc123"), false)
        );
        let nested = parse("group/sub/project@abc123").unwrap();
        assert_eq!((nested.platform, nested.commit), (Forge::Gitlab, true));
        let local = parse("@abc123").unwrap();
        assert_eq!((local.repo, local.commit), (None, true));
        let run = parse("https://github.com/o/r/actions/runs/5").unwrap();
        assert_eq!(
            (run.platform, run.pipeline, run.number),
            (Forge::Github, true, 5)
        );
        assert_eq!(run.repo.as_deref(), Some("o/r"));
        let job = parse("https://github.com/o/r/actions/runs/5/job/7").unwrap();
        assert_eq!((job.pipeline, job.number), (true, 5));
        assert!(parse("https://github.com/o/r/actions/runs/5@abc123").is_err());
    }

    #[test]
    fn an_actions_url_names_its_forge_and_job() {
        let github = actions_url("https://github.com/o/r/actions/runs/5/job/7").unwrap();
        assert_eq!(
            (github.forge, github.run, github.job),
            (Forge::Github, 5, Some(7))
        );
        let forgejo = actions_url("https://codeberg.org/o/r/actions/runs/4/jobs/1").unwrap();
        assert_eq!(
            (forgejo.forge, forgejo.run, forgejo.job),
            (Forge::Forgejo, 4, None)
        );
        // Only GitHub spells a job `/job/`, so a host nothing names is GitHub's with one.
        let unknown = actions_url("https://git.example.invalid/o/r/actions/runs/4/job/1").unwrap();
        assert_eq!(unknown.forge, Forge::Github);
        let unknown = actions_url("https://git.example.invalid/o/r/actions/runs/4").unwrap();
        assert_eq!(unknown.forge, Forge::Forgejo);
        let attempt = actions_url("https://github.com/o/r/actions/runs/5/attempts/2/").unwrap();
        assert_eq!((attempt.run, attempt.job), (5, None));
        // Forgejo's `/jobs/N` is no GitHub URL, and nothing may trail a run or a job.
        for bad in [
            "https://github.com/o/r/actions/runs/5/jobs/7",
            "https://github.com/o/r/actions/runs/5/workflow",
            "https://github.com/o/r/actions/runs/5/job/7/extra",
            "https://codeberg.org/o/r/actions/runs/4/jobs/1/extra",
            "https://github.com/o/r/actions/runs/0",
            "https://github.com/o/r/actions/runs/5/job/0",
            "https://codeberg.org/o/r/actions/runs/0",
        ] {
            assert!(actions_url(bad).is_none(), "{bad}");
        }
        assert!(actions_url("https://gitlab.com/o/r/actions/runs/4").is_none());
        assert!(actions_url("https://github.com/o/r/pull/4").is_none());
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
            "o/r@abc",
            "o/r@xyz123",
            "https://github.com/o/r@abc123",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
