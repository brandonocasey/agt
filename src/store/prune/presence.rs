//! Whether the repository behind a `local/` directory still exists.

use std::fs;
use std::path::Path;

use super::super::Store;
use super::super::sidecars::REPO_FILE;
use crate::core::clock;

/// How long a recorded repository must have gone unseen, with its path missing, to be gone.
pub(super) const GONE_AFTER_SECS: i64 = 30 * 24 * 60 * 60;

/// Directories under which removable or network volumes appear. A path missing below one may
/// only be unmounted, so it never marks a repository gone.
const MOUNT_ROOTS: [&str; 4] = ["/Volumes", "/media", "/run/media", "/mnt"];

/// What `.repo.json` says about the repository behind a `local/` directory.
#[derive(Debug, PartialEq, Eq)]
pub enum Presence {
    NoRecord,
    Unreadable(String),
    /// The recorded path exists.
    Alive(String),
    /// The path is missing but agt wrote there recently: the repository was probably moved.
    Missing {
        common_dir: String,
        unseen_secs: i64,
    },
    /// The path is missing below a mount root: the volume may only be unmounted.
    Unmounted(String),
    /// The path is missing and agt has not run there for [`GONE_AFTER_SECS`].
    Gone {
        common_dir: String,
        unseen_secs: i64,
    },
}

impl Presence {
    /// Whether the repository's targets may go at any age.
    pub fn is_gone(&self) -> bool {
        matches!(self, Self::Gone { .. })
    }

    /// Why the repository is, or is not, treated as gone, naming the rule that decided.
    pub fn describe(&self) -> String {
        let days = |secs: &i64| (secs / clock::SECS_PER_DAY).max(0);
        match self {
            Self::NoRecord => "no recorded path, so its existence cannot be checked".into(),
            Self::Unreadable(why) => format!("its path record is unreadable ({why})"),
            Self::Alive(path) => format!("its repository still exists at {path}"),
            Self::Missing {
                common_dir,
                unseen_secs,
            } => format!(
                "{common_dir} is missing but agt last wrote there {} day(s) ago, under the {} days that mark a repository gone, so it may have moved",
                days(unseen_secs),
                GONE_AFTER_SECS / clock::SECS_PER_DAY
            ),
            Self::Unmounted(path) => {
                format!("{path} is missing below a mount root, so its volume may only be unmounted")
            }
            Self::Gone {
                common_dir,
                unseen_secs,
            } => format!(
                "local repository is gone: {common_dir} no longer exists and agt last wrote there {} day(s) ago (path missing and unseen for {}+ days)",
                days(unseen_secs),
                GONE_AFTER_SECS / clock::SECS_PER_DAY
            ),
        }
    }
}

impl Store {
    /// Whether the repository recorded in `dir` still exists, as of `now`.
    pub fn presence(&self, dir: &Path, now: i64) -> Presence {
        let repo = match self.recorded_repo(dir) {
            Ok(Some(repo)) => repo,
            Ok(None) => return Presence::NoRecord,
            Err(error) => return Presence::Unreadable(error.to_string()),
        };
        let common = Path::new(&repo.common_dir);
        if !common.is_absolute() {
            return Presence::Unreadable(format!("{} is not an absolute path", repo.common_dir));
        }
        match common.try_exists() {
            Ok(true) => return Presence::Alive(repo.common_dir),
            Ok(false) => {}
            Err(error) => return Presence::Unreadable(format!("{}: {error}", repo.common_dir)),
        }
        if MOUNT_ROOTS.iter().any(|root| {
            common
                .strip_prefix(root)
                .is_ok_and(|rest| rest.components().next().is_some())
        }) {
            return Presence::Unmounted(repo.common_dir);
        }
        // A record from before `last_seen` existed was last written when it was made.
        let seen = match repo.last_seen.as_deref() {
            Some(seen) => clock::parse(seen).ok(),
            None => seen_by_mtime(&dir.join(REPO_FILE)),
        };
        let Some(seen) = seen else {
            return Presence::Unreadable("no usable last-seen time".into());
        };
        let unseen_secs = now - seen;
        if unseen_secs >= GONE_AFTER_SECS {
            Presence::Gone {
                common_dir: repo.common_dir,
                unseen_secs,
            }
        } else {
            Presence::Missing {
                common_dir: repo.common_dir,
                unseen_secs,
            }
        }
    }
}

pub(super) fn seen_by_mtime(path: &Path) -> Option<i64> {
    clock::mtime_secs(&fs::metadata(path).ok()?)
}
