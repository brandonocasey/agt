//! Talking to remotes through libgit2: listing, fetching, and the credentials both use.
//! Credentials come from ssh-agent, keys in `~/.ssh`, and a configured git credential helper,
//! which `core::process::disable_credential_prompts` keeps from asking the user.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError, mpsc};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use std::time::Duration;

use super::http_config::{self, Http};
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

/// The largest buffer `getpwuid_r` is offered for a passwd entry.
const PASSWD_BUFFER_LIMIT: usize = 1 << 20;

/// The key files tried, in order, after the ssh agent.
const SSH_KEY_FILES: [&str; 3] = ["id_ed25519", "id_rsa", "id_ecdsa"];

/// A refspec that matches no ref a remote advertises.
const NO_REF: &str = "refs/agt/nothing";

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
        let repo = self.open_refs()?;
        let mut remote = remote_named(&repo, remote)?;
        let url = remote.url().unwrap_or_default().to_owned();
        let config = repo.config()?;
        let mut options = fetch_options(&config, &url)?;
        // `connect_auth` cannot send the extra headers of `http.extraHeader`, so the listing is
        // a download. libgit2 fetches the configured refspecs, or `HEAD`, when none is given; a
        // refspec that no ref matches connects like a fetch and wants no object.
        remote
            .download(&[NO_REF], Some(&mut options))
            .map_err(explain)?;
        let refs = remote
            .list()?
            .iter()
            .map(|head| RemoteRef {
                name: head.name().to_owned(),
                id: head.oid().to_string(),
                symref: head.symref_target().map(str::to_owned),
            })
            .collect();
        remote.disconnect()?;
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
        let repo = self.open_refs()?;
        let mut remote = repo.find_remote(remote)?;
        fetch_from(&repo, &mut remote, refspecs)
    }

    /// Fetches `refspec` from `remote` and returns the commit it points at. Only the remote's
    /// URL is used, so no remote-tracking branch moves.
    pub fn fetch_commit(&self, remote: &str, refspec: &str) -> Result<String> {
        let repo = self.open_refs()?;
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
    remote
        .fetch(refspecs, Some(&mut fetch_options(&config, &url)?), None)
        .map_err(explain)?;
    Ok(())
}

/// Fetch options for `url` with the credential chain, the proxy settings and the `http.*`
/// settings of the config, and no tags.
pub(super) fn fetch_options<'a>(
    config: &'a git2::Config,
    url: &str,
) -> Result<git2::FetchOptions<'a>> {
    let http = http_config::settings(config, url)?;
    trust_extra_authorities(&http)?;
    let headers: Vec<&str> = http.headers.iter().map(String::as_str).collect();
    let mut options = git2::FetchOptions::new();
    options
        .remote_callbacks(callbacks(config, url, http.verify))
        .custom_headers(&headers)
        .download_tags(git2::AutotagOption::None);
    let mut proxy = git2::ProxyOptions::new();
    proxy.auto();
    options.proxy_options(proxy);
    Ok(options)
}

/// Adds the certificate authorities that `http.sslCAInfo` and `http.sslCAPath` name to the ones
/// libgit2 trusts. libgit2 keeps one trust store per process and cannot drop an authority from
/// it, so a later connection to any host trusts them too. On macOS libgit2 asks the system
/// keychain, which takes no authority from a file.
fn trust_extra_authorities(http: &Http) -> Result<()> {
    static LOADING: Mutex<()> = Mutex::new(());
    if cfg!(target_vendor = "apple") {
        return Ok(());
    }
    let _loading = LOADING.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(file) = &http.ca_file {
        // SAFETY: libgit2 does not guard this global; `LOADING` keeps two calls apart.
        unsafe { git2::opts::set_ssl_cert_file(file) }
            .map_err(|error| Error(format!("http.sslCAInfo {}: {error}", file.display())))?;
    }
    if let Some(dir) = &http.ca_dir {
        // SAFETY: as above.
        unsafe { git2::opts::set_ssl_cert_dir(dir) }
            .map_err(|error| Error(format!("http.sslCAPath {}: {error}", dir.display())))?;
    }
    Ok(())
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

/// The callbacks for a connection to `url`. `verify` is `http.sslVerify`: when false, an HTTPS
/// certificate is accepted whatever is wrong with it.
fn callbacks<'a>(config: &'a git2::Config, url: &str, verify: bool) -> git2::RemoteCallbacks<'a> {
    let mut offered = Offered::default();
    let mut callbacks = git2::RemoteCallbacks::new();
    let port = known_hosts::port_of(url);
    callbacks.certificate_check(move |cert, host| check_certificate(cert, host, port, verify));
    callbacks.credentials(move |url, username_from_url, allowed| {
        if allowed.contains(git2::CredentialType::USERNAME) {
            return user_name_credential(username_from_url);
        }
        let user = username_from_url.unwrap_or("git");
        if allowed.contains(git2::CredentialType::SSH_KEY) {
            if !offered.agent {
                offered.agent = true;
                return git2::Cred::ssh_key_from_agent(user);
            }
            while let Some(key) = ssh_key_file(offered.keys) {
                offered.keys += 1;
                if key.is_file() && !needs_passphrase(&key) {
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

/// The answer to libgit2's user name request for an ssh URL that lacks one.
/// It is the name the URL gives, else the login name.
fn user_name_credential(url_user: Option<&str>) -> std::result::Result<git2::Cred, git2::Error> {
    let name = chosen_user_name(url_user, login_name())
        .ok_or_else(|| git2::Error::from_str("no user name: put one in the URL (user@host)"))?;
    git2::Cred::username(&name)
}

fn chosen_user_name(url_user: Option<&str>, login: Option<String>) -> Option<String> {
    url_user.map(str::to_owned).or(login)
}

/// The login name of the current user.
/// It is `USER` or `LOGNAME`, else the password database's name for the user id.
/// The database lookup is what ssh uses.
fn login_name() -> Option<String> {
    ["USER", "LOGNAME"]
        .iter()
        .find_map(|variable| std::env::var(variable).ok().filter(|name| !name.is_empty()))
        .or_else(passwd_name)
}

/// The password database's name for the user id of this process.
fn passwd_name() -> Option<String> {
    let mut buffer = vec![0u8; 1024];
    loop {
        // SAFETY: `passwd` is plain data that `getpwuid_r` fills; `buffer` outlives the name read
        // from it, and the call is given its true length.
        let (status, name) = unsafe {
            let mut entry: libc::passwd = std::mem::zeroed();
            let mut found: *mut libc::passwd = std::ptr::null_mut();
            let status = libc::getpwuid_r(
                libc::getuid(),
                &raw mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &raw mut found,
            );
            let name = (status == 0 && !found.is_null()).then(|| {
                std::ffi::CStr::from_ptr(entry.pw_name)
                    .to_string_lossy()
                    .into_owned()
            });
            (status, name)
        };
        if status == libc::ERANGE && buffer.len() < PASSWD_BUFFER_LIMIT {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        return name.filter(|name| !name.is_empty());
    }
}

thread_local! {
    /// What the host-key check said when it refused the last connection of this thread.
    static HOST_KEY_REFUSAL: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Remembers why a host key was refused.
/// libgit2 replaces the check's error with its own text; [`explain`] restores this one.
fn refuse_host_key(message: String) -> git2::Error {
    HOST_KEY_REFUSAL.with(|refusal| *refusal.borrow_mut() = Some(message.clone()));
    git2::Error::from_str(&message)
}

/// `error` of a network call, as the host-key check explained it when it refused the connection.
pub(super) fn explain(error: git2::Error) -> Error {
    let refusal = HOST_KEY_REFUSAL.with(|refusal| refusal.borrow_mut().take());
    match refusal {
        Some(message) if error.class() == git2::ErrorClass::Ssh => Error(message),
        _ => error.into(),
    }
}

/// Accepts an ssh host key only when a `known_hosts` file lists it; an https certificate is
/// left to libgit2, unless `verify` is false.
fn check_certificate(
    cert: &git2::cert::Cert<'_>,
    host: &str,
    port: u16,
    verify: bool,
) -> std::result::Result<git2::CertificateCheckStatus, git2::Error> {
    let Some(hostkey) = cert.as_hostkey() else {
        return Ok(if verify {
            git2::CertificateCheckStatus::CertificatePassthrough
        } else {
            git2::CertificateCheckStatus::CertificateOk
        });
    };
    let Some(key) = hostkey.hostkey() else {
        return Ok(git2::CertificateCheckStatus::CertificatePassthrough);
    };
    let verdict = known_hosts::check(&known_hosts::default_files(), host, port, key)
        .map_err(|error| refuse_host_key(error.0))?;
    match verdict {
        Verdict::Known => Ok(git2::CertificateCheckStatus::CertificateOk),
        Verdict::Revoked => Err(refuse_host_key(format!(
            "the ssh host key of {host} is marked @revoked in a known_hosts file"
        ))),
        Verdict::Unknown => Err(refuse_host_key(known_hosts::unknown_host_message(
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

/// Whether the private key file `key` is encrypted, or cannot be read. libgit2 then fails the
/// whole connection instead of asking for the next credential, and agt has no passphrase to give.
fn needs_passphrase(key: &Path) -> bool {
    const OPENSSH_BEGIN: &str = "-----BEGIN OPENSSH PRIVATE KEY-----";
    const OPENSSH_END: &str = "-----END OPENSSH PRIVATE KEY-----";
    const OPENSSH_MAGIC: &[u8] = b"openssh-key-v1\0";
    let Ok(text) = std::fs::read_to_string(key) else {
        return true;
    };
    let Some((_, body)) = text.split_once(OPENSSH_BEGIN) else {
        return text.contains("ENCRYPTED");
    };
    let body: String = body
        .split(OPENSSH_END)
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    let Ok(bytes) = STANDARD.decode(body) else {
        return true;
    };
    // After the magic comes the name of the cipher, which is `none` for a key without passphrase.
    let cipher = bytes.strip_prefix(OPENSSH_MAGIC).and_then(|rest| {
        let length = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?);
        rest.get(4..4 + usize::try_from(length).ok()?)
    });
    cipher != Some(b"none")
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
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::super::fixtures::{commit, rev};
    use super::{chosen_user_name, explain, needs_passphrase, passwd_name, refuse_host_key};
    use crate::core::testing::Sandbox;
    use crate::git::Repo;
    use crate::git::http_config;

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
    fn a_refused_host_key_is_explained_once_and_only_for_an_ssh_failure() {
        let failure = |class| {
            git2::Error::new(
                git2::ErrorCode::GenericError,
                class,
                "invalid or unknown remote ssh hostkey",
            )
        };
        let own = "the ssh host key of example.org is not in a known_hosts file";
        refuse_host_key(own.to_owned());
        assert_eq!(explain(failure(git2::ErrorClass::Ssh)).0, own);
        assert_eq!(
            explain(failure(git2::ErrorClass::Ssh)).0,
            "invalid or unknown remote ssh hostkey",
            "a refusal explains one failure"
        );
        refuse_host_key(own.to_owned());
        assert_eq!(
            explain(failure(git2::ErrorClass::Net)).0,
            "invalid or unknown remote ssh hostkey",
            "only an ssh failure carries the refusal"
        );
    }

    #[test]
    fn a_user_name_request_prefers_the_url_over_the_login() {
        let login = || Some("bob".to_owned());
        assert_eq!(chosen_user_name(Some("alice"), login()).unwrap(), "alice");
        assert_eq!(chosen_user_name(None, login()).unwrap(), "bob");
        assert_eq!(chosen_user_name(None, None), None);
    }

    #[test]
    fn the_password_database_names_the_user_as_id_does() {
        let sandbox = Sandbox::new();
        let expected = sandbox.shell_output(sandbox.path(), "id -un");
        assert_eq!(passwd_name().as_deref(), Some(expected.trim()));
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
    fn a_listing_writes_no_object_into_the_repository() {
        let sandbox = Sandbox::new();
        let (dir, _) = with_origin(&sandbox);
        let pushed = push_elsewhere(&sandbox, &dir);
        let has_pushed = || {
            sandbox
                .command("git")
                .current_dir(&dir)
                .args(["cat-file", "-e", &pushed])
                .status()
                .unwrap()
                .success()
        };
        assert!(!has_pushed(), "the clone had the commit already");

        let refs = Repo::at(&dir).ls_remote("origin").unwrap();

        assert!(refs.iter().any(|head| head.id == pushed), "{pushed}");
        assert!(!has_pushed(), "the listing stored {pushed}");
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

    #[test]
    fn a_private_key_that_needs_a_passphrase_is_not_offered() {
        let sandbox = Sandbox::new();
        let cases: [(&[&str], bool); 6] = [
            (&["-t", "ed25519", "-N", ""], false),
            (&["-t", "ed25519", "-N", "secret"], true),
            (&["-t", "rsa", "-b", "2048", "-m", "PEM", "-N", ""], false),
            (
                &["-t", "rsa", "-b", "2048", "-m", "PEM", "-N", "secret"],
                true,
            ),
            (&["-t", "rsa", "-b", "2048", "-m", "PKCS8", "-N", ""], false),
            (
                &["-t", "rsa", "-b", "2048", "-m", "PKCS8", "-N", "secret"],
                true,
            ),
        ];
        for (number, (args, encrypted)) in cases.iter().enumerate() {
            let key = sandbox.path().join(format!("key{number}"));
            let made = sandbox
                .command("ssh-keygen")
                .args(["-q", "-f"])
                .arg(&key)
                .args(*args)
                .status()
                .unwrap();
            assert!(made.success(), "ssh-keygen {args:?}");
            assert_eq!(needs_passphrase(&key), *encrypted, "ssh-keygen {args:?}");
        }
        assert!(needs_passphrase(&sandbox.path().join("absent")));
    }

    /// A local HTTP server that records each request head and answers 404.
    struct Server {
        address: std::net::SocketAddr,
        requests: Arc<Mutex<Vec<String>>>,
        stopped: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Server {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stopped = Arc::new(AtomicBool::new(false));
            let (seen, done) = (Arc::clone(&requests), Arc::clone(&stopped));
            let thread = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if done.load(Ordering::SeqCst) {
                        return;
                    }
                    let mut stream = stream.unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut head = String::new();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        head.push_str(&line);
                    }
                    seen.lock().unwrap().push(head);
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            });
            Self {
                address,
                requests,
                stopped,
                thread: Some(thread),
            }
        }

        fn url(&self, path: &str) -> String {
            format!("http://{}{path}", self.address)
        }

        /// Every request head received so far, in lower case.
        fn heads(&self) -> String {
            self.requests.lock().unwrap().join("\n").to_lowercase()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(self.address);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn set_config(sandbox: &Sandbox, dir: &Path, key: &str, value: &str) {
        sandbox.git(dir, &["config", "--add", key, value]);
    }

    #[test]
    fn ls_remote_and_fetch_send_the_extra_headers_that_match_the_url() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        commit(&sandbox, &dir, &[("a.txt", "a\n")], "one");
        let server = Server::start();
        let url = server.url("/owner/repo.git");
        let base = server.url("/");
        let (deeper, other_path) = (server.url("/owner/"), server.url("/elsewhere/"));
        for (key, value) in [
            ("http.extraHeader".to_owned(), "X-Plain: 1"),
            (format!("http.{base}.extraHeader"), "X-Host: 2"),
            (format!("http.{deeper}.extraHeader"), "X-Path: 3"),
            (format!("http.{other_path}.extraHeader"), "X-Other-Path: 4"),
            (
                "http.http://127.0.0.1:1/.extraHeader".to_owned(),
                "X-Other-Port: 5",
            ),
            (
                "http.http://other.example/.extraHeader".to_owned(),
                "X-Other-Host: 6",
            ),
            (
                "http.https://127.0.0.1/.extraHeader".to_owned(),
                "X-Other-Scheme: 7",
            ),
        ] {
            set_config(&sandbox, &dir, &key, value);
        }
        sandbox.git(&dir, &["remote", "add", "origin", &url]);
        let repo = Repo::at(&dir);

        assert!(repo.ls_remote(&url).is_err());
        assert!(repo.ls_remote("origin").is_err());
        assert!(
            repo.fetch("origin", &["+refs/heads/main:refs/remotes/origin/main"])
                .is_err()
        );
        assert!(repo.fetch_commit("origin", "refs/heads/main").is_err());

        let heads = server.heads();
        assert!(heads.matches("x-plain: 1").count() >= 4, "{heads}");
        assert!(heads.matches("x-host: 2").count() >= 4, "{heads}");
        assert!(heads.matches("x-path: 3").count() >= 4, "{heads}");
        for other in [
            "x-other-path",
            "x-other-port",
            "x-other-host",
            "x-other-scheme",
        ] {
            assert!(!heads.contains(other), "{other} was sent: {heads}");
        }
    }

    /// The `authorization:` lines of every request head that `server` has recorded.
    fn authorizations(server: &Server) -> Vec<String> {
        let mut lines: Vec<String> = server
            .heads()
            .lines()
            .filter(|line| line.starts_with("authorization:"))
            .map(str::to_owned)
            .collect();
        lines.sort();
        lines
    }

    #[test]
    fn extra_headers_follow_the_order_git_reads_them_in() {
        // Whether the global and the local config set a header scoped to the server (else
        // unscoped): git drops an entry less specific than one it has read.
        let cases = [(true, false), (false, true), (true, true), (false, false)];
        for (global_scoped, local_scoped) in cases {
            let sandbox = Sandbox::with_repo();
            let dir = sandbox.repo();
            let server = Server::start();
            let url = server.url("/repo.git");
            for (level, scoped) in [("--global", global_scoped), ("--local", local_scoped)] {
                let (key, value) = if scoped {
                    (
                        format!("http.{}.extraHeader", server.url("/")),
                        "Authorization: scoped",
                    )
                } else {
                    ("http.extraHeader".to_owned(), "Authorization: plain")
                };
                sandbox.git(&dir, &["config", level, "--add", &key, value]);
            }
            let git = sandbox.command("git").args(["ls-remote", &url]).output();
            assert!(!git.unwrap().status.success());
            let expected = authorizations(&server);

            let mut config = git2::Config::new().unwrap();
            let global = sandbox.path().join("gitconfig");
            config
                .add_file(&global, git2::ConfigLevel::Global, false)
                .unwrap();
            config
                .add_file(&dir.join(".git/config"), git2::ConfigLevel::Local, false)
                .unwrap();
            let mut ours: Vec<String> = http_config::settings(&config, &url)
                .unwrap()
                .headers
                .iter()
                .map(|header| header.to_lowercase())
                .collect();
            ours.sort();

            assert_eq!(
                ours, expected,
                "global scoped {global_scoped}, local scoped {local_scoped}"
            );
        }
    }

    #[test]
    fn an_empty_extra_header_drops_the_ones_before_it() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        let server = Server::start();
        let url = server.url("/repo.git");
        for (key, value) in [
            ("http.extraHeader".to_owned(), "X-Dropped: 1"),
            (format!("http.{}.extraHeader", server.url("/")), ""),
            (format!("http.{}.extraHeader", server.url("/")), "X-Kept: 2"),
        ] {
            set_config(&sandbox, &dir, &key, value);
        }

        assert!(Repo::at(&dir).ls_remote(&url).is_err());

        let heads = server.heads();
        assert!(heads.contains("x-kept: 2"), "{heads}");
        assert!(!heads.contains("x-dropped"), "{heads}");
    }

    #[test]
    fn a_listing_goes_through_the_proxy_a_configured_remote_names() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        let proxy = Server::start();
        sandbox.git(
            &dir,
            &[
                "remote",
                "add",
                "origin",
                "https://unreachable.invalid/repo.git",
            ],
        );
        set_config(&sandbox, &dir, "remote.origin.proxy", &proxy.url(""));

        assert!(Repo::at(&dir).ls_remote("origin").is_err());

        let heads = proxy.heads();
        assert!(heads.contains("connect unreachable.invalid:443"), "{heads}");
    }

    /// An `openssl s_server` on a port of its own with a new self-signed certificate for
    /// `127.0.0.1`; it answers every request with a page that is no git repository.
    struct TlsServer {
        child: std::process::Child,
        port: u16,
        certificate: PathBuf,
    }

    impl TlsServer {
        fn start(sandbox: &Sandbox, name: &str) -> Self {
            let dir = sandbox.path().join(name);
            fs::create_dir(&dir).unwrap();
            let config = dir.join("openssl.cnf");
            // OpenSSL looks an authority up by subject, and a store that already holds one
            // never asks a directory for another with the same subject, so each server's
            // certificate has a subject of its own.
            fs::write(
                &config,
                format!(
                    "[req]\ndistinguished_name = dn\nx509_extensions = ext\nprompt = no\n\
                     [dn]\nCN = {name}\n[ext]\nsubjectAltName = IP:127.0.0.1\n"
                ),
            )
            .unwrap();
            let (certificate, key) = (dir.join("cert.pem"), dir.join("key.pem"));
            let made = sandbox
                .command("openssl")
                .args([
                    "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
                ])
                .arg("-config")
                .arg(&config)
                .arg("-keyout")
                .arg(&key)
                .arg("-out")
                .arg(&certificate)
                .output()
                .unwrap();
            assert!(made.status.success(), "{made:?}");
            let port = TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let log = dir.join("s_server.log");
            let child = sandbox
                .command("openssl")
                .args(["s_server", "-quiet", "-www", "-accept", &port.to_string()])
                .arg("-cert")
                .arg(&certificate)
                .arg("-key")
                .arg(&key)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(fs::File::create(&log).unwrap())
                .spawn()
                .unwrap();
            let mut server = Self {
                child,
                port,
                certificate,
            };
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while TcpStream::connect(("127.0.0.1", port)).is_err() {
                let exited = server.child.try_wait().unwrap();
                assert!(
                    exited.is_none(),
                    "openssl s_server exited with {exited:?}: {}",
                    fs::read_to_string(&log).unwrap_or_default()
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "openssl s_server did not listen within 10s: {}",
                    fs::read_to_string(&log).unwrap_or_default()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            server
        }

        fn url(&self) -> String {
            format!("https://127.0.0.1:{}/repo.git", self.port)
        }

        fn scope(&self) -> String {
            format!("https://127.0.0.1:{}/", self.port)
        }
    }

    impl Drop for TlsServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// libgit2 asks the keychain on macOS, which takes no certificate authority from a file.
    const CA_FILES_IGNORED: bool = cfg!(target_vendor = "apple");

    /// Whether `error` says the TLS certificate of the server was not trusted: OpenSSL's text,
    /// or the one Secure Transport gives on macOS.
    fn is_untrusted(error: &str) -> bool {
        error.contains("certificate is invalid") || error.contains("untrusted connection")
    }

    /// What `ls_remote` of `url` fails with.
    fn ls_remote_error(dir: &Path, url: &str) -> String {
        match Repo::at(dir).ls_remote(url) {
            Ok(refs) => panic!("listed {} refs", refs.len()),
            Err(error) => error.0.to_lowercase(),
        }
    }

    #[test]
    fn ssl_verify_decides_whether_an_untrusted_https_certificate_is_accepted() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        let server = TlsServer::start(&sandbox, "tls");
        let url = server.url();
        let refused = ls_remote_error(&dir, &url);
        assert!(is_untrusted(&refused), "{refused}");

        set_config(&sandbox, &dir, "http.sslVerify", "true");
        assert!(is_untrusted(&ls_remote_error(&dir, &url)));
        let elsewhere = "http.https://other.example/.sslVerify";
        set_config(&sandbox, &dir, elsewhere, "false");
        assert!(is_untrusted(&ls_remote_error(&dir, &url)));

        set_config(
            &sandbox,
            &dir,
            &format!("http.{}.sslVerify", server.scope()),
            "false",
        );
        let accepted = ls_remote_error(&dir, &url);
        assert!(!is_untrusted(&accepted), "{accepted}");

        sandbox.git(&dir, &["config", "http.sslVerify", "false"]);
        sandbox.git(
            &dir,
            &[
                "config",
                &format!("http.{}.sslVerify", server.scope()),
                "true",
            ],
        );
        let scoped_wins = ls_remote_error(&dir, &url);
        assert!(is_untrusted(&scoped_wins), "{scoped_wins}");
    }

    #[test]
    fn ssl_ca_info_and_ssl_ca_path_trust_a_private_authority_for_its_url_only() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        let by_file = TlsServer::start(&sandbox, "by-file");
        let by_path = TlsServer::start(&sandbox, "by-path");
        let unscoped = TlsServer::start(&sandbox, "unscoped");
        for server in [&by_file, &by_path, &unscoped] {
            assert!(is_untrusted(&ls_remote_error(&dir, &server.url())));
        }

        let other = format!(
            "http.{}.sslCAInfo",
            unscoped.scope().replace("127.0.0.1", "localhost")
        );
        set_config(
            &sandbox,
            &dir,
            &other,
            unscoped.certificate.to_str().unwrap(),
        );
        assert!(is_untrusted(&ls_remote_error(&dir, &unscoped.url())));

        let key = format!("http.{}.sslCAInfo", by_file.scope());
        set_config(&sandbox, &dir, &key, by_file.certificate.to_str().unwrap());
        let trusted = ls_remote_error(&dir, &by_file.url());
        assert_eq!(is_untrusted(&trusted), CA_FILES_IGNORED, "{trusted}");

        let hash = sandbox
            .command("openssl")
            .args(["x509", "-noout", "-subject_hash", "-in"])
            .arg(&by_path.certificate)
            .output()
            .unwrap();
        let hash = String::from_utf8(hash.stdout).unwrap();
        let authorities = sandbox.path().join("authorities");
        fs::create_dir(&authorities).unwrap();
        fs::copy(
            &by_path.certificate,
            authorities.join(format!("{}.0", hash.trim())),
        )
        .unwrap();
        let key = format!("http.{}.sslCAPath", by_path.scope());
        set_config(&sandbox, &dir, &key, authorities.to_str().unwrap());
        let trusted = ls_remote_error(&dir, &by_path.url());
        assert_eq!(is_untrusted(&trusted), CA_FILES_IGNORED, "{trusted}");
    }

    #[test]
    #[cfg(not(target_vendor = "apple"))]
    fn a_ca_file_that_cannot_be_loaded_is_named_in_the_error() {
        let sandbox = Sandbox::with_repo();
        let dir = sandbox.repo();
        set_config(&sandbox, &dir, "http.sslCAInfo", "/nonexistent/ca.pem");

        let error = ls_remote_error(&dir, "https://127.0.0.1:1/repo.git");

        assert!(
            error.contains("http.sslcainfo /nonexistent/ca.pem"),
            "{error}"
        );
    }
}
