//! The user's `agt` configuration: `$XDG_CONFIG_HOME/agt/config.toml`, overridden by the main
//! checkout's `.config/agt.toml`. A missing file means defaults. The file holds no secrets;
//! credentials come from the forge CLIs.
//!
//! Unknown keys are an error, so a misspelled key is reported instead of silently ignored.
//! Every error names the file and the key.
//!
//! One `[[tracker]]` list serves issues and pull requests. `[sync]` sets the pull and push
//! modes once, `[issues]` and `[pull_requests]` override them per kind, and a tracker overrides
//! both. The older `[forge] post`, `[sync] auto_pull`, `[sync] auto_push` and
//! `[[issues.tracker]]` keys still load: they are rewritten to the new keys and named on stderr.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::core::error::{Error, Result, fail};
use crate::core::local;
use crate::forge::{hosts, service};
use crate::git::RemoteLocation;
use crate::store::model::Forge;

#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Overrides of `[sync]` for issues.
    #[serde(default)]
    pub issues: Modes,
    /// Overrides of `[sync]` for reviews and `agt pr`.
    #[serde(default)]
    pub pull_requests: Modes,
    #[serde(default)]
    pub worktree: Worktree,
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub tracker: Vec<TrackerEntry>,
    /// The trackers came from the repository's own file, which only counts for remotes the
    /// repository has.
    #[serde(skip)]
    pub repo_trackers: bool,
    /// The global file alone: the repository file may raise a tracker's `push` above what this
    /// gives but never below it.
    #[serde(skip)]
    global_floor: Option<Box<Config>>,
}

/// What agt does in one direction between the machine and a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// No forge calls in that direction.
    Off,
    /// Only explicit commands reach the forge.
    Manual,
    /// Explicit commands, plus automatic stale reads and pushes after a change.
    Auto,
}

/// The two kinds of work a tracker can serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Issues,
    PullRequests,
}

impl Kind {
    /// The configuration section that overrides `[sync]` for this kind.
    pub fn section(self) -> &'static str {
        match self {
            Self::Issues => "issues",
            Self::PullRequests => "pull_requests",
        }
    }
}

/// Pull and push modes that override a broader setting; `None` defers to it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Modes {
    pub pull: Option<Mode>,
    pub push: Option<Mode>,
}

/// Which git remotes become trackers on their own.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteSet {
    /// `origin` only.
    Origin,
    /// Every remote whose URL names a repository on a known forge.
    #[default]
    All,
    /// Only the configured `[[tracker]]` entries.
    None,
}

/// The defaults for issues and pull requests, and how often a read syncs.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncConfig {
    /// Default `manual`.
    pub pull: Option<Mode>,
    /// Default `off`.
    pub push: Option<Mode>,
    /// A read syncs when the last sync is older than this many seconds.
    #[serde(default = "default_max_age_secs")]
    pub max_age_secs: u64,
    #[serde(default)]
    pub remotes: RemoteSet,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            pull: None,
            push: None,
            max_age_secs: default_max_age_secs(),
            remotes: RemoteSet::default(),
        }
    }
}

fn default_max_age_secs() -> u64 {
    300
}

#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Worktree {
    /// Globs of repository directories `agt worktree list --all-repos` scans; `~` is the home
    /// directory. `None` means the default layout.
    pub roots: Option<Vec<String>>,
    #[serde(default)]
    pub hooks: Hooks,
}

/// Shell commands `agt worktree` runs in a worktree's root, in order, stopping at the first
/// failure.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    /// After `agt worktree create` makes a new worktree.
    #[serde(default)]
    pub create: Vec<String>,
    /// Before a worktree is backed up and removed.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// One `[[tracker]]` as written.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackerEntry {
    /// Needed only for a host agt cannot identify.
    pub forge: Option<Forge>,
    pub host: String,
    /// `owner/repo` (GitLab: the full project path).
    pub repo: String,
    /// The git remote this tracker is; filled for trackers that come from remotes.
    pub remote: Option<String>,
    pub pull: Option<Mode>,
    pub push: Option<Mode>,
    /// `false` drops the tracker, also when it came from a remote.
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub issues: KindSetting,
    #[serde(default)]
    pub pull_requests: KindSetting,
    /// Set by the `[[issues.tracker]]` migration: the entry configures issues only, so it never
    /// adds a tracker to the pull request list nor changes a remote's entry there.
    #[serde(default)]
    pub legacy_issues_only: bool,
}

fn enabled() -> bool {
    true
}

/// `true` or `false` turns a kind on or off for a tracker; a table also overrides fields for
/// that kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindSetting {
    Enabled(bool),
    Overrides(KindOverrides),
}

impl Default for KindSetting {
    fn default() -> Self {
        Self::Enabled(true)
    }
}

/// Fields one kind of one tracker overrides.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KindOverrides {
    pub pull: Option<Mode>,
    pub push: Option<Mode>,
    /// Issues only.
    pub scope: Option<Scope>,
    /// Issues only.
    pub since_days: Option<u32>,
}

impl<'de> Deserialize<'de> for KindSetting {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Setting;
        impl<'de> Visitor<'de> for Setting {
            type Value = KindSetting;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("true, false or a table of overrides")
            }

            fn visit_bool<E: serde::de::Error>(
                self,
                value: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(KindSetting::Enabled(value))
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                KindOverrides::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    .map(KindSetting::Overrides)
            }
        }
        deserializer.deserialize_any(Setting)
    }
}

impl KindSetting {
    fn is_enabled(&self) -> bool {
        !matches!(self, Self::Enabled(false))
    }

    fn overrides(&self) -> KindOverrides {
        match self {
            Self::Overrides(overrides) => overrides.clone(),
            Self::Enabled(_) => KindOverrides::default(),
        }
    }
}

/// A tracker for one kind, with its modes resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracker {
    pub forge: Forge,
    pub host: String,
    pub repo: String,
    /// The git remote it is, when it is one.
    pub remote: Option<String>,
    /// Issues only.
    pub scope: Scope,
    /// Issues only.
    pub since_days: u32,
    pub pull: Mode,
    /// `None` when no level set it, which `agt pr` writes read as allowed and everything else as
    /// `off`.
    pub push: Option<Mode>,
    /// The tracker itself (or its kind table) set `push`, so an explicit command naming this
    /// tracker is refused, not just kept local.
    pub push_pinned: bool,
}

impl Tracker {
    pub fn push_mode(&self) -> Mode {
        self.push.unwrap_or(Mode::Off)
    }

    /// `host/repo`.
    pub fn name(&self) -> String {
        format!("{}/{}", self.host, self.repo)
    }

    pub fn is_at(&self, host: &str, repo: &str) -> bool {
        self.host.eq_ignore_ascii_case(host) && self.repo.eq_ignore_ascii_case(repo)
    }
}

/// Which issues a tracker pulls.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    Involved,
    Assigned,
    All,
}

fn default_since_days() -> u32 {
    90
}

/// A main checkout's repository config file, which overrides the global one.
pub fn repo_file(main: &Path) -> PathBuf {
    main.join(".config").join("agt.toml")
}

/// The old keys of one file that a newer key replaces.
#[derive(Default)]
struct Legacy {
    post: Option<bool>,
    auto_push: Option<bool>,
}

static WARNED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

/// Prints `line` to stderr unless this process printed it already.
fn warn_once(line: String) {
    let fresh = WARNED
        .lock()
        .map_or(true, |mut seen| seen.insert(line.clone()));
    if fresh {
        eprintln!("{line}");
    }
}

fn deprecated(path: &Path, old: &str, new: &str) {
    warn_once(format!(
        "agt: config: {old} in {} is deprecated; use {new}",
        path.display()
    ));
}

impl Config {
    /// The configuration from the default path, or defaults when the file is missing.
    pub fn load() -> Result<Self> {
        Self::load_from(&path()?)
    }

    pub fn load_for_repo(repo: &crate::git::Repo) -> Result<Self> {
        let global = path()?;
        let local = repo_file(&repo.main_checkout_root()?);
        Self::load_paths(&global, &local)
    }

    fn load_paths(global: &Path, local: &Path) -> Result<Self> {
        let (mut merged, global_legacy) = read_value(global)?;
        let (overrides, local_legacy) = read_value(local)?;
        let repo_trackers = overrides.get("tracker").is_some();
        let global_value = merged.clone();
        let mut floor_value = global_value.clone();
        apply_legacy_push(&mut floor_value, &global_legacy, &Legacy::default());
        let floor: Self = floor_value.try_into().map_err(|error: toml::de::Error| {
            Error(format!("parse {}: {}", global.display(), error.message()))
        })?;
        merge_values(&mut merged, overrides);
        raise_push(&mut merged, &global_value);
        apply_legacy_push(&mut merged, &global_legacy, &local_legacy);
        let mut config: Self = merged.try_into().map_err(|error: toml::de::Error| {
            Error(format!("parse {}: {}", local.display(), error.message()))
        })?;
        config.repo_trackers = repo_trackers;
        config.global_floor = Some(Box::new(floor));
        Ok(config)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let (mut value, legacy) = read_value(path)?;
        apply_legacy_push(&mut value, &legacy, &Legacy::default());
        value.try_into().map_err(|error: toml::de::Error| {
            Error(format!("parse {}: {}", path.display(), error.message()))
        })
    }

    fn validate(&self, path: &Path) -> Result<()> {
        let no_off = |key: &str, mode: Option<Mode>| {
            if mode == Some(Mode::Off) {
                return fail(format!(
                    "{}: {key} = \"off\" is not allowed; pull is \"manual\" or \"auto\" (pulling is always allowed on an explicit command)",
                    path.display()
                ));
            }
            Ok(())
        };
        no_off("[sync] pull", self.sync.pull)?;
        no_off("[issues] pull", self.issues.pull)?;
        no_off("[pull_requests] pull", self.pull_requests.pull)?;
        for (index, tracker) in self.tracker.iter().enumerate() {
            let at = format!("tracker[{index}]");
            no_off(&format!("{at}.pull"), tracker.pull)?;
            for (name, setting) in [
                ("issues", &tracker.issues),
                ("pull_requests", &tracker.pull_requests),
            ] {
                if let KindSetting::Overrides(overrides) = setting {
                    no_off(&format!("{at}.{name}.pull"), overrides.pull)?;
                }
            }
            let invalid =
                |key: &str, why: &str| fail(format!("{}: {at}.{key} {why}", path.display()));
            if tracker.host.trim().is_empty() {
                return invalid("host", "must not be empty");
            }
            let mut parts = tracker.repo.split('/');
            if tracker.repo.split('/').any(str::is_empty) || parts.nth(1).is_none() {
                return invalid("repo", "must look like owner/repo");
            }
            if tracker.forge.is_none() && forge_of(&tracker.host).is_none() {
                return invalid("forge", "is needed: agt does not know this host's forge");
            }
            for (name, setting) in [
                ("issues", &tracker.issues),
                ("pull_requests", &tracker.pull_requests),
            ] {
                let KindSetting::Overrides(overrides) = setting else {
                    continue;
                };
                if overrides.since_days == Some(0) {
                    return invalid(&format!("{name}.since_days"), "must be at least 1");
                }
                if name == "pull_requests"
                    && (overrides.scope.is_some() || overrides.since_days.is_some())
                {
                    return invalid(
                        "pull_requests",
                        "takes only pull and push; scope and since_days are for issues",
                    );
                }
            }
        }
        Ok(())
    }

    /// The pull mode that applies to `kind` before any tracker overrides it.
    pub fn pull_for(&self, kind: Kind) -> Mode {
        self.modes(kind)
            .pull
            .or(self.sync.pull)
            .unwrap_or(Mode::Manual)
    }

    /// The push mode that applies to `kind` before any tracker overrides it; `None` when unset.
    pub fn push_for(&self, kind: Kind) -> Option<Mode> {
        self.modes(kind).push.or(self.sync.push)
    }

    fn modes(&self, kind: Kind) -> Modes {
        match kind {
            Kind::Issues => self.issues,
            Kind::PullRequests => self.pull_requests,
        }
    }

    /// Whether any setting could make `kind` pull (`pull`) or push (`!pull`) automatically. A
    /// cheap check that needs neither git nor a forge, so a read with nothing automatic to do
    /// resolves no tracker.
    pub fn may_be_automatic(&self, kind: Kind, pull: bool) -> bool {
        let pick = |modes: Modes| if pull { modes.pull } else { modes.push };
        let auto = |mode: Option<Mode>| mode == Some(Mode::Auto);
        auto(pick(Modes {
            pull: self.sync.pull,
            push: self.sync.push,
        })) || auto(pick(self.modes(kind)))
            || self.tracker.iter().any(|entry| {
                let setting = match kind {
                    Kind::Issues => &entry.issues,
                    Kind::PullRequests => &entry.pull_requests,
                };
                auto(pick(Modes {
                    pull: entry.pull,
                    push: entry.push,
                })) || auto(pick(Modes {
                    pull: setting.overrides().pull,
                    push: setting.overrides().push,
                }))
            })
    }

    /// The trackers that serve `kind`, with their modes resolved.
    ///
    /// Trackers start as the remotes `[sync] remotes` picks, when their host is a known forge.
    /// A `[[tracker]]` at the same host and repository overrides such a tracker; any other
    /// is added. A tracker with `enabled = false`, or whose kind is `false`, is dropped.
    pub fn trackers(&self, kind: Kind, remotes: &[RemoteLocation]) -> Vec<Tracker> {
        self.trackers_from(kind, remotes, self.sync.remotes)
    }

    /// Like [`Config::trackers`], with `set` in place of `[sync] remotes`.
    pub fn trackers_from(
        &self,
        kind: Kind,
        remotes: &[RemoteLocation],
        set: RemoteSet,
    ) -> Vec<Tracker> {
        struct Draft<'a> {
            forge: Forge,
            host: String,
            repo: String,
            remote: Option<String>,
            entry: Option<&'a TrackerEntry>,
        }
        let mut drafts: Vec<Draft<'_>> = Vec::new();
        let picked = remotes.iter().filter(|remote| match set {
            RemoteSet::Origin => remote.name == "origin",
            RemoteSet::All => true,
            RemoteSet::None => false,
        });
        for remote in picked {
            let Some(forge) = forge_of(&remote.host) else {
                continue;
            };
            if drafts
                .iter()
                .any(|draft| same(&draft.host, &draft.repo, &remote.host, &remote.repo))
            {
                continue;
            }
            drafts.push(Draft {
                forge,
                host: remote.host.clone(),
                repo: remote.repo.clone(),
                remote: Some(remote.name.clone()),
                entry: None,
            });
        }
        for entry in &self.tracker {
            if kind == Kind::PullRequests && entry.legacy_issues_only {
                continue;
            }
            let remote = remotes
                .iter()
                .find(|remote| same(&remote.host, &remote.repo, &entry.host, &entry.repo));
            if self.repo_trackers && remote.is_none() {
                warn_once(format!(
                    "agt: config: ignoring [[tracker]] {}/{} in the repository's .config/agt.toml: \
                     no remote of this repository points there",
                    entry.host, entry.repo
                ));
                continue;
            }
            let Some(forge) = entry.forge.or_else(|| forge_of(&entry.host)) else {
                continue;
            };
            let named = entry
                .remote
                .clone()
                .or_else(|| remote.map(|remote| remote.name.clone()));
            match drafts
                .iter_mut()
                .find(|draft| same(&draft.host, &draft.repo, &entry.host, &entry.repo))
            {
                Some(draft) => {
                    draft.forge = forge;
                    draft.entry = Some(entry);
                    if named.is_some() {
                        draft.remote = named;
                    }
                }
                None => drafts.push(Draft {
                    forge,
                    host: entry.host.clone(),
                    repo: entry.repo.clone(),
                    remote: named,
                    entry: Some(entry),
                }),
            }
        }
        drafts
            .into_iter()
            .filter_map(|draft| {
                let entry = draft.entry;
                let setting = entry.map(|entry| match kind {
                    Kind::Issues => &entry.issues,
                    Kind::PullRequests => &entry.pull_requests,
                });
                if entry.is_some_and(|entry| !entry.enabled)
                    || setting.is_some_and(|setting| !setting.is_enabled())
                {
                    return None;
                }
                let overrides = setting.map(KindSetting::overrides).unwrap_or_default();
                let tracker_pull = entry.and_then(|entry| entry.pull);
                let (push, push_pinned) = {
                    let own = self.tracker_push(kind, &draft.host, &draft.repo);
                    match self.global_floor.as_ref() {
                        Some(floor) => {
                            let low = floor.tracker_push(kind, &draft.host, &draft.repo);
                            if low.0 > own.0 { low } else { own }
                        }
                        None => own,
                    }
                };
                Some(Tracker {
                    forge: draft.forge,
                    host: draft.host,
                    repo: draft.repo,
                    remote: draft.remote,
                    scope: overrides.scope.unwrap_or_default(),
                    since_days: overrides.since_days.unwrap_or_else(default_since_days),
                    pull: overrides
                        .pull
                        .or(tracker_pull)
                        .unwrap_or_else(|| self.pull_for(kind)),
                    push,
                    push_pinned,
                })
            })
            .collect()
    }

    /// The push mode of the tracker at `host` and `repo` for `kind` from this file set alone, and
    /// whether the tracker or its kind table set it.
    fn tracker_push(&self, kind: Kind, host: &str, repo: &str) -> (Option<Mode>, bool) {
        let entry = self
            .tracker
            .iter()
            .find(|entry| same(&entry.host, &entry.repo, host, repo));
        let kind_push = entry.and_then(|entry| {
            match kind {
                Kind::Issues => &entry.issues,
                Kind::PullRequests => &entry.pull_requests,
            }
            .overrides()
            .push
        });
        let pinned = kind_push.or(entry.and_then(|entry| entry.push));
        (pinned.or(self.push_for(kind)), pinned.is_some())
    }

    /// Like [`Config::trackers`], with the remotes of `repo`.
    pub fn trackers_for(&self, repo: &crate::git::Repo, kind: Kind) -> Result<Vec<Tracker>> {
        Ok(self.trackers(kind, &repo.remote_locations()?))
    }
}

fn same(host: &str, repo: &str, other_host: &str, other_repo: &str) -> bool {
    host.eq_ignore_ascii_case(other_host) && repo.eq_ignore_ascii_case(other_repo)
}

/// The forge of `host` when it is known without asking a CLI.
pub fn forge_of(host: &str) -> Option<Forge> {
    service::detect_known(
        &host.to_ascii_lowercase(),
        hosts::named(host).ok().flatten(),
        None,
    )
}

/// A file's value, rewritten to the current keys and checked on its own.
fn read_value(path: &Path) -> Result<(toml::Value, Legacy)> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return fail(format!("read {}: {error}", path.display())),
    };
    let mut value: toml::Value = toml::from_str(&text)
        .map_err(|error| Error(format!("parse {}: {error}", path.display())))?;
    let legacy = migrate(&mut value, path)?;
    check_choices(&value, path)?;
    let config: Config = value.clone().try_into().map_err(|error: toml::de::Error| {
        Error(format!("parse {}: {}", path.display(), error.message()))
    })?;
    config.validate(path)?;
    Ok((value, legacy))
}

/// Names the key and the choices when a `pull`, `push` or `remotes` value is not one of them,
/// which serde's own message does not. Values of another type are left to serde.
fn check_choices(value: &toml::Value, path: &Path) -> Result<()> {
    const PULL: &[&str] = &["manual", "auto"];
    const PUSH: &[&str] = &["off", "manual", "auto"];
    const REMOTES: &[&str] = &["origin", "all", "none"];
    let check = |table: Option<&toml::Value>, at: &str, key: &str, choices: &[&str]| {
        let Some(text) = table
            .and_then(|table| table.get(key))
            .and_then(toml::Value::as_str)
        else {
            return Ok(());
        };
        // `off` for a pull is refused later, by `validate`, with its own explanation.
        if choices.contains(&text) || (key == "pull" && text == "off") {
            return Ok(());
        }
        let list = match choices {
            [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
            [] => String::new(),
        };
        fail(format!(
            "{}: {at} {key} = {text:?}: expected {list}",
            path.display()
        ))
    };
    let modes = |table: Option<&toml::Value>, at: &str| {
        check(table, at, "pull", PULL)?;
        check(table, at, "push", PUSH)
    };
    modes(value.get("sync"), "[sync]")?;
    check(value.get("sync"), "[sync]", "remotes", REMOTES)?;
    modes(value.get("issues"), "[issues]")?;
    modes(value.get("pull_requests"), "[pull_requests]")?;
    if let Some(toml::Value::Array(entries)) = value.get("tracker") {
        for entry in entries {
            modes(Some(entry), "[[tracker]]")?;
            modes(entry.get("issues"), "[[tracker]] issues")?;
            modes(entry.get("pull_requests"), "[[tracker]] pull_requests")?;
        }
    }
    Ok(())
}

/// Rewrites the old keys of one file to the new ones and warns. `[forge] post` and
/// `[sync] auto_push` combine across files, so they are returned instead.
fn migrate(value: &mut toml::Value, path: &Path) -> Result<Legacy> {
    let mut legacy = Legacy::default();
    let Some(root) = value.as_table_mut() else {
        return Ok(legacy);
    };
    let both = |old: &str, new: &str| {
        Error(format!(
            "{}: {old} and {new} are both set; remove the deprecated {old}",
            path.display()
        ))
    };
    let flag = |table: &toml::Table, key: &str, at: &str| match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Boolean(flag)) => Ok(Some(*flag)),
        Some(_) => fail(format!("{}: {at} must be true or false", path.display())),
    };
    // The value `[forge] post` maps to; `auto_push` in the same file decides manual or auto.
    let auto_push = root
        .get("sync")
        .and_then(|sync| sync.get("auto_push"))
        .and_then(toml::Value::as_bool)
        == Some(true);
    if let Some(forge) = root.get_mut("forge").and_then(toml::Value::as_table_mut)
        && forge.contains_key("post")
    {
        legacy.post = flag(forge, "post", "forge.post")?;
        forge.remove("post");
        let mapped = match (legacy.post, auto_push) {
            (Some(true), true) => "auto",
            (Some(true), false) => "manual",
            _ => "off",
        };
        deprecated(
            path,
            "`[forge] post`",
            &format!("`[sync] push = \"{mapped}\"`"),
        );
        if forge.is_empty() {
            root.remove("forge");
        }
    }
    if let Some(sync) = root.get_mut("sync").and_then(toml::Value::as_table_mut) {
        if let Some(auto_push) = flag(sync, "auto_push", "sync.auto_push")? {
            legacy.auto_push = Some(auto_push);
            sync.remove("auto_push");
            deprecated(path, "`[sync] auto_push`", "`[sync] push = \"auto\"`");
        }
        if let Some(auto_pull) = flag(sync, "auto_pull", "sync.auto_pull")? {
            if sync.contains_key("pull") {
                return Err(both("`[sync] auto_pull`", "`[sync] pull`"));
            }
            sync.remove("auto_pull");
            sync.insert(
                "pull".into(),
                toml::Value::String(if auto_pull { "auto" } else { "manual" }.into()),
            );
            deprecated(
                path,
                "`[sync] auto_pull`",
                "`[sync] pull = \"auto\"` or `\"manual\"`",
            );
        }
        let sync_pushes = sync.contains_key("push");
        if sync_pushes && (legacy.post.is_some() || legacy.auto_push.is_some()) {
            return Err(both(
                "`[forge] post` or `[sync] auto_push`",
                "`[sync] push`",
            ));
        }
    }
    if let Some(issues) = root.get_mut("issues").and_then(toml::Value::as_table_mut)
        && let Some(old) = issues.remove("tracker")
    {
        deprecated(path, "`[[issues.tracker]]`", "`[[tracker]]`");
        let toml::Value::Array(old) = old else {
            return fail(format!("{}: issues.tracker must be a list", path.display()));
        };
        let mut moved = Vec::new();
        for entry in old {
            moved.push(upgrade_tracker(entry, path)?);
        }
        match root.get_mut("tracker") {
            Some(toml::Value::Array(list)) => list.extend(moved),
            _ => {
                root.insert("tracker".into(), toml::Value::Array(moved));
            }
        }
    }
    Ok(legacy)
}

/// An `[[issues.tracker]]` entry as a `[[tracker]]` for issues only.
fn upgrade_tracker(entry: toml::Value, path: &Path) -> Result<toml::Value> {
    let toml::Value::Table(mut old) = entry else {
        return fail(format!(
            "{}: issues.tracker entries must be tables",
            path.display()
        ));
    };
    let mut kind = toml::Table::new();
    for key in ["scope", "since_days"] {
        if let Some(value) = old.remove(key) {
            kind.insert(key.into(), value);
        }
    }
    match old.remove("push") {
        None | Some(toml::Value::Boolean(true)) => {}
        Some(toml::Value::Boolean(false)) => {
            kind.insert("push".into(), toml::Value::String("off".into()));
        }
        Some(_) => {
            return fail(format!(
                "{}: issues.tracker push must be true or false",
                path.display()
            ));
        }
    }
    if !kind.is_empty() {
        old.insert("issues".into(), toml::Value::Table(kind));
    }
    old.insert("legacy_issues_only".into(), toml::Value::Boolean(true));
    Ok(toml::Value::Table(old))
}

/// Sets `[sync] push` from the old keys when no file sets it. Posting was on when either file
/// turned it on, and `auto_push` took the repository's value over the global one.
fn apply_legacy_push(merged: &mut toml::Value, global: &Legacy, local: &Legacy) {
    let known = [global.post, local.post, global.auto_push, local.auto_push];
    if known.iter().all(Option::is_none) {
        return;
    }
    let posts = global.post == Some(true) || local.post == Some(true);
    let auto = local.auto_push.or(global.auto_push) == Some(true);
    let mode = match (posts, auto) {
        (false, _) => "off",
        (true, false) => "manual",
        (true, true) => "auto",
    };
    let Some(root) = merged.as_table_mut() else {
        return;
    };
    let sync = root
        .entry("sync")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let Some(sync) = sync.as_table_mut() {
        sync.entry("push")
            .or_insert_with(|| toml::Value::String(mode.into()));
    }
}

fn rank(value: Option<&toml::Value>) -> Option<u8> {
    match value.and_then(toml::Value::as_str)? {
        "off" => Some(0),
        "manual" => Some(1),
        "auto" => Some(2),
        _ => None,
    }
}

/// Lets the repository file raise a `push` mode but never lower the global one: wherever the
/// global file sets a `push`, the merged one is at least as high. A `[[tracker]]` is the same
/// tracker when host and repository match.
fn raise_push(merged: &mut toml::Value, global: &toml::Value) {
    for section in ["sync", "issues", "pull_requests"] {
        if let (Some(merged), Some(global)) = (
            merged.get_mut(section).and_then(toml::Value::as_table_mut),
            global.get(section).and_then(toml::Value::as_table),
        ) {
            raise_in(merged, global);
        }
    }
    let (Some(toml::Value::Array(entries)), Some(toml::Value::Array(globals))) =
        (merged.get_mut("tracker"), global.get("tracker"))
    else {
        return;
    };
    let identity = |value: &toml::Value| {
        let text = |key: &str| {
            value
                .get(key)
                .and_then(toml::Value::as_str)
                .map(str::to_lowercase)
        };
        (text("host"), text("repo"))
    };
    for entry in entries {
        let Some(before) = globals
            .iter()
            .find(|before| identity(before) == identity(entry))
        else {
            continue;
        };
        let (Some(entry), Some(before)) = (entry.as_table_mut(), before.as_table()) else {
            continue;
        };
        raise_in(entry, before);
        for kind in ["issues", "pull_requests"] {
            let Some(before) = before.get(kind).and_then(toml::Value::as_table) else {
                continue;
            };
            if !before.contains_key("push") {
                continue;
            }
            let slot = entry
                .entry(kind)
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if let Some(slot) = slot.as_table_mut() {
                raise_in(slot, before);
            }
        }
    }
}

/// Raises `merged`'s `push` to `global`'s when it is lower or unset.
fn raise_in(merged: &mut toml::Table, global: &toml::Table) {
    let Some(before) = rank(global.get("push")) else {
        return;
    };
    if rank(merged.get("push")).is_none_or(|after| after < before)
        && let Some(push) = global.get("push")
    {
        merged.insert("push".into(), push.clone());
    }
}

fn merge_values(base: &mut toml::Value, overrides: toml::Value) {
    if let (Some(base), toml::Value::Table(overrides)) = (base.as_table_mut(), &overrides) {
        for (key, value) in overrides {
            match base.get_mut(key) {
                Some(existing) => merge_values(existing, value.clone()),
                None => {
                    base.insert(key.clone(), value.clone());
                }
            }
        }
    } else {
        *base = overrides;
    }
}

/// `$XDG_CONFIG_HOME/agt/config.toml`, falling back to `~/.config`.
pub fn path() -> Result<PathBuf> {
    local::agt_config_file("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(text: &str) -> (tempfile::TempDir, Result<Config>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        let result = Config::load_from(&path);
        (dir, result)
    }

    fn parse(text: &str) -> Config {
        load(text).1.unwrap()
    }

    /// The global and repository files as `load_paths` reads them.
    fn load_both(global: &str, local: &str) -> Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        let (global_path, local_path) = (
            dir.path().join("global.toml"),
            dir.path().join("local.toml"),
        );
        std::fs::write(&global_path, global).unwrap();
        std::fs::write(&local_path, local).unwrap();
        Config::load_paths(&global_path, &local_path)
    }

    fn remote(name: &str, host: &str, repo: &str) -> RemoteLocation {
        RemoteLocation {
            name: name.into(),
            host: host.into(),
            repo: repo.into(),
        }
    }

    fn fork() -> Vec<RemoteLocation> {
        vec![
            remote("origin", "github.com", "me/agt"),
            remote("upstream", "github.com", "org/agt"),
        ]
    }

    fn names(trackers: &[Tracker]) -> Vec<String> {
        trackers.iter().map(Tracker::name).collect()
    }

    #[test]
    fn a_missing_file_means_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load_from(&dir.path().join("agt").join("config.toml")).unwrap();
        assert_eq!(config, Config::default());
        assert!(config.tracker.is_empty());
    }

    #[test]
    fn sync_defaults_to_manual_pull_and_no_push() {
        let config = parse("");
        assert_eq!(config.sync.pull, None);
        assert_eq!(config.sync.push, None);
        assert_eq!(config.sync.max_age_secs, 300);
        assert_eq!(config.sync.remotes, RemoteSet::All);
        assert_eq!(config.pull_for(Kind::Issues), Mode::Manual);
        assert_eq!(config.push_for(Kind::PullRequests), None);
        let config = parse("[sync]\npull = \"auto\"\nmax_age_secs = 60\nremotes = \"origin\"\n");
        assert_eq!(config.pull_for(Kind::PullRequests), Mode::Auto);
        assert_eq!(config.sync.max_age_secs, 60);
        assert_eq!(config.sync.remotes, RemoteSet::Origin);
    }

    #[test]
    fn a_kind_section_overrides_the_shared_mode() {
        let config = parse(
            "[sync]\npull = \"manual\"\npush = \"off\"\n[issues]\npush = \"manual\"\n[pull_requests]\npull = \"auto\"\n",
        );
        assert_eq!(config.pull_for(Kind::Issues), Mode::Manual);
        assert_eq!(config.push_for(Kind::Issues), Some(Mode::Manual));
        assert_eq!(config.pull_for(Kind::PullRequests), Mode::Auto);
        assert_eq!(config.push_for(Kind::PullRequests), Some(Mode::Off));
    }

    #[test]
    fn sync_auto_is_an_unknown_key() {
        let (_, config) = load("[sync]\nauto = true\n");
        assert!(config.unwrap_err().0.contains("unknown field"));
    }

    #[test]
    fn repository_config_overrides_fields_and_replaces_arrays() {
        let config = load_both(
            "[sync]\npull = \"auto\"\nmax_age_secs = 17\n[worktree]\nroots = [\"a\", \"b\"]\n[[tracker]]\nhost = \"github.com\"\nrepo = \"o/r\"\n",
            "tracker = []\n[sync]\npull = \"manual\"\n[worktree]\nroots = [\"c\"]\n",
        )
        .unwrap();
        assert_eq!(config.sync.pull, Some(Mode::Manual));
        assert_eq!(config.sync.max_age_secs, 17);
        assert_eq!(config.worktree.roots, Some(vec!["c".into()]));
        assert!(config.tracker.is_empty());
    }

    #[test]
    fn the_repository_file_raises_push_but_never_lowers_it() {
        let both = |global: &str, local: &str| load_both(global, local).unwrap();
        let config = both("[sync]\npush = \"manual\"\n", "[sync]\npush = \"off\"\n");
        assert_eq!(config.sync.push, Some(Mode::Manual));
        let config = both("[sync]\npush = \"manual\"\n", "[sync]\npush = \"auto\"\n");
        assert_eq!(config.sync.push, Some(Mode::Auto));
        let config = both("[sync]\npush = \"off\"\n", "[sync]\npush = \"manual\"\n");
        assert_eq!(config.sync.push, Some(Mode::Manual));
        let config = both(
            "[issues]\npush = \"auto\"\n",
            "[issues]\npush = \"manual\"\n",
        );
        assert_eq!(config.issues.push, Some(Mode::Auto));
        let config = both("[sync]\npush = \"manual\"\n", "");
        assert_eq!(config.sync.push, Some(Mode::Manual));
    }

    #[test]
    fn a_repository_tracker_never_lowers_the_global_trackers_push() {
        let global = "[[tracker]]\nhost = \"github.com\"\nrepo = \"o/r\"\npush = \"auto\"\nissues = { push = \"manual\" }\n";
        let local = "[[tracker]]\nhost = \"GitHub.com\"\nrepo = \"o/R\"\npush = \"off\"\nissues = { push = \"off\" }\n";
        let config = load_both(global, local).unwrap();
        let entry = &config.tracker[0];
        assert_eq!(entry.push, Some(Mode::Auto));
        assert_eq!(
            entry.issues,
            KindSetting::Overrides(KindOverrides {
                push: Some(Mode::Manual),
                ..KindOverrides::default()
            })
        );
    }

    #[test]
    fn a_narrower_repository_key_never_lowers_the_effective_push() {
        let origin = [remote("origin", "github.com", "me/agt")];
        let push = |global: &str, local: &str, kind: Kind| {
            let config = load_both(global, local).unwrap();
            config.trackers(kind, &origin)[0].clone()
        };
        let sync = "[sync]\npush = \"manual\"\n";
        for local in [
            "[issues]\npush = \"off\"\n",
            "[[tracker]]\nhost = \"github.com\"\nrepo = \"me/agt\"\npush = \"off\"\n",
            "[[tracker]]\nhost = \"github.com\"\nrepo = \"me/agt\"\nissues = { push = \"off\" }\n",
        ] {
            let tracker = push(sync, local, Kind::Issues);
            assert_eq!(tracker.push, Some(Mode::Manual), "{local}");
        }
        // The other kind and a raise still work.
        let local = "[issues]\npush = \"auto\"\n[pull_requests]\npush = \"off\"\n";
        assert_eq!(push(sync, local, Kind::Issues).push, Some(Mode::Auto));
        assert_eq!(
            push(sync, local, Kind::PullRequests).push,
            Some(Mode::Manual)
        );
        // Without a global push the repository file sets its own.
        let tracker = push("", "[issues]\npush = \"off\"\n", Kind::Issues);
        assert_eq!(tracker.push, Some(Mode::Off));
    }

    #[test]
    fn a_repository_tracker_without_a_remote_here_is_ignored() {
        let config = load_both(
            "",
            "[[tracker]]\nhost = \"github.com\"\nrepo = \"stranger/else\"\npush = \"auto\"\n[[tracker]]\nhost = \"github.com\"\nrepo = \"me/agt\"\n",
        )
        .unwrap();
        assert!(config.repo_trackers);
        let trackers = config.trackers(Kind::Issues, &fork());
        assert_eq!(
            names(&trackers),
            ["github.com/me/agt", "github.com/org/agt"]
        );
        // The global file may name any repository.
        let config = load_both(
            "[[tracker]]\nhost = \"github.com\"\nrepo = \"stranger/else\"\n",
            "",
        )
        .unwrap();
        assert!(!config.repo_trackers);
        assert_eq!(
            names(&config.trackers(Kind::Issues, &fork())),
            [
                "github.com/me/agt",
                "github.com/org/agt",
                "github.com/stranger/else"
            ]
        );
    }

    #[test]
    fn pull_off_is_an_error_that_names_the_key() {
        for (text, key) in [
            ("[sync]\npull = \"off\"\n", "[sync] pull"),
            ("[issues]\npull = \"off\"\n", "[issues] pull"),
            ("[pull_requests]\npull = \"off\"\n", "[pull_requests] pull"),
            (
                "[[tracker]]\nhost = \"github.com\"\nrepo = \"o/r\"\npull = \"off\"\n",
                "tracker[0].pull",
            ),
            (
                "[[tracker]]\nhost = \"github.com\"\nrepo = \"o/r\"\npull_requests = { pull = \"off\" }\n",
                "tracker[0].pull_requests.pull",
            ),
        ] {
            let error = load(text).1.unwrap_err().0;
            assert!(error.contains(key), "{key}: {error}");
        }
        assert_eq!(
            parse("[sync]\npull = \"auto\"\n").sync.pull,
            Some(Mode::Auto)
        );
    }

    #[test]
    fn old_keys_map_to_the_new_ones() {
        let config = parse("[forge]\npost = false\n");
        assert_eq!(config.sync.push, Some(Mode::Off));
        let config = parse("[forge]\npost = true\n");
        assert_eq!(config.sync.push, Some(Mode::Manual));
        let config = parse("[forge]\npost = true\n[sync]\nauto_push = false\n");
        assert_eq!(config.sync.push, Some(Mode::Manual));
        let config = parse("[forge]\npost = true\n[sync]\nauto_push = true\n");
        assert_eq!(config.sync.push, Some(Mode::Auto));
        let config = parse("[sync]\nauto_push = true\n");
        assert_eq!(config.sync.push, Some(Mode::Off));
        let config = parse("[sync]\nauto_pull = true\n");
        assert_eq!(config.sync.pull, Some(Mode::Auto));
        let config = parse("[sync]\nauto_pull = false\n");
        assert_eq!(config.sync.pull, Some(Mode::Manual));
        assert_eq!(parse("").sync.push, None);
    }

    #[test]
    fn old_post_and_auto_push_combine_across_files() {
        let config = load_both("[forge]\npost = true\n", "[sync]\nauto_push = true\n").unwrap();
        assert_eq!(config.sync.push, Some(Mode::Auto));
        let config = load_both("[forge]\npost = true\n", "[forge]\npost = false\n").unwrap();
        assert_eq!(config.sync.push, Some(Mode::Manual));
        let config = load_both("", "[forge]\npost = true\n").unwrap();
        assert_eq!(config.sync.push, Some(Mode::Manual));
    }

    #[test]
    fn an_old_issue_tracker_becomes_an_issues_only_tracker() {
        let config = parse(
            "[[issues.tracker]]\nforge = \"gitlab\"\nhost = \"gitlab.example.com\"\n\
             repo = \"group/sub/project\"\nscope = \"assigned\"\nsince_days = 30\npush = false\n\
             [[issues.tracker]]\nforge = \"github\"\nhost = \"github.com\"\nrepo = \"o/r\"\npush = true\n",
        );
        assert_eq!(config.tracker.len(), 2);
        let issues = config.trackers(Kind::Issues, &[]);
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].forge, Forge::Gitlab);
        assert_eq!(issues[0].scope, Scope::Assigned);
        assert_eq!(issues[0].since_days, 30);
        assert_eq!(issues[0].push, Some(Mode::Off));
        assert_eq!(issues[1].scope, Scope::Involved);
        assert_eq!(issues[1].push, None);
        assert!(config.trackers(Kind::PullRequests, &[]).is_empty());
    }

    #[test]
    fn an_old_issue_tracker_keeps_the_remote_entry_for_pull_requests() {
        let config = parse(
            "[[issues.tracker]]\nforge = \"github\"\nhost = \"github.com\"\nrepo = \"me/agt\"\n\
             [[issues.tracker]]\nforge = \"github\"\nhost = \"github.com\"\nrepo = \"stranger/else\"\n",
        );
        let remotes = fork();
        assert_eq!(
            names(&config.trackers(Kind::PullRequests, &remotes)),
            ["github.com/me/agt", "github.com/org/agt"]
        );
        assert_eq!(
            names(&config.trackers(Kind::Issues, &remotes)),
            [
                "github.com/me/agt",
                "github.com/org/agt",
                "github.com/stranger/else"
            ]
        );
    }

    #[test]
    fn old_and_new_keys_for_one_field_in_one_file_are_an_error() {
        for (text, old, new) in [
            (
                "[sync]\nauto_pull = true\npull = \"auto\"\n",
                "auto_pull",
                "pull",
            ),
            (
                "[forge]\npost = true\n[sync]\npush = \"manual\"\n",
                "post",
                "push",
            ),
            (
                "[sync]\nauto_push = true\npush = \"auto\"\n",
                "auto_push",
                "push",
            ),
        ] {
            let (_dir, config) = load(text);
            let error = config.unwrap_err().0;
            assert!(error.contains(old) && error.contains(new), "{error}");
            assert!(error.contains("config.toml"), "{error}");
        }
    }

    #[test]
    fn remotes_choose_which_remotes_become_trackers() {
        let config = parse("");
        let trackers = config.trackers(Kind::PullRequests, &fork());
        assert_eq!(
            names(&trackers),
            ["github.com/me/agt", "github.com/org/agt"]
        );
        assert_eq!(trackers[1].remote.as_deref(), Some("upstream"));
        let config = parse("[sync]\nremotes = \"origin\"\n");
        assert_eq!(
            names(&config.trackers(Kind::Issues, &fork())),
            ["github.com/me/agt"]
        );
        let config = parse("[sync]\nremotes = \"none\"\n");
        assert!(config.trackers(Kind::Issues, &fork()).is_empty());
    }

    #[test]
    fn a_remote_on_an_unknown_host_is_not_a_tracker() {
        let remotes = [remote("origin", "git.example.invalid", "me/agt")];
        assert!(parse("").trackers(Kind::Issues, &remotes).is_empty());
    }

    #[test]
    fn a_configured_tracker_overrides_the_remote_at_the_same_place() {
        let config = parse(
            "[[tracker]]\nhost = \"GitHub.com\"\nrepo = \"ME/agt\"\npush = \"auto\"\nissues = { scope = \"all\" }\n",
        );
        let trackers = config.trackers(Kind::Issues, &fork());
        // The configured tracker replaces origin's entry; upstream stays a plain remote.
        assert_eq!(
            names(&trackers),
            ["github.com/me/agt", "github.com/org/agt"]
        );
        assert_eq!(trackers[1].remote.as_deref(), Some("upstream"));
        assert_eq!(trackers[1].push, None);
        assert_eq!(trackers[0].remote.as_deref(), Some("origin"));
        assert_eq!(trackers[0].push, Some(Mode::Auto));
        assert_eq!(trackers[0].scope, Scope::All);
        // The scope is for issues; pull requests keep the defaults.
        let requests = config.trackers(Kind::PullRequests, &fork());
        assert_eq!(requests[0].scope, Scope::Involved);
    }

    #[test]
    fn a_new_place_adds_a_tracker_and_a_remote_names_it() {
        let config = parse(
            "[[tracker]]\nhost = \"github.com\"\nrepo = \"org/agt\"\n[[tracker]]\nforge = \"gitlab\"\nhost = \"tickets.example.com\"\nrepo = \"team/tickets\"\n",
        );
        let trackers = config.trackers(Kind::Issues, &fork());
        assert_eq!(
            names(&trackers),
            [
                "github.com/me/agt",
                "github.com/org/agt",
                "tickets.example.com/team/tickets"
            ]
        );
        assert_eq!(trackers[1].remote.as_deref(), Some("upstream"));
        assert_eq!(trackers[2].remote, None);
    }

    #[test]
    fn disabling_drops_a_tracker_for_one_kind_or_both() {
        let config = parse(
            "[[tracker]]\nhost = \"github.com\"\nrepo = \"me/agt\"\npull_requests = false\n\
             [[tracker]]\nhost = \"github.com\"\nrepo = \"org/agt\"\nenabled = false\n",
        );
        let all = &mut fork();
        all.push(remote("fork", "github.com", "org/agt"));
        let config = Config {
            sync: SyncConfig {
                remotes: RemoteSet::All,
                ..SyncConfig::default()
            },
            ..config
        };
        assert_eq!(
            names(&config.trackers(Kind::Issues, all)),
            ["github.com/me/agt"]
        );
        assert!(config.trackers(Kind::PullRequests, all).is_empty());
    }

    #[test]
    fn modes_resolve_from_the_kind_table_to_the_default() {
        let config = parse(
            "[sync]\npull = \"manual\"\npush = \"off\"\n[issues]\npull = \"manual\"\n\
             [[tracker]]\nhost = \"github.com\"\nrepo = \"me/agt\"\npull = \"auto\"\npush = \"manual\"\n\
             pull_requests = { pull = \"manual\", push = \"auto\" }\n",
        );
        let issues = &config.trackers(Kind::Issues, &fork())[0];
        // The tracker beats the `[issues]` section, which beats `[sync]`.
        assert_eq!((issues.pull, issues.push), (Mode::Auto, Some(Mode::Manual)));
        let requests = &config.trackers(Kind::PullRequests, &fork())[0];
        assert_eq!(
            (requests.pull, requests.push),
            (Mode::Manual, Some(Mode::Auto))
        );
        let config = parse("[issues]\npull = \"auto\"\n");
        let bare = &config.trackers(Kind::PullRequests, &fork())[0];
        assert_eq!((bare.pull, bare.push), (Mode::Manual, None));
        assert_eq!(bare.push_mode(), Mode::Off);
        let section = &config.trackers(Kind::Issues, &fork())[0];
        assert_eq!(section.pull, Mode::Auto);
    }

    #[test]
    fn a_tracker_on_an_unknown_host_needs_its_forge() {
        let (_dir, config) = load("[[tracker]]\nhost = \"git.example.invalid\"\nrepo = \"o/r\"\n");
        let error = config.unwrap_err().0;
        assert!(error.contains("tracker[0].forge"), "{error}");
        let (_dir, config) = load(
            "[[tracker]]\nforge = \"gitlab\"\nhost = \"git.example.invalid\"\nrepo = \"o/r\"\n",
        );
        assert!(config.is_ok());
    }

    #[test]
    fn hooks_merge_per_key_and_reject_unknown_keys() {
        let global = "[worktree.hooks]\ncreate = [\"a\"]\nremove = [\"b\"]\n";
        let config = load_both(global, "[worktree.hooks]\ncreate = [\"c\", \"d\"]\n").unwrap();
        assert_eq!(config.worktree.hooks.create, ["c", "d"]);
        assert_eq!(config.worktree.hooks.remove, ["b"]);
        assert!(load_both(global, "[worktree.hooks]\nafter = []\n").is_err());
    }

    #[test]
    fn invalid_global_fields_name_the_global_file() {
        let error = load_both(
            "[[tracker]]\nhost = \"\"\nrepo = \"o/r\"\nforge = \"github\"\n",
            "",
        )
        .unwrap_err()
        .0;
        assert!(error.contains("global.toml"), "{error}");
        assert!(error.contains("host"), "{error}");
    }

    #[test]
    fn sync_refuses_unknown_keys_and_bad_values() {
        for text in [
            "[sync]\nbogus = 1\n",
            "[sync]\nauto = \"yes\"\n",
            "[sync]\nmax_age_secs = -1\n",
            "[sync]\npush = \"sometimes\"\n",
            "[sync]\nremotes = \"some\"\n",
        ] {
            let (_dir, config) = load(text);
            let error = config.unwrap_err().0;
            assert!(error.contains("config.toml"), "{error}");
        }
    }

    #[test]
    fn bad_choices_name_the_key_and_the_allowed_values() {
        for (text, expected) in [
            (
                "[sync]\npull = \"bad\"\n",
                "[sync] pull = \"bad\": expected manual or auto",
            ),
            (
                "[sync]\npush = \"bad\"\n",
                "[sync] push = \"bad\": expected off, manual or auto",
            ),
            (
                "[sync]\nremotes = \"bogus\"\n",
                "[sync] remotes = \"bogus\": expected origin, all or none",
            ),
            (
                "[issues]\npush = \"x\"\n",
                "[issues] push = \"x\": expected off, manual or auto",
            ),
            (
                "[[tracker]]\nhost = \"github.com\"\nrepo = \"o/r\"\npull = \"x\"\n",
                "[[tracker]] pull = \"x\": expected manual or auto",
            ),
            (
                "[[tracker]]\nhost = \"github.com\"\nrepo = \"o/r\"\nissues = { push = \"x\" }\n",
                "[[tracker]] issues push = \"x\"",
            ),
        ] {
            let error = load(text).1.unwrap_err().0;
            assert!(error.contains(expected), "{expected}: {error}");
            assert!(!error.contains("unknown variant"), "{error}");
        }
    }

    #[test]
    fn worktree_roots_are_optional() {
        assert_eq!(parse("").worktree.roots, None);
        assert_eq!(
            parse("[worktree]\nroots = [\"~/code/*\"]\n").worktree.roots,
            Some(vec!["~/code/*".to_owned()])
        );
    }

    #[test]
    fn a_tracker_takes_defaults_for_omitted_keys() {
        let config =
            parse("[[tracker]]\nforge = \"github\"\nhost = \"github.com\"\nrepo = \"o/r\"\n");
        let tracker = &config.tracker[0];
        assert_eq!(tracker.forge, Some(Forge::Github));
        assert!(tracker.enabled);
        assert_eq!(tracker.issues, KindSetting::Enabled(true));
        let resolved = &config.trackers(Kind::Issues, &[])[0];
        assert_eq!(resolved.scope, Scope::Involved);
        assert_eq!(resolved.since_days, 90);
        assert_eq!(resolved.push, None);
        assert_eq!(resolved.pull, Mode::Manual);
    }

    #[test]
    fn invalid_toml_names_the_file() {
        let (dir, config) = load("[[tracker\n");
        let error = config.unwrap_err().0;
        assert!(
            error.contains(&dir.path().join("config.toml").display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_key_is_refused_and_named() {
        for text in [
            "colour = \"red\"\n",
            "[issues]\nbogus = 1\n",
            "[[tracker]]\nforge = \"github\"\nhost = \"h\"\nrepo = \"o/r\"\nbogus = 1\n",
            "[[tracker]]\nforge = \"github\"\nhost = \"h\"\nrepo = \"o/r\"\nissues = { bogus = 1 }\n",
        ] {
            let (dir, config) = load(text);
            let error = config.unwrap_err().0;
            assert!(error.contains("config.toml"), "{error}");
            assert!(error.contains("unknown field"), "{error}");
            drop(dir);
        }
    }

    #[test]
    fn a_wrong_value_names_the_key() {
        let (_dir, config) = load("[[tracker]]\nforge = \"jira\"\nhost = \"h\"\nrepo = \"o/r\"\n");
        let error = config.unwrap_err().0;
        assert!(error.contains("forge") || error.contains("jira"), "{error}");
        let (_dir, config) = load("[[tracker]]\nforge = \"github\"\nhost = \"h\"\n");
        assert!(config.unwrap_err().0.contains("repo"));
        let (_dir, config) =
            load("[[tracker]]\nforge = \"github\"\nhost = \"h\"\nrepo = \"o/r\"\nissues = 3\n");
        assert!(config.unwrap_err().0.contains("table of overrides"));
    }

    #[test]
    fn semantic_errors_name_the_file_and_key() {
        for (body, key) in [
            ("host = \"\"\nrepo = \"o/r\"", "tracker[0].host"),
            ("host = \"h\"\nrepo = \"noslash\"", "tracker[0].repo"),
            ("host = \"h\"\nrepo = \"o/\"", "tracker[0].repo"),
            (
                "host = \"h\"\nrepo = \"o/r\"\nissues = { since_days = 0 }",
                "tracker[0].issues.since_days",
            ),
            (
                "host = \"h\"\nrepo = \"o/r\"\npull_requests = { scope = \"all\" }",
                "tracker[0].pull_requests",
            ),
        ] {
            let (dir, config) = load(&format!("[[tracker]]\nforge = \"github\"\n{body}\n"));
            let error = config.unwrap_err().0;
            assert!(error.contains(key), "{error}");
            assert!(error.contains(&dir.path().join("config.toml").display().to_string()));
        }
    }
}
