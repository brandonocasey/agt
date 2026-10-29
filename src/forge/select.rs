//! Choosing the tracker a command acting on one PR/MR talks to, and what its modes allow.

use crate::core::config::{Config, Kind, Mode, RemoteSet, Tracker, forge_of};
use crate::core::error::{Error, Result, fail};
use crate::forge::service;
use crate::forge::{PrRef, hosts};
use crate::git::{RemoteLocation, Repo};

/// The repository a PR/MR command works on, with the modes that apply to it.
#[derive(Debug, Clone)]
pub struct Selected {
    pub host: String,
    pub repo: String,
    /// `None` for a repository no tracker names: `origin` of a configuration with no tracker for
    /// it, or the repository of a PR/MR URL.
    pub tracker: Option<Tracker>,
    pub pull: Mode,
    /// `None` when nothing set it.
    pub push: Option<Mode>,
}

impl Selected {
    fn of_tracker(tracker: &Tracker) -> Self {
        Self {
            host: tracker.host.clone(),
            repo: tracker.repo.clone(),
            tracker: Some(tracker.clone()),
            pull: tracker.pull,
            push: tracker.push,
        }
    }

    fn bare(config: &Config, kind: Kind, host: &str, repo: &str) -> Self {
        Self {
            host: host.to_owned(),
            repo: repo.to_owned(),
            tracker: None,
            pull: config.pull_for(kind),
            push: config.push_for(kind),
        }
    }

    /// `host/repo`.
    pub fn name(&self) -> String {
        format!("{}/{}", self.host, self.repo)
    }

    pub fn push_mode(&self) -> Mode {
        self.push.unwrap_or(Mode::Off)
    }
}

/// The tracker for `host` and `repo` among every remote of the repository and every configured
/// tracker, or a bare selection with the modes of `kind` when none names it.
pub fn at_location(
    repo: &Repo,
    config: &Config,
    kind: Kind,
    host: &str,
    path: &str,
) -> Result<Selected> {
    let trackers = config.trackers_from(kind, &repo.remote_locations()?, RemoteSet::All);
    Ok(trackers
        .iter()
        .find(|tracker| tracker.is_at(host, path))
        .map_or_else(
            || Selected::bare(config, kind, host, path),
            Selected::of_tracker,
        ))
}

/// Picks the tracker for a command, in order: `--remote` (a remote name or `host/repo`), the
/// repository of a PR/MR URL, the tracker of `origin`, the only tracker. `None` when there is
/// nothing to pick from: no tracker and no usable `origin`.
pub fn select_tracker(
    repo: &Repo,
    config: &Config,
    kind: Kind,
    remote: Option<&str>,
    pr: Option<&PrRef>,
) -> Result<Option<Selected>> {
    let locations = repo.remote_locations()?;
    if let Some(name) = remote {
        // Any remote of the repository can be named, whatever `[sync] remotes` picks.
        let trackers = config.trackers_from(kind, &locations, RemoteSet::All);
        let found: Vec<&Tracker> = trackers
            .iter()
            .filter(|tracker| {
                tracker.remote.as_deref() == Some(name) || tracker.name().eq_ignore_ascii_case(name)
            })
            .collect();
        return match found.as_slice() {
            [one] => Ok(Some(Selected::of_tracker(one))),
            _ => fail(format!(
                "--remote {name} matches no tracker for {}; choose {}",
                kind_name(kind),
                choices(&trackers)
            )),
        };
    }
    let trackers = config.trackers(kind, &locations);
    if let Some((host, path)) = pr.and_then(|pr| pr.location.as_ref()) {
        return Ok(Some(
            trackers
                .iter()
                .find(|tracker| tracker.is_at(host, path))
                .map_or_else(
                    || Selected::bare(config, kind, host, path),
                    Selected::of_tracker,
                ),
        ));
    }
    if let Some(origin) = trackers
        .iter()
        .find(|tracker| tracker.remote.as_deref() == Some("origin"))
    {
        return Ok(Some(Selected::of_tracker(origin)));
    }
    // An origin only a CLI login identifies (GitHub Enterprise, a self-hosted GitLab) is still
    // origin, and wins over other remotes.
    if !trackers.is_empty()
        && let Some(origin) = origin_only(config, kind, &locations)
        && service::detect(&origin.host, hosts::named(&origin.host)?, None).is_some()
    {
        return Ok(Some(origin));
    }
    match trackers.as_slice() {
        [one] => Ok(Some(Selected::of_tracker(one))),
        [] => Ok(origin_only(config, kind, &locations)),
        _ => fail(format!(
            "several trackers serve {} and none is origin; pass --remote with {}",
            kind_name(kind),
            choices(&trackers)
        )),
    }
}

/// `origin` as a tracker nothing configured, which keeps a repository on a host agt cannot
/// identify without asking a CLI working as before. A host agt knows that has no tracker was
/// dropped on purpose, and `[sync] remotes = "none"` asks for configured trackers only.
fn origin_only(config: &Config, kind: Kind, locations: &[RemoteLocation]) -> Option<Selected> {
    if config.sync.remotes == RemoteSet::None {
        return None;
    }
    let origin = locations.iter().find(|remote| remote.name == "origin")?;
    if forge_of(&origin.host).is_some() {
        return None;
    }
    Some(Selected::bare(config, kind, &origin.host, &origin.repo))
}

/// `origin` as a bare selection when nothing configured it and a CLI login, not its host name,
/// is what identifies it; see [`origin_only`].
pub fn origin_fallback(repo: &Repo, config: &Config, kind: Kind) -> Result<Option<Selected>> {
    Ok(origin_only(config, kind, &repo.remote_locations()?))
}

/// Like [`select_tracker`], but nothing to pick from is an error.
pub fn require_tracker(
    repo: &Repo,
    config: &Config,
    kind: Kind,
    remote: Option<&str>,
    pr: Option<&PrRef>,
) -> Result<Selected> {
    select_tracker(repo, config, kind, remote, pr)?.ok_or_else(|| {
        let hint = if config.sync.remotes == crate::core::config::RemoteSet::None {
            "; `[sync] remotes = \"none\"` turns off remote trackers"
        } else {
            ""
        };
        Error(format!(
            "no tracker for {}: add a GitHub or GitLab origin remote, a [[tracker]], or pass a PR/MR URL{hint}",
            kind_name(kind)
        ))
    })
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Issues => "issues",
        Kind::PullRequests => "pull requests",
    }
}

fn choices(trackers: &[Tracker]) -> String {
    if trackers.is_empty() {
        return "a remote of this repository once a tracker serves it (none does)".into();
    }
    trackers
        .iter()
        .map(|tracker| match &tracker.remote {
            Some(remote) => format!("`{remote}` ({})", tracker.name()),
            None => format!("`{}`", tracker.name()),
        })
        .collect::<Vec<_>>()
        .join(", ")
}
