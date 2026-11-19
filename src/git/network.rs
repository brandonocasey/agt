//! Talking to remotes through libgit2: listing, fetching, and the credentials both use.
//! Credentials come from ssh-agent, keys in `~/.ssh`, and a configured git credential helper;
//! nothing here prompts.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use super::known_hosts::{self, Verdict};
use super::{Kind, Repo};
use crate::core::error::{Error, Result};

/// One ref a remote advertises.
pub struct RemoteRef {
    pub name: String,
    pub id: String,
    /// The ref this one points at, for a symbolic ref such as `HEAD`.
    pub symref: Option<String>,
}

/// The key files tried, in order, after the ssh agent.
const SSH_KEY_FILES: [&str; 3] = ["id_ed25519", "id_rsa", "id_ecdsa"];

/// What the credential callback has handed out, so that a rejected credential is not offered
/// again and the connection fails instead of looping.
#[derive(Default)]
struct Offered {
    agent: bool,
    keys: usize,
    helper: bool,
}

impl Repo {
    /// Every ref `remote` (a configured name or a URL) advertises.
    pub fn ls_remote(&self, remote: &str) -> Result<Vec<RemoteRef>> {
        let repo = self.open()?;
        let mut remote = remote_named(&repo, remote)?;
        let config = repo.config()?;
        let url = remote.url().unwrap_or_default().to_owned();
        let mut proxy = git2::ProxyOptions::new();
        proxy.auto();
        let connection = remote.connect_auth(
            git2::Direction::Fetch,
            Some(callbacks(&config, &url)),
            Some(proxy),
        )?;
        let refs = connection
            .list()?
            .iter()
            .map(|head| RemoteRef {
                name: head.name().to_owned(),
                id: head.oid().to_string(),
                symref: head.symref_target().map(str::to_owned),
            })
            .collect();
        Ok(refs)
    }

    /// [`Self::ls_remote`], giving up after `seconds`: a remote that does not answer MUST NOT
    /// hold the caller.
    pub fn ls_remote_within(&self, remote: &str, seconds: u64) -> Result<Vec<RemoteRef>> {
        let (root, kind): (PathBuf, Kind) = (self.root.clone(), self.kind);
        let remote = remote.to_owned();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let listed = Repo::of(root, kind).ls_remote(&remote);
            // The receiver is gone once the deadline passed.
            let _ = sender.send(listed);
        });
        receiver
            .recv_timeout(Duration::from_secs(seconds))
            .map_err(|_| Error(format!("ls-remote took longer than {seconds}s")))?
    }

    /// Whether `remote` has `reference` (a full ref name). The error is libgit2's own text when
    /// it could not ask.
    pub fn remote_has(&self, remote: &str, reference: &str) -> Result<bool> {
        Ok(self
            .ls_remote(remote)?
            .iter()
            .any(|advertised| advertised.name == reference))
    }

    /// Fetches `refspecs` from the configured `remote` without tags, and updates the refs a
    /// refspec names on the right.
    pub fn fetch(&self, remote: &str, refspecs: &[&str]) -> Result<()> {
        let repo = self.open()?;
        let mut remote = repo.find_remote(remote)?;
        fetch_from(&repo, &mut remote, refspecs)
    }

    /// Fetches `refspec` from `remote` and returns the commit it points at. Only the remote's
    /// URL is used, so no remote-tracking branch moves.
    pub fn fetch_commit(&self, remote: &str, refspec: &str) -> Result<String> {
        let repo = self.open()?;
        let url = remote_url(&repo, remote)?;
        let mut anonymous = repo.remote_anonymous(&url)?;
        fetch_from(&repo, &mut anonymous, &[refspec])?;
        let mut fetched = None;
        repo.fetchhead_foreach(|_, _, id, _| {
            fetched.get_or_insert(*id);
            true
        })?;
        fetched
            .map(|id| id.to_string())
            .ok_or_else(|| Error(format!("{remote} sent nothing for {refspec}")))
    }
}

fn fetch_from(
    repo: &git2::Repository,
    remote: &mut git2::Remote<'_>,
    refspecs: &[&str],
) -> Result<()> {
    let config = repo.config()?;
    let url = remote.url().unwrap_or_default().to_owned();
    remote.fetch(refspecs, Some(&mut fetch_options(&config, &url)), None)?;
    Ok(())
}

/// Fetch options for `url` with the credential chain, the proxy settings of the config and no
/// tags.
pub(super) fn fetch_options<'a>(config: &'a git2::Config, url: &str) -> git2::FetchOptions<'a> {
    let mut options = git2::FetchOptions::new();
    options
        .remote_callbacks(callbacks(config, url))
        .download_tags(git2::AutotagOption::None);
    let mut proxy = git2::ProxyOptions::new();
    proxy.auto();
    options.proxy_options(proxy);
    options
}

/// The configured remote `name`, or an anonymous one when `name` is a URL or path.
fn remote_named<'r>(repo: &'r git2::Repository, name: &str) -> Result<git2::Remote<'r>> {
    match repo.find_remote(name) {
        Ok(remote) => Ok(remote),
        Err(_) => Ok(repo.remote_anonymous(name)?),
    }
}

fn remote_url(repo: &git2::Repository, name: &str) -> Result<String> {
    let remote = repo.find_remote(name)?;
    remote
        .url()
        .map(str::to_owned)
        .ok_or_else(|| Error(format!("remote {name} has no URL")))
}

fn callbacks<'a>(config: &'a git2::Config, url: &str) -> git2::RemoteCallbacks<'a> {
    let mut offered = Offered::default();
    let mut callbacks = git2::RemoteCallbacks::new();
    let port = known_hosts::port_of(url);
    callbacks.certificate_check(move |cert, host| check_host_key(cert, host, port));
    callbacks.credentials(move |url, username_from_url, allowed| {
        let user = username_from_url.unwrap_or("git");
        if allowed.contains(git2::CredentialType::SSH_KEY) {
            if !offered.agent {
                offered.agent = true;
                return git2::Cred::ssh_key_from_agent(user);
            }
            while let Some(key) = ssh_key_file(offered.keys) {
                offered.keys += 1;
                if key.is_file() {
                    return git2::Cred::ssh_key(user, None, &key, None);
                }
            }
        }
        if allowed.contains(git2::CredentialType::USER_PASS_PLAINTEXT)
            && !offered.helper
            && has_credential_helper(config)
        {
            offered.helper = true;
            return git2::Cred::credential_helper(config, url, username_from_url);
        }
        Err(git2::Error::from_str(
            "no credentials: add a key to ssh-agent or ~/.ssh, or configure a git credential helper",
        ))
    });
    callbacks
}

/// Accepts an ssh host key only when a `known_hosts` file lists it; a key of any other
/// certificate (https) is left to libgit2.
fn check_host_key(
    cert: &git2::cert::Cert<'_>,
    host: &str,
    port: u16,
) -> std::result::Result<git2::CertificateCheckStatus, git2::Error> {
    let Some(hostkey) = cert.as_hostkey() else {
        return Ok(git2::CertificateCheckStatus::CertificatePassthrough);
    };
    let Some(key) = hostkey.hostkey() else {
        return Ok(git2::CertificateCheckStatus::CertificatePassthrough);
    };
    let verdict = known_hosts::check(&known_hosts::default_files(), host, port, key)
        .map_err(|error| git2::Error::from_str(&error.0))?;
    match verdict {
        Verdict::Known => Ok(git2::CertificateCheckStatus::CertificateOk),
        Verdict::Revoked => Err(git2::Error::from_str(&format!(
            "the ssh host key of {host} is marked @revoked in a known_hosts file"
        ))),
        Verdict::Unknown => Err(git2::Error::from_str(&known_hosts::unknown_host_message(
            host,
            port,
            hostkey.hash_sha256().map(known_hosts::fingerprint),
        ))),
    }
}

/// The `index`th key file of `~/.ssh`, or `None` after the last.
fn ssh_key_file(index: usize) -> Option<PathBuf> {
    let name = SSH_KEY_FILES.get(index)?;
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".ssh").join(name))
}

/// Whether the config names a credential helper, which is the only way to get a password
/// without asking for it.
fn has_credential_helper(config: &git2::Config) -> bool {
    let Ok(mut entries) = config.entries(Some("credential.*")) else {
        return false;
    };
    while let Some(Ok(entry)) = entries.next() {
        let is_helper = entry.name().is_some_and(|name| name.ends_with(".helper"));
        if is_helper && entry.value().is_some_and(|value| !value.is_empty()) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::super::fixtures::{commit, rev};
    use crate::core::testing::Sandbox;
    use crate::git::Repo;

    /// A work tree whose `origin` is a bare repository next to it, with `main` pushed there.
    fn with_origin(sandbox: &Sandbox) -> (PathBuf, PathBuf) {
        let bare = sandbox.path().join("origin.git");
        sandbox.git(
            sandbox.path(),
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        let dir = sandbox.repo();
        sandbox.git(&dir, &["init", "-q", "-b", "main"]);
        commit(sandbox, &dir, &[("a.txt", "a\n")], "one");
        sandbox.git(&dir, &["remote", "add", "origin", bare.to_str().unwrap()]);
        sandbox.git(&dir, &["push", "-q", "origin", "main"]);
        sandbox.git(&dir, &["tag", "v1"]);
        sandbox.git(&dir, &["push", "-q", "origin", "v1"]);
        (dir, bare)
    }

    /// Pushes a commit that only the remote has and returns its id.
    fn push_elsewhere(sandbox: &Sandbox, dir: &Path) -> String {
        let other = sandbox.path().join("other");
        let bare = sandbox.git_output(dir, &["remote", "get-url", "origin"]);
        sandbox.git(
            sandbox.path(),
            &["clone", "-q", bare.trim(), other.to_str().unwrap()],
        );
        commit(sandbox, &other, &[("b.txt", "b\n")], "two");
        sandbox.git(&other, &["tag", "v2"]);
        sandbox.git(&other, &["push", "-q", "origin", "main", "v2"]);
        rev(sandbox, &other, "HEAD")
    }

    #[test]
    fn ls_remote_lists_what_git_ls_remote_lists() {
        let sandbox = Sandbox::new();
        let (dir, _) = with_origin(&sandbox);
        let repo = Repo::at(&dir);

        let mut ours: Vec<String> = repo
            .ls_remote("origin")
            .unwrap()
            .iter()
            .map(|advertised| format!("{}\t{}", advertised.id, advertised.name))
            .collect();
        let mut expected: Vec<String> = sandbox
            .git_output(&dir, &["ls-remote", "origin"])
            .lines()
            .map(str::to_owned)
            .collect();
        ours.sort();
        expected.sort();
        assert_eq!(ours, expected);
        assert!(expected.len() >= 3, "{expected:?}");
        assert!(repo.remote_has("origin", "refs/heads/main").unwrap());
        assert!(!repo.remote_has("origin", "refs/heads/absent").unwrap());
        assert_eq!(
            repo.ls_remote_within("origin", 30).unwrap().len(),
            expected.len()
        );
        let missing = repo.ls_remote("nowhere");
        assert!(missing.is_err());
    }

    #[test]
    fn fetch_moves_the_tracking_branch_and_fetch_commit_moves_nothing() {
        let sandbox = Sandbox::new();
        let (dir, _) = with_origin(&sandbox);
        let repo = Repo::at(&dir);
        let before = rev(&sandbox, &dir, "refs/remotes/origin/main");
        let pushed = push_elsewhere(&sandbox, &dir);

        assert_eq!(
            repo.fetch_commit("origin", "refs/heads/main").unwrap(),
            pushed
        );
        assert_eq!(rev(&sandbox, &dir, "refs/remotes/origin/main"), before);
        assert_eq!(rev(&sandbox, &dir, &pushed), pushed);

        repo.fetch("origin", &["+refs/heads/main:refs/remotes/origin/main"])
            .unwrap();
        assert_eq!(rev(&sandbox, &dir, "refs/remotes/origin/main"), pushed);
        assert_eq!(
            sandbox.git_output(&dir, &["tag", "--list"]).trim(),
            "v1",
            "a fetch brings no tags"
        );
    }
}
