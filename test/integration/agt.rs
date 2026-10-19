//! The `agt` binary itself: its top-level contract, the 0.4.0 store, the module boundaries.

use std::fs;
use std::path::{Path, PathBuf};

use crate::support::Env;

const COMMANDS: [&str; 12] = [
    "run", "pass", "finding", "comment", "reply", "resolve", "approve", "list", "show", "status",
    "sync", "publish",
];

fn agt(args: &[&str]) -> std::process::Output {
    let env = Env::new();
    env.command(env!("CARGO_BIN_EXE_agt"), &env.repo)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn review_help_lists_the_v0_4_0_commands() {
    let output = agt(&["review", "--help"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let listed: Vec<&str> = text
        .lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| *name != "help")
        .collect();
    assert_eq!(listed, COMMANDS, "{text}");
}

#[test]
fn unknown_top_level_command_is_a_usage_error() {
    let output = agt(&["frobnicate"]);
    assert_eq!(output.status.code(), Some(2));
    // The bare 0.4.0 form is gone: the commands live under `review`.
    assert_eq!(agt(&["status"]).status.code(), Some(2));
}

#[test]
fn errors_carry_the_agt_prefix() {
    let env = Env::with_branch();
    let error = env.fail(&["show", "h99"], None);
    assert!(error.starts_with("agt: error: "), "{error}");
}

#[test]
fn the_agt_override_names_win_over_the_old_ones() {
    let env = Env::with_branch();
    env.git(&["remote", "add", "origin", "git@github.com:Owner/Repo.git"]);
    let output = env
        .agt(&env.repo)
        .args(["sync", "--pr", "1"])
        .env("AGT_GH_BIN", crate::support::FAKE_FORGE)
        .env("AGT_GH_BIN", "/nonexistent/gh")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("run gh: No such file"), "{error}");
    assert!(
        env.forge_calls().is_empty(),
        "the AGT override must win over the old name"
    );
}

#[test]
fn a_0_4_0_store_reads_and_updates_unchanged() {
    const KEY: &str =
        "branch-feature-6e69e245c346ba398dad5c99913ffd9111403ece-1767225600-6e69e245c346";
    // The commits are pinned by date, so their ids match the ones the fixture was written for.
    let env = Env::new();
    let dated = [
        ("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z"),
        ("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z"),
    ];
    env.git(&["remote", "add", "origin", "git@github.com:Owner/Repo.git"]);
    env.write("a.txt", "one\ntwo\nthree\n");
    env.git(&["add", "-A"]);
    env.git_env(&["commit", "-q", "-m", "base"], &dated);
    env.git_env(&["switch", "-q", "-c", "feature"], &dated);
    env.write("a.txt", "one\nTWO\nthree\nfour\n");
    env.git_env(&["commit", "-q", "-am", "change"], &dated);

    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test/fixtures/store-0.4.0/github.com/Owner/Repo");
    let dir = env
        .path("state")
        .join("agents/reviews/github.com/Owner/Repo");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{KEY}.json"));
    fs::copy(source.join(format!("{KEY}.json")), &file).unwrap();

    let listed = env.run(&["--target", KEY, "show", "h2"]);
    let text = listed.to_string();
    assert!(text.contains("fix this"), "{text}");

    env.run(&["--target", KEY, "reply", "h2", "--body", "agreed"]);
    let original: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert!(!original.to_string().contains("agreed"));
    let migrated = env.state().join(format!("{KEY}.json"));
    let document: serde_json::Value = serde_json::from_slice(&fs::read(migrated).unwrap()).unwrap();
    assert_eq!(document["version"], 1);
    assert_eq!(document["revision"], 5);
    assert!(document.to_string().contains("agreed"));
}

#[test]
fn the_pre_0_5_override_names_still_work() {
    let env = Env::with_branch();
    env.git(&["remote", "add", "origin", "git@github.com:Owner/Repo.git"]);
    let output = env
        .agt(&env.repo)
        .args(["sync", "--pr", "1"])
        .env_remove("AGT_GH_BIN")
        .env("AGT_GH_BIN", "/nonexistent/gh")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("run gh: No such file"), "{error}");
}

/// Domain modules, which the foundation modules must not import.
const DOMAINS: [&str; 10] = [
    "review", "issue", "ci", "task", "sessions", "worktree", "secrets", "pr", "branch", "doctor",
];
const FOUNDATIONS: [&str; 4] = ["core", "store", "git", "forge"];

/// `path` is relative to `src/`. Returns one message per offending line.
fn violations(path: &str, text: &str) -> Vec<String> {
    let top = path.split(['/', '.']).next().unwrap_or("");
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let code = line.split("//").next().unwrap_or("");
        let mut rest = code;
        while let Some(at) = rest.find("crate::") {
            let after = &rest[at + "crate::".len()..];
            let module: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let bad = if after.starts_with('{') {
                Some("brace-imports from the crate root hide the module".to_owned())
            } else if DOMAINS.contains(&module.as_str())
                && (FOUNDATIONS.contains(&top) || (DOMAINS.contains(&top) && top != module))
            {
                Some(format!("`{top}` must not import `{module}`"))
            } else {
                None
            };
            if let Some(reason) = bad {
                found.push(format!(
                    "src/{path}:{}: {reason}: {}",
                    index + 1,
                    line.trim()
                ));
            }
            rest = after;
        }
    }
    found
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn modules_respect_the_import_direction() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&root, &mut files);
    assert!(
        files.len() > 10,
        "found no sources under {}",
        root.display()
    );
    let found: Vec<String> = files
        .iter()
        .flat_map(|file| {
            let relative = file
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            violations(&relative, &fs::read_to_string(file).unwrap())
        })
        .collect();
    assert!(found.is_empty(), "{}", found.join("\n"));
}

#[test]
fn the_boundary_scan_flags_bad_imports() {
    let bad = violations("forge/github.rs", "use crate::review::cli::Cli;\n");
    assert_eq!(bad.len(), 1, "{bad:?}");
    assert!(bad[0].contains("src/forge/github.rs:1"), "{bad:?}");
    assert_eq!(
        violations("review/commands.rs", "use crate::task::Thing;").len(),
        1
    );
    assert_eq!(
        violations("store/mod.rs", "use crate::{review, core};").len(),
        1
    );
    assert!(
        violations(
            "review/commands.rs",
            "use crate::forge::Fetched; // crate::issue"
        )
        .is_empty()
    );
    assert!(violations("cli.rs", "use crate::review;").is_empty());
}
