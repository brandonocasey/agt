//! Refusing to read or write a path that a required filter applies to. libgit2 runs no filter
//! drivers (git-lfs, git-crypt), so staging such a path would hash its content unfiltered and a
//! checkout would write the stored blob where the driver's output belongs.

use std::cell::RefCell;
use std::fs;
use std::path::Path;

use git2::build::CheckoutBuilder;
use git2::{AttrCheckFlags, CheckoutNotificationType, DiffFile};

use crate::core::error::{Error, Result};

/// `GIT_ATTR_CHECK_INCLUDE_HEAD`, which `git2` has no name for: also read the attributes `HEAD`
/// holds, so a work tree that has no `.gitattributes` yet still sees them.
const INCLUDE_HEAD: AttrCheckFlags = AttrCheckFlags::from_bits_retain(1 << 3);

/// What a refused path was going to be used for.
#[derive(Clone, Copy)]
enum Use {
    Stage,
    CheckOut,
}

/// The reason a checkout stopped at a path a required filter applies to.
pub(super) type Refusal = RefCell<Option<String>>;

/// The file-type bits of a mode, and the value they have for a regular file.
const FILE_TYPE: u32 = 0o170_000;
const REGULAR_FILE: u32 = 0o100_000;

/// The question "may this path be staged or written?" for one operation on `repo`. When no
/// filter is required it answers yes at once: it reads the config once, where an attribute
/// lookup for each path would cost more than the operation itself on a large tree.
pub(super) struct Guard<'r> {
    repo: &'r git2::Repository,
    any_required: bool,
}

impl<'r> Guard<'r> {
    pub(super) fn new(repo: &'r git2::Repository) -> Self {
        Self {
            repo,
            any_required: any_required(repo),
        }
    }

    /// Why `path` cannot be read as git would stage it: its `filter` attribute names a driver the
    /// config marks required. A symlink is staged as its target, which no filter sees.
    pub(super) fn refusal_for_staging(&self, path: &Path) -> Option<String> {
        if !self.any_required {
            return None;
        }
        let on_disk = self.repo.workdir()?.join(path);
        let is_file = fs::symlink_metadata(on_disk).is_ok_and(|meta| meta.is_file());
        is_file.then(|| required_filter(self.repo, path, Use::Stage))?
    }

    /// Why the entry `path` of mode `mode` cannot be written as git would check it out. Git runs a
    /// filter on a regular file only, never on a symlink or a gitlink.
    pub(super) fn refusal_for_writing(&self, path: &Path, mode: u32) -> Option<String> {
        if !self.any_required {
            return None;
        }
        let regular = mode & FILE_TYPE == REGULAR_FILE;
        regular.then(|| required_filter(self.repo, path, Use::CheckOut))?
    }
}

/// Whether any `filter.<name>.required` is true in the config of `repo`.
fn any_required(repo: &git2::Repository) -> bool {
    let Ok(config) = repo.config() else {
        return false;
    };
    let Ok(mut entries) = config.entries(Some(r"^filter\..*\.required$")) else {
        return false;
    };
    while let Some(Ok(entry)) = entries.next() {
        if entry
            .name()
            .is_some_and(|name| config.get_bool(name) == Ok(true))
        {
            return true;
        }
    }
    false
}

/// Why `path` cannot be used as `purpose` says: its `filter` attribute names a driver the config
/// marks required.
fn required_filter(repo: &git2::Repository, path: &Path, purpose: Use) -> Option<String> {
    let (flags, outcome) = match purpose {
        Use::Stage => (
            AttrCheckFlags::FILE_THEN_INDEX,
            "read as git would stage it",
        ),
        Use::CheckOut => (
            AttrCheckFlags::FILE_THEN_INDEX | INCLUDE_HEAD,
            "written as git would check it out",
        ),
    };
    let driver = repo.get_attr(path, "filter", flags).ok()??;
    let required = repo
        .config()
        .ok()?
        .get_bool(&format!("filter.{driver}.required"))
        .unwrap_or(false);
    required.then(|| {
        format!(
            "{}: the required filter '{driver}' cannot be run, so the file cannot be {outcome}",
            path.display()
        )
    })
}

/// Fails, naming the first of the `entries` (a path and its mode) a required filter applies to.
/// For a write that does not go through libgit2's checkout, or one that must be refused before
/// anything else changes.
pub(super) fn ensure_writable<'a>(
    repo: &git2::Repository,
    entries: impl IntoIterator<Item = (&'a Path, u32)>,
) -> Result<()> {
    let guard = Guard::new(repo);
    match entries
        .into_iter()
        .find_map(|(path, mode)| guard.refusal_for_writing(path, mode))
    {
        Some(message) => Err(Error(message)),
        None => Ok(()),
    }
}

/// Whether a checkout may write `path`, whose new entry is `target`. When a required filter
/// applies, `refusal` takes the reason and the checkout MUST be aborted: this is for a notify
/// callback that returns the result.
pub(super) fn may_write(
    guard: &Guard<'_>,
    refusal: &Refusal,
    path: Option<&Path>,
    target: Option<&DiffFile<'_>>,
) -> bool {
    let Some(path) = path else {
        return true;
    };
    let mode = target.map_or(REGULAR_FILE, |target| i32::from(target.mode()) as u32);
    let Some(message) = guard.refusal_for_writing(path, mode) else {
        return true;
    };
    refusal.replace(Some(message));
    false
}

/// Makes `options` abort a checkout, before it changes the work tree or the index, at the first
/// path it would write that a required filter applies to. `finished` then reports why.
pub(super) fn refuse_required_filters<'cb>(
    options: &mut CheckoutBuilder<'cb>,
    repo: &'cb git2::Repository,
    refusal: &'cb Refusal,
) {
    let guard = Guard::new(repo);
    if !guard.any_required {
        return;
    }
    options
        .notify_on(CheckoutNotificationType::UPDATED)
        .notify(move |_, path, _, target, _| may_write(&guard, refusal, path, target.as_ref()));
}

/// The result of a checkout built with [`refuse_required_filters`]: the refusal when it made the
/// checkout stop, else the checkout's own result.
pub(super) fn finished<T>(
    refusal: &Refusal,
    checked: std::result::Result<T, git2::Error>,
) -> Result<T> {
    match refusal.take() {
        Some(message) => Err(Error(message)),
        None => Ok(checked?),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;

    use super::any_required;
    use crate::core::testing::Sandbox;

    #[test]
    fn a_required_filter_is_found_however_the_config_spells_it() {
        let filter = |name: &str, lines: &str| format!("[filter \"{name}\"]\n{lines}");
        for (config, expected) in [
            (String::new(), false),
            (filter("lfs", "\tclean = cat\n"), false),
            (filter("lfs", "\trequired = false\n"), false),
            (filter("lfs", "\trequired = true\n"), true),
            (filter("lfs", "\trequired\n"), true),
            (filter("LFS", "\trequired = yes\n"), true),
            (
                filter("a", "\trequired = true\n") + &filter("a", "\trequired = false\n"),
                false,
            ),
        ] {
            let sandbox = Sandbox::with_repo();
            let dir = sandbox.repo();
            let mut file = OpenOptions::new()
                .append(true)
                .open(dir.join(".git/config"))
                .unwrap();
            file.write_all(config.as_bytes()).unwrap();

            let repo = git2::Repository::open(&dir).unwrap();

            assert_eq!(any_required(&repo), expected, "{config}");
        }
    }
}
