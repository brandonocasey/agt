//! Which ignored paths a backup skips: what a build or install recreates.

use crate::core::harness;

const DIRS: [&str; 20] = [
    "node_modules",
    ".venv",
    "vendor",
    "target",
    "dist",
    "build",
    "coverage",
    // Caches a tool regenerates on its next run.
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".next",
    ".gradle",
    ".turbo",
    ".parcel-cache",
    ".tox",
    ".hypothesis",
    ".svelte-kit",
    ".terraform",
    "htmlcov",
];

/// Whether `name`, a file name, is a file the operating system writes and recreates.
pub fn is_os_junk(name: &str) -> bool {
    matches!(name, ".DS_Store" | "Thumbs.db" | "desktop.ini")
}

/// Whether `path` (relative, `/`-separated; a trailing `/` marks a directory) is disposable.
/// Files such as `.env*`, credentials, notes and source are never disposable.
pub fn is_disposable(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let dirs = &parts[..parts.len() - 1];
    let name = if path.ends_with('/') {
        ""
    } else {
        parts[parts.len() - 1]
    };
    if dirs
        .iter()
        .any(|dir| DIRS.contains(dir) || dir.ends_with(".egg-info"))
    {
        return true;
    }
    let scratch: Vec<&str> = harness::SCRATCH_DIR.split('/').collect();
    if dirs
        .windows(scratch.len())
        .any(|window| window == scratch.as_slice())
    {
        return true;
    }
    is_os_junk(name)
        || name.ends_with(".log")
        || name.ends_with(".tsbuildinfo")
        || name == ".coverage"
        || name.starts_with(".coverage.")
        || name == ".eslintcache"
        || name.ends_with(".swp")
        || name.ends_with(".swo")
        || name.ends_with('~')
        || name.starts_with(".#")
        || (name.len() > 1 && name.starts_with('#') && name.ends_with('#'))
}

#[cfg(test)]
mod tests {
    use super::is_disposable;

    #[test]
    fn dependency_build_and_scratch_paths_are_disposable() {
        for path in [
            "node_modules/",
            "web/node_modules/a/b.js",
            ".venv/",
            "target/debug/agt",
            "dist/",
            "coverage/lcov.info",
            "pkg/__pycache__/",
            ".pytest_cache/v/cache",
            ".mypy_cache/",
            ".ruff_cache/",
            "web/.next/cache/x",
            ".gradle/",
            ".turbo/",
            ".parcel-cache/",
            ".tox/py/lib",
            "app.log",
            "x/tsconfig.tsbuildinfo",
            ".DS_Store",
            "src/.DS_Store",
            "Thumbs.db",
            "assets/desktop.ini",
            "a.swp",
            "notes.txt~",
            ".#lock",
            "#draft#",
            ".cache/agents/scratch/t/file",
            ".cache/agents/scratch/",
            ".hypothesis/",
            "web/.svelte-kit/output/x",
            "infra/.terraform/",
            "htmlcov/index.html",
            "src/pkg.egg-info/",
            ".coverage",
            ".coverage.host.1234",
            "web/.eslintcache",
        ] {
            assert!(is_disposable(path), "{path}");
        }
    }

    #[test]
    fn secrets_notes_and_source_are_kept() {
        for path in [
            ".env",
            ".env.local",
            ".next.env",
            "__pycache__",
            ".tox",
            "config/credentials.json",
            "NOTES.md",
            "src/main.rs",
            "build",
            "dist.txt",
            ".cache/other/file",
            ".cache/agents/keep.txt",
            "secrets/",
            ".coveragerc",
            "terraform.tfstate",
        ] {
            assert!(!is_disposable(path), "{path}");
        }
    }
}
