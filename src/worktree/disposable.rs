//! Which ignored paths a backup skips: what a build or install recreates.

const DIRS: [&str; 7] = [
    "node_modules",
    ".venv",
    "vendor",
    "target",
    "dist",
    "build",
    "coverage",
];

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
    if dirs.iter().any(|dir| DIRS.contains(dir)) {
        return true;
    }
    if dirs
        .windows(3)
        .any(|window| window == [".cache", "agents", "scratch"])
    {
        return true;
    }
    name == ".DS_Store"
        || name.ends_with(".log")
        || name.ends_with(".tsbuildinfo")
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
            "app.log",
            "x/tsconfig.tsbuildinfo",
            ".DS_Store",
            "src/.DS_Store",
            "a.swp",
            "notes.txt~",
            ".#lock",
            "#draft#",
            ".cache/agents/scratch/t/file",
            ".cache/agents/scratch/",
        ] {
            assert!(is_disposable(path), "{path}");
        }
    }

    #[test]
    fn secrets_notes_and_source_are_kept() {
        for path in [
            ".env",
            ".env.local",
            "config/credentials.json",
            "NOTES.md",
            "src/main.rs",
            "build",
            "dist.txt",
            ".cache/other/file",
            ".cache/agents/keep.txt",
            "secrets/",
        ] {
            assert!(!is_disposable(path), "{path}");
        }
    }
}
