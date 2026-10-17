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
    /// Writes stay local until this is true.
    #[serde(default)]
    pub push: bool,
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
    fn a_tracker_takes_defaults_for_omitted_keys() {
        let (_dir, config) =
            load("[[issues.tracker]]\nforge = \"github\"\nhost = \"github.com\"\nrepo = \"o/r\"\n");
        let tracker = &config.unwrap().issues.tracker[0];
        assert_eq!(tracker.forge, TrackerForge::Github);
        assert_eq!(tracker.scope, Scope::Involved);
        assert_eq!(tracker.since_days, 90);
        assert!(!tracker.push);
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
        assert!(trackers[0].push);
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
