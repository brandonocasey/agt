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

/// Why `path` cannot be read as git would stage it: its `filter` attribute names a driver the
/// config marks required. A symlink is staged as its target, which no filter sees.
pub(super) fn refusal_for_staging(repo: &git2::Repository, path: &Path) -> Option<String> {
    let on_disk = repo.workdir()?.join(path);
    let is_file = fs::symlink_metadata(on_disk).is_ok_and(|meta| meta.is_file());
    is_file.then(|| required_filter(repo, path, Use::Stage))?
}

/// Why the entry `path` of mode `mode` cannot be written as git would check it out. Git runs a
/// filter on a regular file only, never on a symlink or a gitlink.
pub(super) fn refusal_for_writing(
    repo: &git2::Repository,
    path: &Path,
    mode: u32,
) -> Option<String> {
    let regular = mode & FILE_TYPE == REGULAR_FILE;
    regular.then(|| required_filter(repo, path, Use::CheckOut))?
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
    match entries
        .into_iter()
        .find_map(|(path, mode)| refusal_for_writing(repo, path, mode))
    {
        Some(message) => Err(Error(message)),
        None => Ok(()),
    }
}

/// Whether a checkout may write `path`, whose new entry is `target`. When a required filter
/// applies, `refusal` takes the reason and the checkout MUST be aborted: this is for a notify
/// callback that returns the result.
pub(super) fn may_write(
    repo: &git2::Repository,
    refusal: &Refusal,
    path: Option<&Path>,
    target: Option<&DiffFile<'_>>,
) -> bool {
    let Some(path) = path else {
        return true;
    };
    let mode = target.map_or(REGULAR_FILE, |target| i32::from(target.mode()) as u32);
    let Some(message) = refusal_for_writing(repo, path, mode) else {
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
    options
        .notify_on(CheckoutNotificationType::UPDATED)
        .notify(move |_, path, _, target, _| may_write(repo, refusal, path, target.as_ref()));
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
