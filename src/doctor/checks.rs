//! The checks behind `agt doctor`. Each returns rows; none writes anything.

use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::core::config::{self, Config};
use crate::core::local;
use crate::forge::service::{Kind, SERVICES};
use crate::forge::transport::Cli;
use crate::git::{Repo, split_remote};
use crate::store::model::Forge;
use crate::store::{Namespace, Store};

use super::Row;

/// `git merge-tree --write-tree` arrived in this git version.
const MIN_GIT: [u64; 2] = [2, 38];
const MISE_PIN: &str = "github:brandonocasey/agt";
const IGNORED: [(&str, &str); 2] = [
    (".worktrees/", ".worktrees/probe"),
    ("/.cache/agents/scratch/", ".cache/agents/scratch/probe"),
];

static VERSION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+(?:\.\d+)+").expect("version pattern is valid"));
static ACCOUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:account|as) (\S+)").expect("account pattern is valid"));

pub fn all() -> Vec<Row> {
    let repo = Repo::discover().ok();
    let origin = repo
        .as_ref()
        .and_then(|repo| repo.origin_url().ok().flatten())
        .and_then(|url| split_remote(&url));
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

fn git() -> Cli {
    Cli::other("git", "AGT_GIT_BIN")
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

/// The version a tool prints for `--version`; `None` when it is missing or fails.
fn version(cli: &Cli) -> Option<String> {
    let (ok, out, _) = cli.probe(&["--version"])?;
    let found = VERSION.find(&out)?.as_str().to_owned();
    ok.then_some(found)
}

fn tools() -> Vec<Row> {
    let mut rows = Vec::new();
    rows.push(match version(&git()) {
        Some(found) if numbers(&found).is_some_and(|have| have[..] >= MIN_GIT[..]) => {
            Row::ok("git", found)
        }
        Some(found) => Row::fail(
            "git",
            format!("{found}, older than 2.38 (needed for `git merge-tree --write-tree`)"),
            "upgrade git to 2.38 or newer",
        ),
        None => Row::fail("git", "not found", "install git 2.38 or newer"),
    });
    for (name, cli, hint) in [
        ("gh", gh(), "install the GitHub CLI: https://cli.github.com"),
        (
            "glab",
            glab(),
            "install the GitLab CLI: https://gitlab.com/gitlab-org/cli",
        ),
        ("mise", mise(), "install mise: https://mise.jdx.dev"),
    ] {
        rows.push(match version(&cli) {
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
    }
}

fn tool_name(forge: Forge) -> &'static str {
    match forge {
        Forge::Github => "gh",
        Forge::Gitlab => "glab",
    }
}

/// Logged in to `host` (or to anything, without one) through the forge's CLI: `Ok` with the
/// account, `Err` with why not. A missing CLI is `Err(None)`.
fn login(forge: Forge, host: Option<&str>) -> Result<String, Option<String>> {
    let mut args = vec!["auth", "status"];
    args.extend(host.map(|_| "--hostname").into_iter().chain(host));
    let (ok, out, err) = forge_cli(forge, host).probe(&args).ok_or(None)?;
    if ok {
        Ok(account(&out, &err).unwrap_or_else(|| "unknown account".into()))
    } else {
        Err(Some("not logged in".into()))
    }
}

fn login_hint(forge: Forge, host: Option<&str>) -> String {
    let name = tool_name(forge);
    match host {
        Some(host) => format!("{name} auth login --hostname {host}"),
        None => format!("{name} auth login"),
    }
}

fn forge_for(host: &str) -> Option<&'static Kind> {
    SERVICES
        .iter()
        .find(|kind| (kind.well_known)(host) || (kind.configured)(host))
        .or_else(|| SERVICES.iter().find(|kind| (kind.probe)(host)))
}

fn forge_auth(origin_host: Option<&str>) -> Vec<Row> {
    let mut rows = Vec::new();
    let own = origin_host.map(|host| (host, forge_for(host).map(|kind| kind.forge)));
    if let Some((host, forge)) = own {
        let name = format!("auth {host}");
        rows.push(match forge {
            // It may still be GitHub Enterprise or self-hosted GitLab that nobody logged in to.
            None => Row::warn(
                &name,
                format!("no gh or glab login for {host}; forge features unavailable"),
                format!(
                    "{} or {}",
                    login_hint(Forge::Github, Some(host)),
                    login_hint(Forge::Gitlab, Some(host))
                ),
            ),
            Some(forge) => match login(forge, Some(host)) {
                Ok(account) => Row::ok(&name, format!("{} as {account}", tool_name(forge))),
                Err(why) => Row::fail(
                    &name,
                    format!(
                        "{}: {}",
                        tool_name(forge),
                        why.unwrap_or("not installed".into())
                    ),
                    login_hint(forge, Some(host)),
                ),
            },
        });
    }
    for forge in [Forge::Github, Forge::Gitlab] {
        if own.is_some_and(|(_, own)| own == Some(forge)) {
            continue;
        }
        let name = format!("auth {}", tool_name(forge));
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
    let configured = git()
        .probe(&["config", "--path", "core.excludesFile"])
        .filter(|(ok, ..)| *ok)
        .map(|(_, out, _)| out.trim().to_owned())
        .filter(|path| !path.is_empty());
    configured
        .map(PathBuf::from)
        .or_else(|| Some(local::config_home().ok()?.join("git").join("ignore")))
}

fn git_ignore(repo: Option<&Repo>) -> Row {
    let file = excludes_file();
    let text = file
        .as_deref()
        .and_then(|path| fs::read_to_string(path).ok())
        .unwrap_or_default();
    let missing: Vec<&str> = IGNORED
        .iter()
        .filter(|(pattern, probe)| match repo {
            Some(repo) => !repo
                .succeeds(&["check-ignore", "-q", "--", probe])
                .unwrap_or(false),
            None => !text
                .lines()
                .any(|line| line.trim().trim_start_matches('/') == pattern.trim_start_matches('/')),
        })
        .map(|(pattern, _)| *pattern)
        .collect();
    if missing.is_empty() {
        return Row::ok(
            "git ignore",
            ".worktrees/ and /.cache/agents/scratch/ are ignored",
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

fn writable(path: &Path) -> bool {
    CString::new(path.as_os_str().as_encoded_bytes()).is_ok_and(|path| {
        // SAFETY: `access` reads the NUL-terminated path, which `path` keeps alive for the call.
        unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
    })
}

/// A directory agt writes to: ok when it is writable, or absent with a writable parent that
/// agt creates it in on first use.
fn directory(name: &str, path: &Path) -> Row {
    if path.is_dir() {
        return if writable(path) {
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
        Some(parent) if writable(parent) => {
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
        let local = main.join(".config/agt.toml");
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
    let mut stray = 0;
    let mut unreadable = None;
    for namespace in [Namespace::Reviews, Namespace::Issues] {
        match Store::inspect_repo(namespace, repo).and_then(|store| store.stray_locks()) {
            Ok(found) => stray += found.len(),
            Err(error) => unreadable = Some(error.0),
        }
    }
    rows.push(match (stray, unreadable) {
        (0, None) => Row::ok("agt locks", "no stray lock files"),
        (0, Some(why)) => Row::warn("agt locks", why, "check .agt/ in the main checkout"),
        (count, _) => Row::warn(
            "agt locks",
            format!("{count} stray lock file(s)"),
            "agt cleanup",
        ),
    });
    rows
}
