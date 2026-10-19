//! The user's `agt` configuration: `$XDG_CONFIG_HOME/agt/config.toml`. A missing file means
//! defaults. The file holds no secrets; credentials come from the forge CLIs.
//!
//! Unknown keys are an error, so a misspelled key is reported instead of silently ignored.
//! Every error names the file and the key.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::error::{Error, Result, fail};

#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub issues: Issues,
    #[serde(default)]
    pub worktree: Worktree,
    #[serde(default)]
    pub sync: SyncConfig,
}

/// When a read pulls from the forge first.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncConfig {
    /// `false` keeps every read local.
    #[serde(default, alias = "auto")]
    pub auto_pull: bool,
    #[serde(default)]
    pub auto_push: bool,
    /// A read syncs when the last sync is older than this many seconds.
    #[serde(default = "default_max_age_secs")]
    pub max_age_secs: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            auto_pull: false,
            auto_push: false,
            max_age_secs: default_max_age_secs(),
        }
    }
}

fn default_max_age_secs() -> u64 {
    300
}

#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Worktree {
    /// Globs of repository directories `agt worktree list --all` scans; `~` is the home
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

#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Issues {
    #[serde(default)]
    pub tracker: Vec<Tracker>,
}

/// One repository whose issues are synced.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tracker {
    pub forge: TrackerForge,
    pub host: String,
    /// `owner/repo` (GitLab: the full project path).
    pub repo: String,
    #[serde(default)]
    pub scope: Scope,
    #[serde(default = "default_since_days")]
    pub since_days: u32,
    #[serde(default)]
    pub push: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackerForge {
    Github,
    Gitlab,
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

impl Config {
    /// The configuration from the default path, or defaults when the file is missing.
    pub fn load() -> Result<Self> {
        Self::load_from(&path()?)
    }

    pub fn load_for_repo(repo: &crate::git::Repo) -> Result<Self> {
        let global = path()?;
        let local = repo.main_checkout_root()?.join(".config/agt.toml");
        Self::load_paths(&global, &local)
    }

    fn load_paths(global: &Path, local: &Path) -> Result<Self> {
        let mut merged = read_value(global)?;
        let overrides = read_value(local)?;
        merge_values(&mut merged, overrides);
        let config: Self = merged.try_into().map_err(|error: toml::de::Error| {
            Error(format!("parse {}: {}", local.display(), error.message()))
        })?;
        config.validate(local)?;
        Ok(config)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return fail(format!("read {}: {error}", path.display())),
        };
        let config: Self = toml::from_str(&text)
            .map_err(|error| Error(format!("parse {}: {}", path.display(), error.message())))?;
        config.validate(path)?;
        Ok(config)
    }

    fn validate(&self, path: &Path) -> Result<()> {
        for (index, tracker) in self.issues.tracker.iter().enumerate() {
            let at = format!("issues.tracker[{index}]");
            let invalid =
                |key: &str, why: &str| fail(format!("{}: {at}.{key} {why}", path.display()));
            if tracker.host.trim().is_empty() {
                return invalid("host", "must not be empty");
            }
            let mut parts = tracker.repo.split('/');
            if tracker.repo.split('/').any(str::is_empty) || parts.nth(1).is_none() {
                return invalid("repo", "must look like owner/repo");
            }
            if tracker.since_days == 0 {
                return invalid("since_days", "must be at least 1");
            }
        }
        Ok(())
    }
}

fn read_value(path: &Path) -> Result<toml::Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return fail(format!("read {}: {error}", path.display())),
    };
    let mut value: toml::Value = toml::from_str(&text)
        .map_err(|error| Error(format!("parse {}: {error}", path.display())))?;
    let config: Config = value
        .clone()
        .try_into()
        .map_err(|error| Error(format!("parse {}: {error}", path.display())))?;
    config.validate(path)?;
    if let Some(sync) = value.get_mut("sync").and_then(toml::Value::as_table_mut)
        && let Some(auto) = sync.remove("auto")
    {
        sync.insert("auto_pull".into(), auto);
    }
    Ok(value)
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
    let base = match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        _ => {
            let home = std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .ok_or_else(|| Error("HOME is not set".into()))?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("agt").join("config.toml"))
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

    #[test]
    fn a_missing_file_means_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load_from(&dir.path().join("agt").join("config.toml")).unwrap();
        assert_eq!(config, Config::default());
        assert!(config.issues.tracker.is_empty());
    }

    #[test]
    fn sync_defaults_to_local_with_five_minutes() {
        let (_dir, config) = load("");
        let sync = config.unwrap().sync;
        assert!(!sync.auto_pull);
        assert!(!sync.auto_push);
        assert_eq!(sync.max_age_secs, 300);
        let (_dir, config) = load("[sync]\nauto = false\nmax_age_secs = 60\n");
        let sync = config.unwrap().sync;
        assert!(!sync.auto_pull);
        assert_eq!(sync.max_age_secs, 60);
    }

    #[test]
    fn sync_legacy_alias_is_explicit_and_duplicate_aliases_fail() {
        let (_, config) = load("[sync]\nauto = true\n");
        assert!(config.unwrap().sync.auto_pull);
        let (_, config) = load("[sync]\nauto = true\nauto_pull = false\n");
        assert!(config.unwrap_err().0.contains("duplicate field"));
    }

    #[test]
    fn repository_config_overrides_fields_and_replaces_arrays() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");
        std::fs::write(&global, "[sync]\nauto = true\nauto_push = true\nmax_age_secs = 17\n[worktree]\nroots = [\"a\", \"b\"]\n[[issues.tracker]]\nforge = \"github\"\nhost = \"github.com\"\nrepo = \"o/r\"\n").unwrap();
        std::fs::write(
            &local,
            "[sync]\nauto_pull = false\n[worktree]\nroots = [\"c\"]\n[issues]\ntracker = []\n",
        )
        .unwrap();
        let config = Config::load_paths(&global, &local).unwrap();
        assert!(!config.sync.auto_pull);
        assert!(config.sync.auto_push);
        assert_eq!(config.sync.max_age_secs, 17);
        assert_eq!(config.worktree.roots, Some(vec!["c".into()]));
        assert!(config.issues.tracker.is_empty());
        std::fs::write(&local, "[sync]\nauto = false\nauto_pull = true\n").unwrap();
        assert!(
            Config::load_paths(&global, &local)
                .unwrap_err()
                .0
                .contains("duplicate field")
        );
    }

    #[test]
    fn hooks_merge_per_key_and_reject_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");
        std::fs::write(
            &global,
            "[worktree.hooks]\ncreate = [\"a\"]\nremove = [\"b\"]\n",
        )
        .unwrap();
        std::fs::write(&local, "[worktree.hooks]\ncreate = [\"c\", \"d\"]\n").unwrap();
        let hooks = Config::load_paths(&global, &local).unwrap().worktree.hooks;
        assert_eq!(hooks.create, ["c", "d"]);
        assert_eq!(hooks.remove, ["b"]);
        std::fs::write(&local, "[worktree.hooks]\nafter = []\n").unwrap();
        assert!(Config::load_paths(&global, &local).is_err());
    }

    #[test]
    fn invalid_global_fields_name_the_global_file() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let local = dir.path().join("local.toml");
        std::fs::write(
            &global,
            "[[issues.tracker]]\nforge = \"github\"\nhost = \"\"\nrepo = \"o/r\"\n",
        )
        .unwrap();
        let error = Config::load_paths(&global, &local).unwrap_err().0;
        assert!(error.contains("global.toml"), "{error}");
        assert!(error.contains("host"), "{error}");
    }

    #[test]
    fn sync_refuses_unknown_keys_and_bad_values() {
        for text in [
            "[sync]\nbogus = 1\n",
            "[sync]\nauto = \"yes\"\n",
            "[sync]\nmax_age_secs = -1\n",
        ] {
            let (_dir, config) = load(text);
            let error = config.unwrap_err().0;
            assert!(error.contains("config.toml"), "{error}");
        }
    }

    #[test]
    fn worktree_roots_are_optional() {
        let (_dir, config) = load("");
        assert_eq!(config.unwrap().worktree.roots, None);
        let (_dir, config) = load("[worktree]\nroots = [\"~/code/*\"]\n");
        assert_eq!(
            config.unwrap().worktree.roots,
            Some(vec!["~/code/*".to_owned()])
        );
    }

    #[test]
    fn a_tracker_takes_defaults_for_omitted_keys() {
        let (_dir, config) =
            load("[[issues.tracker]]\nforge = \"github\"\nhost = \"github.com\"\nrepo = \"o/r\"\n");
        let tracker = &config.unwrap().issues.tracker[0];
        assert_eq!(tracker.forge, TrackerForge::Github);
        assert_eq!(tracker.scope, Scope::Involved);
        assert_eq!(tracker.since_days, 90);
        assert_eq!(tracker.push, None);
    }

    #[test]
    fn a_full_config_parses() {
        let (_dir, config) = load(
            "[[issues.tracker]]\nforge = \"gitlab\"\nhost = \"gitlab.example.com\"\n\
             repo = \"group/sub/project\"\nscope = \"assigned\"\nsince_days = 30\npush = true\n\
             [[issues.tracker]]\nforge = \"github\"\nhost = \"github.com\"\nrepo = \"o/r\"\n\
             scope = \"all\"\n",
        );
        let trackers = config.unwrap().issues.tracker;
        assert_eq!(trackers.len(), 2);
        assert_eq!(trackers[0].forge, TrackerForge::Gitlab);
        assert_eq!(trackers[0].scope, Scope::Assigned);
        assert_eq!(trackers[0].since_days, 30);
        assert_eq!(trackers[0].push, Some(true));
        assert_eq!(trackers[1].scope, Scope::All);
    }

    #[test]
    fn invalid_toml_names_the_file() {
        let (dir, config) = load("[[issues.tracker\n");
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
            "[[issues.tracker]]\nforge = \"github\"\nhost = \"h\"\nrepo = \"o/r\"\npull = true\n",
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
        let (_dir, config) =
            load("[[issues.tracker]]\nforge = \"jira\"\nhost = \"h\"\nrepo = \"o/r\"\n");
        let error = config.unwrap_err().0;
        assert!(error.contains("forge") || error.contains("jira"), "{error}");
        let (_dir, config) = load("[[issues.tracker]]\nforge = \"github\"\nhost = \"h\"\n");
        assert!(config.unwrap_err().0.contains("repo"));
    }

    #[test]
    fn semantic_errors_name_the_file_and_key() {
        for (body, key) in [
            ("host = \"\"\nrepo = \"o/r\"", "issues.tracker[0].host"),
            ("host = \"h\"\nrepo = \"noslash\"", "issues.tracker[0].repo"),
            ("host = \"h\"\nrepo = \"o/\"", "issues.tracker[0].repo"),
            (
                "host = \"h\"\nrepo = \"o/r\"\nsince_days = 0",
                "issues.tracker[0].since_days",
            ),
        ] {
            let (dir, config) = load(&format!("[[issues.tracker]]\nforge = \"github\"\n{body}\n"));
            let error = config.unwrap_err().0;
            assert!(error.contains(key), "{error}");
            assert!(error.contains(&dir.path().join("config.toml").display().to_string()));
        }
    }
}
