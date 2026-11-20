//! The checks behind `agt doctor`. Each returns rows; none writes anything.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::core::config::{self, Config};
use crate::core::harness;
use crate::core::local;
use crate::forge::auto::Offline;
use crate::forge::transport::{Cli, TEA_LOGINS_LIST, parse_tea_logins, tea_login_for};
use crate::forge::{hosts, service};
use crate::git::{Repo, global_config_path};
use crate::store::model::Forge;

use super::Row;

/// `tea api -d`, which agt sends every request body through, first shipped in this tea version.
const MIN_TEA: [u64; 2] = [0, 13];
const MISE_PIN: &str = "github:brandonocasey/agt";
const IGNORED: [(&str, &str); 3] = [
    (".worktrees/", ".worktrees/probe"),
    ("/.agt/", ".agt/probe"),
    (harness::SCRATCH_IGNORE, harness::SCRATCH_PROBE),
];

static VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+(?:\.\d+)+").expect("version pattern is valid"));
static ACCOUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:account|as) (\S+)").expect("account pattern is valid"));

pub fn all() -> Vec<Row> {
    let repo = Repo::discover().ok();
    let origin = repo
        .as_ref()
        .and_then(|repo| repo.origin_location().ok().flatten());
    let mut rows = tools();
    rows.extend(forge_auth(origin.as_ref().map(|(host, _)| host.as_str())));
    rows.push(agt_version());
    rows.push(git_ignore(repo.as_ref()));
    rows.extend(state_and_config(repo.as_ref()));
    if let Some(repo) = &repo {
        rows.extend(repository(repo, origin.as_ref()));
    }
    rows
}

fn numbers(text: &str) -> Option<Vec<u64>> {
    VERSION
        .find(text)?
        .as_str()
        .split('.')
        .map(|part| part.parse().ok())
        .collect()
}

/// Whether the version in `text` is `min` or newer.
fn at_least(text: &str, min: [u64; 2]) -> bool {
    numbers(text).is_some_and(|have| have[..] >= min[..])
}

fn mise() -> Cli {
    Cli::other("mise", "AGT_MISE_BIN")
}

fn gh() -> Cli {
    Cli::gh("github.com")
}

fn glab() -> Cli {
    Cli::glab_configured()
}

fn tea() -> Cli {
    Cli::tea()
}

/// The version a tool prints for `--version`; `None` when it is missing or fails.
fn version(cli: &Cli) -> Option<String> {
    let (ok, out, _) = cli.probe(&["--version"])?;
    let found = VERSION.find(&out)?.as_str().to_owned();
    ok.then_some(found)
}

/// The version of the libgit2 agt is built with.
fn libgit2_version() -> String {
    let (major, minor, revision) = git2::Version::get().libgit2_version();
    format!("{major}.{minor}.{revision}")
}

fn tools() -> Vec<Row> {
    let mut rows = Vec::new();
    rows.push(Row::ok("libgit2", libgit2_version()));
    for (name, cli, hint) in [
        (
            service::kind(Forge::Github).tool,
            gh(),
            "install the GitHub CLI: https://cli.github.com",
        ),
        (
            service::kind(Forge::Gitlab).tool,
            glab(),
            "install the GitLab CLI: https://gitlab.com/gitlab-org/cli",
        ),
        (
            service::kind(Forge::Forgejo).tool,
            tea(),
            "install the Gitea CLI: https://gitea.com/gitea/tea",
        ),
        ("mise", mise(), "install mise: https://mise.jdx.dev"),
    ] {
        rows.push(match version(&cli) {
            Some(found)
                if name == service::kind(Forge::Forgejo).tool && !at_least(&found, MIN_TEA) =>
            {
                Row::warn(
                    name,
                    format!("{found}, older than 0.13 (its `tea api` has no `-d`)"),
                    "upgrade tea to 0.13 or newer",
                )
            }
            Some(found) => Row::ok(name, found),
            None => Row::warn(name, "not found", hint),
        });
    }
    rows
}

/// The login of `auth status` output: the account name, the only part of it that is shown.
fn account(out: &str, err: &str) -> Option<String> {
    ACCOUNT
        .captures(&format!("{out}\n{err}"))
        .map(|found| found[1].to_owned())
}

fn forge_cli(forge: Forge, host: Option<&str>) -> Cli {
    match (forge, host) {
        (Forge::Github, Some(host)) => Cli::gh(host),
        (Forge::Github, None) => gh(),
        (Forge::Gitlab, Some(host)) => Cli::glab(host),
        (Forge::Gitlab, None) => glab(),
        (Forge::Forgejo, _) => tea(),
    }
}

/// Logged in to `host` (or to anything, without one) through the forge's CLI: `Ok` with the
/// account, `Err` with why not. A missing CLI is `Err(None)`.
fn login(forge: Forge, host: Option<&str>) -> Result<String, Option<String>> {
    if forge == Forge::Forgejo {
        return tea_login(host);
    }
    let mut args = vec!["auth", "status"];
    args.extend(host.map(|_| "--hostname").into_iter().chain(host));
    let (ok, out, err) = forge_cli(forge, host).probe(&args).ok_or(None)?;
    if ok {
        Ok(account(&out, &err).unwrap_or_else(|| "unknown account".into()))
    } else {
        Err(Some("not logged in".into()))
    }
}

/// Like [`login`] for tea, which has no `auth status`: its login list names the account.
fn tea_login(host: Option<&str>) -> Result<String, Option<String>> {
    let (ok, out, _) = tea().probe(TEA_LOGINS_LIST).ok_or(None)?;
    let logins = if ok {
        parse_tea_logins(&out)
    } else {
        Vec::new()
    };
    let found = match host {
        Some(host) => tea_login_for(&logins, host),
        None => logins.first(),
    };
    match found {
        Some(login) => Ok(login.user.clone().unwrap_or_else(|| login.name.clone())),
        None => Err(Some("not logged in".into())),
    }
}

fn login_hint(forge: Forge, host: Option<&str>) -> String {
    let name = service::kind(forge).tool;
    if forge == Forge::Forgejo {
        return match host {
            Some(host) => format!("{name} login add --url https://{host}"),
            None => format!("{name} login add"),
        };
    }
    match host {
        Some(host) => format!("{name} auth login --hostname {host}"),
        None => format!("{name} auth login"),
    }
}

fn forge_auth(origin_host: Option<&str>) -> Vec<Row> {
    if Offline::current().is_set() {
        return vec![Row::ok("auth", "skipped: offline")];
    }
    let mut rows = Vec::new();
    let own = origin_host.map(|host| {
        let named = hosts::named(host).ok().flatten();
        (host, service::detect(host, named, None))
    });
    if let Some((host, forge)) = own {
        let name = format!("auth {host}");
        rows.push(match forge {
            // It may still be GitHub Enterprise or self-hosted GitLab that nobody logged in to.
            None => Row::warn(
                &name,
                format!("no gh, glab or tea login for {host}; forge features unavailable"),
                format!(
                    "{}, {} or {}",
                    login_hint(Forge::Github, Some(host)),
                    login_hint(Forge::Gitlab, Some(host)),
                    login_hint(Forge::Forgejo, Some(host))
                ),
            ),
            Some(forge) => match login(forge, Some(host)) {
                Ok(account) => {
                    Row::ok(&name, format!("{} as {account}", service::kind(forge).tool))
                }
                Err(why) => Row::fail(
                    &name,
                    format!(
                        "{}: {}",
                        service::kind(forge).tool,
                        why.unwrap_or("not installed".into())
                    ),
                    login_hint(forge, Some(host)),
                ),
            },
        });
    }
    for forge in [Forge::Github, Forge::Gitlab, Forge::Forgejo] {
        if own.is_some_and(|(_, own)| own == Some(forge)) {
            continue;
        }
        let name = format!("auth {}", service::kind(forge).tool);
        // A missing CLI is already a warning in the tools rows.
        match login(forge, None) {
            Ok(account) => rows.push(Row::ok(&name, format!("logged in as {account}"))),
            Err(Some(why)) => rows.push(Row::warn(&name, why, login_hint(forge, None))),
            Err(None) => {}
        }
    }
    rows
}

/// The pinned agt version; the error is the detail and the fix hint.
fn pinned() -> Result<Option<String>, (String, String)> {
    let install = |why: &str| {
        (
            why.to_owned(),
            "install mise to track the pinned agt".to_owned(),
        )
    };
    let (ok, out, err) = mise()
        .probe(&["ls", "--current", "--json"])
        .ok_or_else(|| install("mise is not available"))?;
    if !ok {
        // mise puts warnings first: prefer its `ERROR` line, else the last line it printed.
        let lines = || err.lines().map(str::trim).filter(|line| !line.is_empty());
        let first = lines()
            .find(|line| line.contains("ERROR"))
            .or_else(|| lines().next_back());
        let hint = if err.to_lowercase().contains("not trusted") {
            "mise trust"
        } else {
            "fix the mise error above"
        };
        let why = match first {
            Some(line) => format!("`mise ls --current --json` failed: {line}"),
            None => "`mise ls --current --json` failed".to_owned(),
        };
        return Err((why, hint.to_owned()));
    }
    let listing: Value = serde_json::from_str(&out).map_err(|_| {
        (
            "unreadable mise output".to_owned(),
            "fix the mise error above".to_owned(),
        )
    })?;
    Ok(listing.as_object().and_then(|tools| {
        tools
            .iter()
            .find(|(name, _)| name.starts_with(MISE_PIN))
            .and_then(|(_, entries)| entries.get(0)?.get("version")?.as_str())
            .map(str::to_owned)
    }))
}

fn agt_version() -> Row {
    let running = env!("CARGO_PKG_VERSION");
    match pinned() {
        Err((why, hint)) => Row::warn("agt version", format!("{running} running; {why}"), hint),
        Ok(None) => Row::ok("agt version", format!("{running} (not pinned in mise)")),
        Ok(Some(pin)) if numbers(running) < numbers(&pin) => Row::warn(
            "agt version",
            format!("{running} running, {pin} pinned"),
            "mise install",
        ),
        Ok(Some(pin)) => Row::ok("agt version", format!("{running} running, {pin} pinned")),
    }
}

fn excludes_file() -> Option<PathBuf> {
    global_config_path("core.excludesFile")
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| Some(local::config_home().ok()?.join("git").join("ignore")))
}

/// Whether the excludes-file `text` ignores `probe`, with no repository to ask.
/// The last matching line wins, so a later `!` line re-includes it.
fn excluded(text: &str, probe: &str) -> bool {
    let parts: Vec<&str> = probe.split('/').collect();
    let mut ignored = false;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (negated, pattern) = line
            .strip_prefix('!')
            .map_or((false, line), |rest| (true, rest));
        let directory_only = pattern.ends_with('/');
        let pattern = pattern.trim_end_matches('/');
        let anchored = pattern.contains('/');
        let pattern = pattern.trim_start_matches('/');
        let names = (1..=parts.len()).any(|depth| {
            // The probe itself is a file; a directory-only line cannot name it.
            if directory_only && depth == parts.len() {
                return false;
            }
            if anchored {
                parts[..depth].join("/") == pattern
            } else {
                parts[depth - 1] == pattern
            }
        });
        if names {
            ignored = !negated;
        }
    }
    ignored
}

fn git_ignore(repo: Option<&Repo>) -> Row {
    let file = excludes_file();
    let text = file
        .as_deref()
        .and_then(|path| fs::read_to_string(path).ok())
        .unwrap_or_default();
    let missing: Vec<&str> = IGNORED
        .iter()
        .filter(|(_, probe)| match repo {
            Some(repo) => !repo.is_ignored(probe).unwrap_or(false),
            None => !excluded(&text, probe),
        })
        .map(|(pattern, _)| *pattern)
        .collect();
    if missing.is_empty() {
        return Row::ok(
            "git ignore",
            format!(
                ".worktrees/, /.agt/ and {} are ignored",
                harness::SCRATCH_IGNORE
            ),
        );
    }
    let target = file.map_or("your global git ignore".into(), |path| {
        path.display().to_string()
    });
    Row::warn(
        "git ignore",
        format!("not ignored: {}", missing.join(", ")),
        format!("printf '{}\\n' >> {target}", missing.join("\\n")),
    )
}

/// A directory agt writes to: ok when it is writable, or absent with a writable parent that
/// agt creates it in on first use.
fn directory(name: &str, path: &Path) -> Row {
    if path.is_dir() {
        return if local::writable(path) {
            Row::ok(name, path.display().to_string())
        } else {
            Row::fail(
                name,
                format!("{} is not writable", path.display()),
                format!("chmod u+rwx {}", path.display()),
            )
        };
    }
    match path.ancestors().skip(1).find(|parent| parent.is_dir()) {
        Some(parent) if local::writable(parent) => {
            Row::ok(name, format!("{} (created on first use)", path.display()))
        }
        _ => Row::fail(
            name,
            format!("{} is missing and cannot be created", path.display()),
            format!("mkdir -p {}", path.display()),
        ),
    }
}

fn state_and_config(repo: Option<&Repo>) -> Vec<Row> {
    let mut rows = Vec::new();
    if let Ok(agents) = local::agents_state() {
        rows.push(directory("state dir", &agents));
    }
    let Ok(global) = config::path() else {
        rows.push(Row::fail(
            "config",
            "HOME is not set",
            "set HOME or XDG_CONFIG_HOME",
        ));
        return rows;
    };
    if let Some(dir) = global.parent().and_then(Path::parent) {
        rows.push(directory("config dir", dir));
    }
    rows.push(config_row(
        "global config",
        &global,
        Config::load_from(&global),
    ));
    if let Some(repo) = repo
        && let Ok(main) = repo.main_checkout_root()
    {
        let local = config::repo_file(&main);
        if local.is_file() {
            rows.push(config_row(
                "repo config",
                &local,
                Config::load_for_repo(repo),
            ));
        }
    }
    rows
}

fn config_row(name: &str, path: &Path, loaded: crate::core::error::Result<Config>) -> Row {
    match loaded {
        Ok(_) if path.is_file() => Row::ok(name, format!("{} parses", path.display())),
        Ok(_) => Row::ok(name, format!("{} absent (defaults)", path.display())),
        Err(error) => Row::fail(
            name,
            error.0,
            format!("fix the error in {}", path.display()),
        ),
    }
}

fn repository(repo: &Repo, origin: Option<&(String, String)>) -> Vec<Row> {
    let mut rows = vec![match origin {
        // Host and path only: an origin URL may embed credentials.
        Some((host, path)) => Row::ok("origin", format!("{host}/{path}")),
        None => Row::warn(
            "origin",
            "no origin remote, or one agt cannot read",
            "git remote add origin <url>",
        ),
    }];
    rows.push(match repo.default_branch(None) {
        Ok(branch) => Row::ok(
            "default branch",
            branch.strip_prefix("refs/").unwrap_or(&branch).to_owned(),
        ),
        Err(_) => Row::fail(
            "default branch",
            "cannot find the default branch",
            "git remote set-head origin --auto, or create main",
        ),
    });
    rows
}
