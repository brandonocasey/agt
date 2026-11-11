//! The repository of the selected tracker, with the service that talks to its forge.
//! The GitHub, GitLab and Forgejo providers share it.

use super::provider::At;
use crate::core::config::{Config, Kind};
use crate::core::error::{Error, Result, fail};
use crate::forge::auto::pr_write_allowed;
use crate::forge::landed::Source;
use crate::forge::select::{Selected, require_tracker};
use crate::forge::service::{ReviewService, SERVICES, kind};
use crate::forge::transport::Transport;
use crate::forge::{PrRef, parse_pr};
use crate::git::Repo;

pub struct Remote {
    pub origin: Source,
    pub service: Box<dyn ReviewService>,
    selected: Selected,
}

impl Remote {
    pub fn connect(
        repo: &Repo,
        config: &Config,
        remote: Option<&str>,
        pr: Option<&PrRef>,
    ) -> Result<Self> {
        let selected = require_tracker(repo, config, Kind::PullRequests, remote, pr)?;
        let origin = Source::of(&selected)?;
        let service = (kind(origin.forge).connect)(&origin.host);
        Ok(Self {
            origin,
            service,
            selected,
        })
    }

    /// The transport and the `owner/name` every forge call takes.
    pub fn api(&self) -> (&dyn Transport, &str) {
        (self.service.transport(), self.origin.repo.as_str())
    }

    pub fn at(&self, text: &str) -> Result<At> {
        let PrRef {
            number, location, ..
        } = parse_pr(text)?;
        if let Some((host, repo)) = location
            && (host != self.origin.host || repo != self.origin.repo)
        {
            return fail(format!(
                "{text} is not in this repository ({}/{})",
                self.origin.host, self.origin.repo
            ));
        }
        Ok(At::Number(number))
    }

    /// Refuses `what` when the tracker's `push` is `off`.
    pub fn allow_write(&self, what: &str) -> Result<()> {
        pr_write_allowed(what, &self.selected.name(), self.selected.push)
    }

    pub fn key(&self, at: &At) -> String {
        format!("{}/{}#{}", self.origin.host, self.origin.repo, at.id())
    }
}

/// The number a forge call names.
pub fn number(at: &At) -> Result<u64> {
    match at {
        At::Number(number) => Ok(*number),
        At::Branch(branch) => fail(format!(
            "{branch} is a branch; a forge addresses a PR/MR by number"
        )),
    }
}

/// Names a missing or unauthenticated gh/glab/tea instead of repeating the raw tool error.
pub fn explain(error: Error) -> Error {
    let message = &error.0;
    for tool in SERVICES.iter().map(|kind| kind.tool) {
        if message.starts_with(&format!("run {tool}:")) {
            return Error(format!(
                "{tool} is not installed or not on PATH (needed for this repository's forge)"
            ));
        }
        if message.starts_with(&format!("{tool} "))
            && [
                "auth login",
                "login add",
                "HTTP 401",
                "not logged in",
                "authentication",
            ]
            .iter()
            .any(|hint| message.contains(hint))
        {
            let login = if tool == "tea" {
                "login add"
            } else {
                "auth login"
            };
            return Error(format!(
                "{tool} is not authenticated: run `{tool} {login}`\n{message}"
            ));
        }
    }
    error
}
