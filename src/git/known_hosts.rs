//! Checks an ssh host key as `ssh` does, against the system and user `known_hosts` files.
//! It matches plain, wildcard, negated, `[host]:port` and hashed entries.
//! libgit2 reads only `~/.ssh/known_hosts`; an unknown key is never accepted.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha1_smol::Sha1;

use crate::core::error::{Error, Result};

/// The port `ssh` uses when none is given.
pub(super) const DEFAULT_SSH_PORT: u16 = 22;

const HASHED_PREFIX: &str = "|1|";
const SHA1_BLOCK_BYTES: usize = 64;

/// What the `known_hosts` files say about a host key.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Known,
    Unknown,
    Revoked,
}

/// The files `ssh` reads, in its order: system, then user.
pub(super) fn default_files() -> Vec<PathBuf> {
    let mut files = vec![
        PathBuf::from("/etc/ssh/ssh_known_hosts"),
        PathBuf::from("/etc/ssh/ssh_known_hosts2"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let ssh = Path::new(&home).join(".ssh");
        files.push(ssh.join("known_hosts"));
        files.push(ssh.join("known_hosts2"));
    }
    files
}

/// Whether `files` list `key` (the raw host key blob) for `host` on `port`. A missing file is
/// skipped; any other read error is returned.
pub(super) fn check(files: &[PathBuf], host: &str, port: u16, key: &[u8]) -> Result<Verdict> {
    let name = if port == DEFAULT_SSH_PORT {
        host.to_ascii_lowercase()
    } else {
        format!("[{}]:{port}", host.to_ascii_lowercase())
    };
    let mut verdict = Verdict::Unknown;
    for file in files {
        let bytes = match fs::read(file) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(Error(format!("read {}: {error}", file.display()))),
        };
        // ssh skips what it cannot read; a stray byte must not fail every host.
        let text = String::from_utf8_lossy(&bytes);
        for line in text.lines() {
            match line_verdict(line, &name, key) {
                Verdict::Revoked => return Ok(Verdict::Revoked),
                Verdict::Known => verdict = Verdict::Known,
                Verdict::Unknown => {}
            }
        }
    }
    Ok(verdict)
}

/// What one `known_hosts` line says about `key` for the host `name`, which is the host, or
/// `[host]:port` off the default port.
fn line_verdict(line: &str, name: &str, key: &[u8]) -> Verdict {
    let mut fields = line.split_whitespace();
    let mut hosts = fields.next().unwrap_or("#");
    let marker = hosts.starts_with('@').then_some(hosts);
    if marker.is_some() {
        hosts = fields.next().unwrap_or("#");
    }
    let (Some(_key_type), Some(encoded)) = (fields.next(), fields.next()) else {
        return Verdict::Unknown;
    };
    if hosts.starts_with('#') || !names_host(hosts, name) {
        return Verdict::Unknown;
    }
    if STANDARD.decode(encoded).ok().as_deref() != Some(key) {
        return Verdict::Unknown;
    }
    match marker {
        None => Verdict::Known,
        Some("@revoked") => Verdict::Revoked,
        // A certificate authority vouches for host certificates, not for this raw key.
        Some(_) => Verdict::Unknown,
    }
}

/// Whether the comma-separated `patterns` of a line name `name`: one pattern matches and no
/// negated one does.
fn names_host(patterns: &str, name: &str) -> bool {
    let mut matched = false;
    for pattern in patterns.split(',') {
        if let Some(hashed) = pattern.strip_prefix(HASHED_PREFIX) {
            matched |= hashed_matches(hashed, name);
        } else if let Some(negated) = pattern.strip_prefix('!') {
            if wildcard_matches(negated.as_bytes(), name.as_bytes()) {
                return false;
            }
        } else {
            matched |= wildcard_matches(pattern.as_bytes(), name.as_bytes());
        }
    }
    matched
}

/// Whether a hashed entry (`salt|hash`, both base64) is the HMAC-SHA1 of `name`.
fn hashed_matches(entry: &str, name: &str) -> bool {
    let Some((salt, hash)) = entry.split_once('|') else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (STANDARD.decode(salt), STANDARD.decode(hash)) else {
        return false;
    };
    hmac_sha1(&salt, name.as_bytes()).as_slice() == hash.as_slice()
}

fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    let mut padded = [0u8; SHA1_BLOCK_BYTES];
    if key.len() > SHA1_BLOCK_BYTES {
        padded[..20].copy_from_slice(&Sha1::from(key).digest().bytes());
    } else {
        padded[..key.len()].copy_from_slice(key);
    }
    let inner_pad = padded.map(|byte| byte ^ 0x36);
    let outer_pad = padded.map(|byte| byte ^ 0x5c);
    let mut inner = Sha1::new();
    inner.update(&inner_pad);
    inner.update(message);
    let mut outer = Sha1::new();
    outer.update(&outer_pad);
    outer.update(&inner.digest().bytes());
    outer.digest().bytes()
}

/// Glob match of `pattern` (`*` any run, `?` one byte) against `name`, ignoring ASCII case.
fn wildcard_matches(pattern: &[u8], name: &[u8]) -> bool {
    match (pattern.first(), name.first()) {
        (None, None) => true,
        (Some(b'*'), _) => {
            wildcard_matches(&pattern[1..], name)
                || (!name.is_empty() && wildcard_matches(pattern, &name[1..]))
        }
        (Some(b'?'), Some(_)) => wildcard_matches(&pattern[1..], &name[1..]),
        (Some(expected), Some(found)) => {
            expected.eq_ignore_ascii_case(found) && wildcard_matches(&pattern[1..], &name[1..])
        }
        _ => false,
    }
}

/// The port an ssh `url` names: `ssh://[user@]host[:port]/path`; any other form (`user@host:path`)
/// uses the default.
pub(super) fn port_of(url: &str) -> u16 {
    let Some(rest) = url.strip_prefix("ssh://") else {
        return DEFAULT_SSH_PORT;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    host.rsplit_once(':')
        .filter(|(name, _)| !name.contains(':') || name.ends_with(']'))
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(DEFAULT_SSH_PORT)
}

/// The error for an unlisted host key, naming the host and how to add it.
pub(super) fn unknown_host_message(host: &str, port: u16, fingerprint: Option<String>) -> String {
    let (connect, scan) = if port == DEFAULT_SSH_PORT {
        (format!("ssh {host}"), format!("ssh-keyscan {host}"))
    } else {
        (
            format!("ssh -p {port} {host}"),
            format!("ssh-keyscan -p {port} {host}"),
        )
    };
    let shown = fingerprint.map_or_else(String::new, |print| format!(" (offered key {print})"));
    format!(
        "the ssh host key of {host} is not in /etc/ssh/ssh_known_hosts or ~/.ssh/known_hosts{shown}; connect once with `{connect}` and accept the key, or add it with `{scan} >> ~/.ssh/known_hosts` after checking its fingerprint"
    )
}

/// The `SHA256:` fingerprint `ssh` prints for a key whose SHA-256 hash is `hash`.
pub(super) fn fingerprint(hash: &[u8; 32]) -> String {
    format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(hash)
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    use super::{Verdict, check, port_of, unknown_host_message};
    use crate::core::testing::Sandbox;

    /// A new ed25519 key as its `<type> <base64>` public half and its raw blob.
    fn key(sandbox: &Sandbox, name: &str) -> (String, Vec<u8>) {
        let path = sandbox.path().join(name);
        let made = sandbox
            .command("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success());
        let public = fs::read_to_string(format!("{}.pub", path.display())).unwrap();
        let mut fields = public.split_whitespace();
        let (kind, encoded) = (fields.next().unwrap(), fields.next().unwrap());
        (
            format!("{kind} {encoded}"),
            STANDARD.decode(encoded).unwrap(),
        )
    }

    /// What `ssh-keygen -F` says: whether `file` has an entry for the host or `[host]:port`.
    fn keygen_finds(sandbox: &Sandbox, file: &Path, host: &str, port: u16) -> bool {
        let name = if port == 22 {
            host.to_owned()
        } else {
            format!("[{host}]:{port}")
        };
        sandbox
            .command("ssh-keygen")
            .args(["-F", &name, "-f"])
            .arg(file)
            .output()
            .unwrap()
            .status
            .success()
    }

    #[test]
    fn host_entries_match_as_ssh_keygen_finds_them() {
        let sandbox = Sandbox::new();
        let (pair, blob) = key(&sandbox, "offered");
        let (other, _) = key(&sandbox, "other");
        let file = sandbox.path().join("known_hosts");
        fs::write(
            &file,
            format!(
                "# comment\n\nplain.example.com {pair}\nMixed.Example.COM {pair}\n\
                 *.wild.example.com,!bad.wild.example.com {pair}\n\
                 [custom.example.com]:2222 {pair}\n\
                 hashed.example.com,hashed.example.org {pair}\n\
                 single?.example.com {pair}\n\
                 @cert-authority ca.example.com {pair}\n\
                 stranger.example.com {other}\n"
            ),
        )
        .unwrap();
        let hashed = sandbox
            .command("ssh-keygen")
            .args(["-H", "-f"])
            .arg(&file)
            .status()
            .unwrap();
        assert!(hashed.success());

        let hosts = [
            ("plain.example.com", 22),
            ("PLAIN.example.com", 22),
            ("plain.example.com", 2222),
            ("mixed.example.com", 22),
            ("a.wild.example.com", 22),
            ("bad.wild.example.com", 22),
            ("wild.example.com", 22),
            ("custom.example.com", 2222),
            ("custom.example.com", 22),
            ("hashed.example.com", 22),
            ("hashed.example.org", 22),
            ("hashed.example.net", 22),
            ("single1.example.com", 22),
            ("single12.example.com", 22),
            ("ca.example.com", 22),
            ("absent.example.com", 22),
        ];
        for (host, port) in hosts {
            let listed = keygen_finds(&sandbox, &file, &host.to_ascii_lowercase(), port);
            let verdict = check(std::slice::from_ref(&file), host, port, &blob).unwrap();
            // `ssh-keygen -F` also finds a certificate authority line, which vouches for no raw key.
            let expected = if listed && host != "ca.example.com" {
                Verdict::Known
            } else {
                Verdict::Unknown
            };
            assert_eq!(verdict, expected, "{host}:{port}");
        }
        assert_eq!(
            check(
                std::slice::from_ref(&file),
                "stranger.example.com",
                22,
                &blob
            )
            .unwrap(),
            Verdict::Unknown,
            "a host listed with another key"
        );
    }

    #[test]
    fn a_key_in_any_listed_file_is_known_and_a_revoked_one_is_not() {
        let sandbox = Sandbox::new();
        let (pair, blob) = key(&sandbox, "offered");
        let first = sandbox.path().join("first");
        let second = sandbox.path().join("second");
        let missing: PathBuf = sandbox.path().join("missing");
        fs::write(&second, format!("later.example.com {pair}\n")).unwrap();
        let files = [missing, first.clone(), second.clone()];

        assert_eq!(
            check(&files, "later.example.com", 22, &blob).unwrap(),
            Verdict::Known
        );
        assert_eq!(
            check(&files, "none.example.com", 22, &blob).unwrap(),
            Verdict::Unknown
        );

        fs::write(&first, format!("@revoked later.example.com {pair}\n")).unwrap();
        assert_eq!(
            check(&files, "later.example.com", 22, &blob).unwrap(),
            Verdict::Revoked
        );
    }

    #[test]
    fn a_stray_non_utf8_byte_does_not_hide_the_hosts_of_a_file() {
        let sandbox = Sandbox::new();
        let (pair, blob) = key(&sandbox, "offered");
        let file = sandbox.path().join("known_hosts");
        let mut text = b"# comment \xff\xfe\n".to_vec();
        text.extend(format!("listed.example.com {pair}\n").bytes());
        fs::write(&file, text).unwrap();

        assert_eq!(
            check(&[file], "listed.example.com", 22, &blob).unwrap(),
            Verdict::Known
        );
    }

    #[test]
    fn an_unreadable_file_is_an_error_not_an_unknown_host() {
        let sandbox = Sandbox::new();
        let directory = sandbox.path().join("known_hosts");
        fs::create_dir(&directory).unwrap();
        let error = check(&[directory], "any.example.com", 22, b"key").unwrap_err();
        assert!(error.0.contains("known_hosts"), "{}", error.0);
    }

    #[test]
    fn the_port_comes_from_an_ssh_url_only() {
        for (url, port) in [
            ("ssh://git@host.example/repo.git", 22),
            ("ssh://git@host.example:2222/repo.git", 2222),
            ("ssh://host.example:2200/repo.git", 2200),
            ("ssh://git@[::1]:2022/repo.git", 2022),
            ("ssh://git@[::1]/repo.git", 22),
            ("git@host.example:group/repo.git", 22),
            ("https://host.example:8443/repo.git", 22),
        ] {
            assert_eq!(port_of(url), port, "{url}");
        }
    }

    #[test]
    fn the_unknown_host_error_names_the_host_and_the_fix() {
        let message = unknown_host_message("git.example.com", 2222, Some("SHA256:abc".into()));
        for part in [
            "git.example.com",
            "SHA256:abc",
            "ssh -p 2222 git.example.com",
            "ssh-keyscan -p 2222 git.example.com >> ~/.ssh/known_hosts",
        ] {
            assert!(message.contains(part), "{part}: {message}");
        }
    }
}
