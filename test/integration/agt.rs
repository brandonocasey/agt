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
    env.bin(&env.repo).args(args).output().unwrap()
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
    let dir = env.state();
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{KEY}.json"));
    fs::copy(source.join(format!("{KEY}.json")), &file).unwrap();

    let listed = env.run(&["--target", KEY, "show", "h2"]);
    let text = listed.to_string();
    assert!(text.contains("fix this"), "{text}");

    env.run(&["--target", KEY, "reply", "h2", "--body", "agreed"]);
    let document: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
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

/// `path` MUST be relative to `src/`. Macro bodies are checked for absolute paths without expansion.
fn violations(path: &str, text: &str) -> Vec<String> {
    use syn::visit::Visit;

    let syntax = match syn::parse_file(text) {
        Ok(syntax) => syntax,
        Err(error) => return vec![format!("src/{path}:{}: {error}", error.span().start().line)],
    };
    let mut module: Vec<String> = path
        .trim_end_matches(".rs")
        .split('/')
        .map(str::to_owned)
        .collect();
    if module
        .last()
        .is_some_and(|name| ["mod", "lib", "main"].contains(&name.as_str()))
    {
        module.pop();
    }
    let mut scan = BoundaryScan {
        path,
        text,
        top: module.first().cloned().unwrap_or_default(),
        module,
        found: Vec::new(),
    };
    scan.visit_file(&syntax);
    scan.found
}

struct BoundaryScan<'a> {
    path: &'a str,
    text: &'a str,
    top: String,
    module: Vec<String>,
    found: Vec<String>,
}

impl BoundaryScan<'_> {
    fn record(&mut self, span: proc_macro2::Span, reason: &str) {
        let line = span.start().line;
        let prefix = format!("src/{}:{line}: ", self.path);
        if !self.found.iter().any(|found| found.starts_with(&prefix)) {
            let source = self.text.lines().nth(line.saturating_sub(1)).unwrap_or("");
            self.found
                .push(format!("{prefix}{reason}: {}", source.trim()));
        }
    }

    fn resolve(&self, names: &[String]) -> Option<Vec<String>> {
        let mut resolved = self.module.clone();
        let mut parts = names.iter();
        match parts.next()?.as_str() {
            "crate" => resolved.clear(),
            "self" => {}
            "super" => {
                resolved.pop()?;
            }
            _ => return None,
        }
        for name in parts {
            if name == "super" {
                resolved.pop()?;
            } else if name != "self" {
                resolved.push(name.clone());
            }
        }
        Some(resolved)
    }

    fn check(&mut self, names: &[String], span: proc_macro2::Span) {
        let Some(resolved) = self.resolve(names) else {
            return;
        };
        let Some(module) = resolved.first() else {
            return;
        };
        if DOMAINS.contains(&module.as_str())
            && (FOUNDATIONS.contains(&self.top.as_str())
                || (DOMAINS.contains(&self.top.as_str()) && &self.top != module))
        {
            self.record(span, &format!("`{}` must not import `{module}`", self.top));
        }
    }

    fn macro_tokens(&mut self, stream: proc_macro2::TokenStream) {
        use proc_macro2::{Delimiter, TokenTree};
        let tokens = stream.into_iter().collect::<Vec<_>>();
        for token in &tokens {
            if let TokenTree::Group(group) = token {
                self.macro_tokens(group.stream());
            }
        }
        for window in tokens.windows(4) {
            let [
                TokenTree::Ident(root),
                TokenTree::Punct(first),
                TokenTree::Punct(second),
                next,
            ] = window
            else {
                continue;
            };
            if identifier(root) != "crate" || first.as_char() != ':' || second.as_char() != ':' {
                continue;
            }
            match next {
                TokenTree::Ident(module) => {
                    self.check(&["crate".into(), identifier(module)], root.span())
                }
                TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => self.record(
                    root.span(),
                    "brace-imports from the crate root hide the module",
                ),
                _ => {}
            }
        }
    }

    fn use_tree(&mut self, tree: &syn::UseTree, names: &mut Vec<String>) {
        use syn::spanned::Spanned;
        match tree {
            syn::UseTree::Path(path) => {
                names.push(identifier(&path.ident));
                self.use_tree(&path.tree, names);
                names.pop();
            }
            syn::UseTree::Group(group) => {
                if names.first().is_some_and(|name| name == "crate") && names.len() == 1 {
                    self.record(
                        group.span(),
                        "brace-imports from the crate root hide the module",
                    );
                }
                for item in &group.items {
                    self.use_tree(item, names);
                }
            }
            syn::UseTree::Name(name) => {
                names.push(identifier(&name.ident));
                self.check(names, name.span());
                names.pop();
            }
            syn::UseTree::Rename(rename) => {
                names.push(identifier(&rename.ident));
                self.check(names, rename.span());
                names.pop();
            }
            syn::UseTree::Glob(glob) => self.check(names, glob.span()),
        }
    }
}

fn identifier(ident: &syn::Ident) -> String {
    ident.to_string().trim_start_matches("r#").to_owned()
}

impl<'ast> syn::visit::Visit<'ast> for BoundaryScan<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.module.push(identifier(&item.ident));
        syn::visit::visit_item_mod(self, item);
        self.module.pop();
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.use_tree(&item.tree, &mut Vec::new());
    }

    fn visit_meta_list(&mut self, meta: &'ast syn::MetaList) {
        self.macro_tokens(meta.tokens.clone());
        syn::visit::visit_meta_list(self, meta);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.macro_tokens(mac.tokens.clone());
        syn::visit::visit_macro(self, mac);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        use syn::spanned::Spanned;
        let names = path
            .segments
            .iter()
            .map(|part| identifier(&part.ident))
            .collect::<Vec<_>>();
        self.check(&names, path.span());
        syn::visit::visit_path(self, path);
    }
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

#[test]
fn the_boundary_scan_resolves_rust_paths() {
    for (path, code) in [
        ("store/mod.rs", "use crate ::\n review::Cli;"),
        ("forge/mod.rs", "use super::review::Cli;"),
        ("core/x/y.rs", "use super::super::super::review::Cli;"),
        ("core/x/mod.rs", "use super::super::review::Cli;"),
        (
            "core/mod.rs",
            "mod nested { use super::super::review::Cli; }",
        ),
        ("core/mod.rs", "use super::{review::Cli};"),
    ] {
        let found = violations(path, code);
        assert_eq!(found.len(), 1, "{path}: {code}: {found:?}");
    }
    for code in [
        "use super::local::Thing;",
        "mod nested { use super::super::local::Thing; }",
        "const TEXT: &str = r#\"crate::review\"#; /* crate::review */",
    ] {
        assert!(violations("core/x.rs", code).is_empty(), "{code}");
    }
}

#[test]
fn the_boundary_scan_handles_rust_syntax_without_text_false_positives() {
    for (path, code) in [
        ("store/mod.rs", "use crate::{core::Thing};"),
        ("core/mod.rs", "use {super::review::Thing as Renamed};"),
        (
            "core/mod.rs",
            "mod a { mod b { use super::{super::{super::review::Thing}}; } }",
        ),
        ("forge/github.rs", "type Alias = crate::r#review::Thing;"),
        ("review/mod.rs", "fn f(_: &crate::issue::Thing) {}"),
        ("core/mod.rs", "fn f() { crate ::\nreview :: action(); }"),
        ("core/mod.rs", "type Alias = Vec<crate::issue::Thing>;"),
        ("store/mod.rs", "use crate::review::*;"),
    ] {
        let found = violations(path, code);
        assert_eq!(found.len(), 1, "{path}: {code}: {found:?}");
    }
    for (path, code) in [
        (
            "core/mod.rs",
            "/* crate::review */ const S: &str = \"crate::issue\";",
        ),
        (
            "core/mod.rs",
            "const S: &str = r###\"crate::{review};\"###;",
        ),
        (
            "review/x.rs",
            "use super::local::Thing; type Alias = self::nested::Thing;",
        ),
        (
            "review/mod.rs",
            "mod x { mod y { use super::super::local::Thing; } }",
        ),
        (
            "cleanup/mod.rs",
            "use crate::review::prune; use super::issue::prune;",
        ),
        ("core/x.rs", "use super::{local::Thing, other::Type};"),
    ] {
        assert!(violations(path, code).is_empty(), "{path}: {code}");
    }
    let parse_error = violations("core/mod.rs", "use crate::review::{");
    assert_eq!(parse_error.len(), 1);
    assert!(parse_error[0].contains("src/core/mod.rs:"));
}

#[test]
fn the_boundary_scan_checks_absolute_paths_in_macro_tokens() {
    for code in [
        r#"fn f() { println!("{}", crate::review::value()); }"#,
        "macro_rules! bad { () => { crate::review::value() }; }",
        "#[anything(crate::review)] fn f() {}",
        "macro_rules! bad { () => { $crate::r#review::value() }; }",
        "macro_rules! bad { () => { use crate::{core}; }; }",
        "fn f() { outer!({ inner!([crate :: review :: value()]); }); }",
    ] {
        let found = violations("core/mod.rs", code);
        assert_eq!(found.len(), 1, "{code}: {found:?}");
    }
    for code in [
        r#"fn f() { println!("crate::review"); }"#,
        r##"macro_rules! safe { () => { r#"crate::{review}"# }; }"##,
        "macro_rules! safe { () => { crate::core::value() }; }",
    ] {
        assert!(violations("core/mod.rs", code).is_empty(), "{code}");
    }
}

#[test]
fn integration_commands_isolate_inherited_git_configuration() {
    let env = Env::new();
    if std::env::var_os("AGT_TEST_GIT_ENV_CHILD").is_some() {
        assert_eq!(env.git(&["config", "user.name"]), "Tester");
        assert_eq!(env.git(&["config", "user.email"]), "tester@example.com");
        assert!(
            env.git(&["var", "GIT_AUTHOR_IDENT"])
                .starts_with("Tester <tester@example.com>")
        );
        return;
    }
    let test = std::env::current_exe().unwrap();
    let output = env
        .command(test.to_str().unwrap(), &env.repo)
        .args([
            "--exact",
            "agt::integration_commands_isolate_inherited_git_configuration",
            "--nocapture",
        ])
        .env("AGT_TEST_GIT_ENV_CHILD", "1")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "user.name")
        .env("GIT_CONFIG_VALUE_0", "inherited name")
        .env(
            "GIT_CONFIG_PARAMETERS",
            "'user.email=inherited@example.com'",
        )
        .env("GIT_AUTHOR_NAME", "inherited author")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
