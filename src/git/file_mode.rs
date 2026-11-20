//! The mode of a diff entry, read without the panic `git2::DiffFile::mode` has.

use git2::DiffFile;
use libgit2_sys::git_diff_file;

/// The file-type bits of a mode, and the value they have for a regular file.
pub(super) const FILE_TYPE: u32 = 0o170_000;
pub(super) const REGULAR_FILE: u32 = 0o100_000;

const SYMLINK: u32 = 0o120_000;
const GITLINK: u32 = 0o160_000;
const OWNER_EXECUTE: u32 = 0o100;

// `file_mode` reads the one pointer `DiffFile` holds.
const _: () = assert!(size_of::<DiffFile<'static>>() == size_of::<*const git_diff_file>());

/// The mode of `file` as git reads it: a regular file is 100644 or 100755 whatever permission
/// bits its tree entry holds, so a legacy entry such as 100775 is an executable file, and a
/// symlink or gitlink carries no permission bits.
///
/// `DiffFile::mode` panics on a mode outside its enum, and libgit2 passes a tree entry's raw
/// mode through. A release build aborts on a panic, so the call cannot be guarded; this reads
/// the field itself.
pub(super) fn file_mode(file: &DiffFile<'_>) -> u32 {
    // SAFETY: the assertion above makes `file` its pointer, which libgit2 keeps valid for as long
    // as `file` lives.
    let raw = unsafe { (**(std::ptr::from_ref(file).cast::<*const git_diff_file>())).mode };
    let mode = u32::from(raw);
    match mode & FILE_TYPE {
        REGULAR_FILE if mode & OWNER_EXECUTE == 0 => REGULAR_FILE | 0o644,
        REGULAR_FILE => REGULAR_FILE | 0o755,
        SYMLINK | GITLINK => mode & FILE_TYPE,
        _ => mode,
    }
}
