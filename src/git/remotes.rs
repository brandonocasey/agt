//! Remotes as the repository configures them: names and URLs, read from config.

use super::{RemoteLocation, Repo, split_remote};
use crate::core::error::Result;

impl Repo {
    /// The names of the remotes, sorted.
    pub fn remote_names(&self) -> Result<Vec<String>> {
        let repo = self.open_refs()?;
        let mut names: Vec<String> = repo
            .remotes()?
            .iter()
            .flatten()
            .map(str::to_owned)
            .collect();
        names.sort();
        Ok(names)
    }

    /// Whether the repository has a remote called `name`, whatever its URL.
    pub fn has_remote(&self, name: &str) -> Result<bool> {
        Ok(self.remote_names()?.iter().any(|remote| remote == name))
    }

    /// The URL of `origin` as git expands it (`insteadOf`), `None` without an origin.
    pub fn origin_url(&self) -> Result<Option<String>> {
        self.remote_url("origin")
    }

    fn remote_url(&self, name: &str) -> Result<Option<String>> {
        let repo = self.open_refs()?;
        Ok(repo
            .find_remote(name)
            .ok()
            .and_then(|remote| remote.url().map(str::to_owned)))
    }

    /// The URL of remote `name` as the config file writes it, before `insteadOf`.
    fn written_remote_url(&self, name: &str) -> Result<Option<String>> {
        let repo = self.open_refs()?;
        let config = repo.config()?;
        Ok(config.get_string(&format!("remote.{name}.url")).ok())
    }

    /// The lowercase host and `owner/repo` path of `origin`; `None` without an origin or when no
    /// URL of it names a repository. The URL git expands (`insteadOf`) is tried first, then the
    /// one written in the config, so a rewritten URL that no longer parses still resolves.
    pub fn origin_location(&self) -> Result<Option<(String, String)>> {
        self.remote_location("origin")
    }

    fn remote_location(&self, name: &str) -> Result<Option<(String, String)>> {
        if let Some(location) = self.remote_url(name)?.and_then(|url| split_remote(&url)) {
            return Ok(Some(location));
        }
        let written = self.written_remote_url(name)?;
        Ok(written.and_then(|url| split_remote(&url)))
    }

    /// The host and `owner/repo` path of every remote whose URL names a repository, as
    /// [`Repo::origin_location`] reads `origin`: `origin` first, then `upstream`, then the rest
    /// by name.
    pub fn remote_locations(&self) -> Result<Vec<RemoteLocation>> {
        let mut names = self.remote_names()?;
        names.sort_by_key(|name| {
            let rank = match name.as_str() {
                "origin" => 0,
                "upstream" => 1,
                _ => 2,
            };
            (rank, name.clone())
        });
        let mut found = Vec::new();
        for name in names {
            if let Some((host, repo)) = self.remote_location(&name)? {
                found.push(RemoteLocation { name, host, repo });
            }
        }
        Ok(found)
    }
}
