//! `agt sessions` on fixture transcripts. `test/fixtures/sessions/generate.py` writes the
//! fixtures; groups that span files have equal examples and distinct counts, so the expected
//! output holds in any directory order.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::support::Env;

const SINCE: &str = "2026-02-15";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test/fixtures/sessions")
}

/// A report name and its flags as `agt sessions report` arguments: the name picks the section.
fn report_args<'a>(args: &[&'a str]) -> Vec<&'a str> {
    match args.split_first() {
        Some((name, rest))
            if [
                "cost",
                "tools",
                "cache-gaps",
                "errors",
                "denials",
                "friction",
            ]
            .contains(name) =>
        {
            ["report", "--sections", name]
                .into_iter()
                .chain(rest.iter().copied())
                .collect()
        }
        _ => args.to_vec(),
    }
}

/// `agt sessions` in the isolated environment, reading fixtures by relative path.
fn bare(env: &Env, args: &[&str]) -> Command {
    let mut command = env.bin(&fixtures());
    command
        .env("AGT_PRICES", fixtures().join("prices.json"))
        .env("TZ", "UTC")
        .arg("sessions")
        .args(report_args(args));
    command
}

fn sessions(env: &Env, args: &[&str]) -> Command {
    let mut command = bare(env, args);
    command.args(["--since", SINCE]);
    command
}

/// Exit code, stdout and stderr.
fn run(mut command: Command) -> (Option<i32>, String, String) {
    let output = command.output().expect("agt runs");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Stdout of a run that must succeed.
fn stdout(command: Command) -> String {
    let (code, out, err) = run(command);
    assert_eq!(code, Some(0), "{err}");
    out
}

fn expected(name: &str) -> String {
    std::fs::read_to_string(fixtures().join("expected").join(format!("{name}.txt")))
        .expect("expected output exists")
}

#[test]
fn claude_reports_print_the_expected_tables() {
    let env = Env::new();
    for (args, name) in [
        ("cost", "cost"),
        ("cost --json", "cost_--json"),
        (
            "cost --by agent,model,project,session",
            "cost_--by_agent_model_project_session",
        ),
        (
            "cost --scratch --project repo",
            "cost_--scratch_--project_repo",
        ),
        ("cost --limit 2", "cost_--limit_2"),
        ("cost --by agent", "cost_--by_agent"),
        ("tools", "tools"),
        ("cache-gaps", "cache-gaps"),
        ("cache-gaps --gap 0.5", "cache-gaps_--gap_0_5"),
        ("errors", "errors"),
        ("denials", "denials"),
        ("friction", "friction"),
        ("friction --tsv --width 20", "friction_--tsv_--width_20"),
    ] {
        let mut args: Vec<&str> = args.split(' ').collect();
        args.extend(["--root", "projects"]);
        let mut actual = stdout(sessions(&env, &args));
        let mut wanted = expected(name);
        if name.contains("tsv") {
            // Rows follow the directory listing order, which differs between file systems.
            let sorted = |text: &str| {
                let mut lines: Vec<&str> = text.lines().collect();
                lines.sort_unstable();
                lines.join("\n")
            };
            (actual, wanted) = (sorted(&actual), sorted(&wanted));
        }
        assert_eq!(actual, wanted, "{name}");
    }
}

#[test]
fn scratch_projects_are_skipped_and_counted() {
    let env = Env::new();
    let (code, _, err) = run(sessions(&env, &["cost", "--root", "projects"]));
    assert_eq!(code, Some(0));
    assert_eq!(
        err,
        "(skipped 1 scratch transcripts; --scratch includes them)\n"
    );
    let root = env.path("roots");
    std::fs::create_dir_all(root.join("-tmp-probe")).unwrap();
    std::fs::write(root.join("-tmp-probe/run.jsonl"), "{}\n").unwrap();
    let mut named = bare(&env, &["cost", "--since", SINCE, "--root"]);
    named.arg(&root);
    let (code, _, err) = run(named);
    assert_eq!(code, Some(0));
    assert_eq!(
        err,
        "(skipped 1 scratch transcripts; --scratch includes them)\n"
    );
    // A transcript older than the start date is not read, so it is not counted.
    let mut late = bare(&env, &["cost", "--since", "2099-01-01", "--root"]);
    late.arg(&root);
    let (code, _, err) = run(late);
    assert_eq!(code, Some(0));
    assert_eq!(err, "");
    // Outside `--project`, it is not counted at all.
    let mut other = bare(
        &env,
        &["cost", "--since", SINCE, "--project", "repo", "--root"],
    );
    other.arg(&root);
    let (code, _, err) = run(other);
    assert_eq!(code, Some(0));
    assert_eq!(err, "");
}

#[test]
fn default_roots_follow_linked_claude_profiles() {
    let env = Env::new();
    let home = env.path("home");
    let profile = env.path("elsewhere");
    std::fs::create_dir_all(&profile).unwrap();
    let projects = fixtures().join("projects");
    std::os::unix::fs::symlink(&projects, profile.join("projects")).unwrap();
    std::os::unix::fs::symlink(&profile, home.join(".claudework")).unwrap();
    let out = stdout(sessions(&env, &["cost", "--harness", "claude"]));
    assert_eq!(out, expected("cost"));
}

#[test]
fn default_roots_include_the_claude_config_dir() {
    let env = Env::new();
    let profile = env.path("relocated");
    std::fs::create_dir_all(&profile).unwrap();
    std::os::unix::fs::symlink(fixtures().join("projects"), profile.join("projects")).unwrap();
    let mut command = sessions(&env, &["cost", "--harness", "claude"]);
    command.env("CLAUDE_CONFIG_DIR", &profile);
    assert_eq!(stdout(command), expected("cost"));
}

#[test]
fn codex_rollouts_read_as_calls_tools_and_messages() {
    let env = Env::new();
    assert_eq!(
        stdout(sessions(
            &env,
            &["cost", "--by", "harness,agent,model", "--root", "codex"]
        )),
        concat!(
            "Total $0.01 since 2026-02-15\n",
            "harness  agent     model            calls    in  write  read  out   USD     %  write%  read%  out%\n",
            "-------  --------  ---------------  -----  ----  -----  ----  ---  ----  ----  ------  -----  ----\n",
            "codex    main      gpt-test-1           2  1.4k      0  1.6k  150  0.01  81.0       0     16    29\n",
            "codex    explorer  gpt-test-1-mini      1   500      0     0   20  0.00  19.0       0      0    17\n",
        )
    );
    let out = stdout(sessions(&env, &["errors", "--root", "codex"]));
    assert!(
        out.contains("Bash: Script failed  cd /repo && cargo test"),
        "{out}"
    );
    // Sessions are labelled by the random end of their UUIDv7 thread id.
    assert_eq!(
        stdout(sessions(&env, &["friction", "--tsv", "--root", "codex"])),
        "2026-03-05\t-repo-app\t00000001\tdid you run it\n"
    );
}

#[test]
fn codex_cache_misses_forks_exits_and_exec_prompts() {
    let env = Env::new();
    let gaps = stdout(sessions(&env, &["cache-gaps", "--root", "codex-cases"]));
    assert!(
        gaps.starts_with(
            "Gaps over 5 min: 1; cache expired in 1; rewritten 100.0k tokens, about $0.20"
        ),
        "{gaps}"
    );
    // The fork's replayed calls, turn context and prompt were billed and typed in its parent;
    // only its own turn counts.
    let cost = stdout(sessions(
        &env,
        &["cost", "--by", "session", "--root", "codex-cases"],
    ));
    assert!(cost.contains("\n0000bbbb      1    1.0k"), "{cost}");
    let errors = stdout(sessions(&env, &["errors", "--root", "codex-cases"]));
    // Claude Code's "Exit code N" form, with the first output line; an exit line inside the
    // output of a command that exited 0 is not a failure.
    // Both of Codex's exit forms count, and so does a failed command inside an `exec` script.
    assert!(errors.starts_with("3 total in 2 sessions"), "{errors}");
    assert!(
        errors.contains("apply_patch: Exit code # | Operation not permitted"),
        "{errors}"
    );
    assert!(
        errors.contains("Bash: Exit code # | FAIL src/a.test.ts"),
        "{errors}"
    );
    assert!(
        errors.contains("Bash: Exit code # | boom ") && errors.contains("  make check\n"),
        "{errors}"
    );
    // A built-in subagent is grouped under its parent's session.
    let agents = stdout(sessions(
        &env,
        &["cost", "--by", "session,agent", "--root", "codex-cases"],
    ));
    assert!(agents.contains("\n0000cccc  guardian"), "{agents}");
    assert_eq!(
        stdout(sessions(
            &env,
            &["friction", "--tsv", "--root", "codex-cases"]
        )),
        ""
    );
}

#[test]
fn harness_picks_one_side_of_a_mixed_scan() {
    let env = Env::new();
    let both = ["--root", "projects", "--root", "codex"];
    let mut args = vec!["cost", "--harness", "claude"];
    args.extend(both);
    assert_eq!(stdout(sessions(&env, &args)), expected("cost"));
    let mut args = vec!["cost", "--by", "harness", "--harness", "codex"];
    args.extend(both);
    let out = stdout(sessions(&env, &args));
    assert!(
        out.contains("\ncodex        3") && !out.contains("claude"),
        "{out}"
    );
}

#[test]
fn reports_need_a_home_directory() {
    let env = Env::new();
    let mut command = sessions(&env, &["cost"]);
    command.env_remove("HOME");
    let (code, _, err) = run(command);
    assert_eq!(code, Some(1));
    assert!(err.contains("HOME is not set"), "{err}");
}

#[test]
fn bad_input_fails_with_a_reason() {
    let env = Env::new();
    let (code, _, err) = run(sessions(
        &env,
        &["cost", "--by", "colour", "--root", "projects"],
    ));
    assert_eq!(code, Some(1));
    assert!(err.contains("--by takes agent, model"), "{err}");
    let (code, _, err) = run(bare(&env, &["cost", "--since", "2026-02-30"]));
    assert_eq!(code, Some(1));
    assert!(err.contains("--since needs YYYY-MM-DD"), "{err}");
    let (code, _, err) = run(sessions(&env, &["cost", "/nonexistent/run.jsonl"]));
    assert_eq!(code, Some(1));
    assert!(err.contains("read /nonexistent/run.jsonl"), "{err}");
    let (code, _, err) = run(sessions(&env, &["cost", "projects"]));
    assert_eq!(code, Some(1));
    assert!(
        err.contains("not a file; --root takes a directory"),
        "{err}"
    );
    let mut missing = sessions(&env, &["cost", "--root", "projects"]);
    missing.env("AGT_PRICES", "/nonexistent/prices.json");
    let (code, _, err) = run(missing);
    assert_eq!(code, Some(1));
    assert!(
        err.contains("cannot read prices from /nonexistent/prices.json"),
        "{err}"
    );
}

#[test]
fn prices_come_from_the_agt_config_directory_else_the_older_agents_one() {
    let env = Env::new();
    let args = ["cost", "--root", "projects"];
    let wanted = run(sessions(&env, &args));
    assert_eq!(wanted.0, Some(0));
    let fixture = std::fs::read(fixtures().join("prices.json")).unwrap();
    let default_prices = |env: &Env| {
        let mut command = sessions(env, &args);
        command.env_remove("AGT_PRICES");
        run(command)
    };

    let (code, _, err) = default_prices(&env);
    assert_eq!(code, Some(1));
    assert!(
        err.contains("cannot read prices from") && err.contains("config/agt/model-prices.json"),
        "{err}"
    );

    let older = env.path("home/.config/agents");
    std::fs::create_dir_all(&older).unwrap();
    std::fs::write(older.join("model-prices.json"), &fixture).unwrap();
    let (code, _, err) = default_prices(&env);
    assert_eq!(code, Some(1), "the older location is no longer read");
    assert!(err.contains("config/agt/model-prices.json"), "{err}");

    let current = env.path("config/agt");
    std::fs::create_dir_all(&current).unwrap();
    std::fs::write(current.join("model-prices.json"), "{}").unwrap();
    let (code, _, err) = default_prices(&env);
    assert_eq!(code, Some(1));
    assert!(err.contains("config/agt/model-prices.json"), "{err}");
    std::fs::write(current.join("model-prices.json"), &fixture).unwrap();
    assert_eq!(default_prices(&env), wanted);
}

/// Links the fixture transcripts into the isolated home as Claude Code and Codex keep them.
fn link_transcripts(env: &Env) {
    let home = env.path("home");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::os::unix::fs::symlink(fixtures().join("projects"), home.join(".claude/projects")).unwrap();
    std::os::unix::fs::symlink(fixtures().join("codex"), home.join(".codex/sessions")).unwrap();
    std::os::unix::fs::symlink(
        fixtures().join("codex-cases"),
        home.join(".codex/archived_sessions"),
    )
    .unwrap();
}

#[test]
fn one_session_prices_with_its_subagents() {
    let env = Env::new();
    link_transcripts(&env);
    let by_session = |root: &str| {
        let json: serde_json::Value = serde_json::from_str(&stdout(sessions(
            &env,
            &["cost", "--by", "session", "--json", "--root", root],
        )))
        .unwrap();
        json["rows"].as_array().unwrap().clone()
    };
    // A Claude Code session's subagents are grouped under it.
    let claude = by_session("projects");
    let main = claude
        .iter()
        .find(|row| row["session"] == "aaaaaaaa")
        .expect("session row");
    assert_eq!(main["calls"], 10);
    // A Codex thread, and a built-in subagent's rollout that names only its parent thread.
    let codex = by_session("codex");
    let thread = codex
        .iter()
        .find(|row| row["session"] == "00000001")
        .expect("codex row");
    assert_eq!(thread["calls"], 3);
    let cases = by_session("codex-cases");
    let parent = cases
        .iter()
        .find(|row| row["session"] == "0000cccc")
        .expect("parent row");
    assert_eq!(parent["calls"], 2);
    assert_eq!(parent["input_tokens"], 15);
    assert_eq!(parent["output_tokens"], 2);
}

#[test]
fn combined_reports_match_standalone_sections() {
    let env = Env::new();
    for root in ["projects", "codex"] {
        let sections = [
            ("cost", "Cost"),
            ("tools", "Tools"),
            ("cache-gaps", "Cache gaps"),
            ("errors", "Errors"),
            ("denials", "Denials"),
            ("friction", "Friction"),
        ];
        let wanted = sections
            .iter()
            .map(|(command, heading)| {
                format!(
                    "{heading}:\n{}",
                    stdout(sessions(&env, &[command, "--root", root]))
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(stdout(sessions(&env, &["report", "--root", root])), wanted);
        let selected = stdout(sessions(
            &env,
            &[
                "report",
                "--sections",
                "denials,cost",
                "--root",
                root,
                "--project",
                "repo",
                "--limit",
                "2",
                "--by",
                "harness,model",
            ],
        ));
        let denials = stdout(sessions(
            &env,
            &[
                "denials",
                "--root",
                root,
                "--project",
                "repo",
                "--limit",
                "2",
            ],
        ));
        let cost = stdout(sessions(
            &env,
            &[
                "cost",
                "--root",
                root,
                "--project",
                "repo",
                "--limit",
                "2",
                "--by",
                "harness,model",
            ],
        ));
        assert_eq!(selected, format!("Denials:\n{denials}\nCost:\n{cost}"));
    }
}

#[test]
fn errors_and_denials_sections_split_the_failures() {
    let env = Env::new();
    let report = |args: &[&str]| {
        let mut command = sessions(&env, args);
        command.args(["--root", "projects"]);
        stdout(command)
    };
    let both = report(&["report", "--sections", "errors,denials"]);
    let errors = report(&["errors"]);
    let denials = report(&["denials"]);
    assert_eq!(both, format!("Errors:\n{errors}\nDenials:\n{denials}"));
    assert!(denials.contains("auto mode"), "{denials}");
    assert!(errors.contains("Exit code"), "{errors}");
    assert_ne!(errors, denials);
}

#[test]
fn selected_reports_do_not_require_unused_prices() {
    let env = Env::new();
    let mut command = sessions(
        &env,
        &[
            "report",
            "--sections",
            "tools,errors,denials,friction",
            "--root",
            "projects",
        ],
    );
    command.env("AGT_PRICES", "/nonexistent/prices.json");
    let actual = stdout(command);
    let wanted = [
        ("tools", "Tools"),
        ("errors", "Errors"),
        ("denials", "Denials"),
        ("friction", "Friction"),
    ]
    .into_iter()
    .map(|(name, heading)| format!("{heading}:\n{}", expected(name)))
    .collect::<Vec<_>>()
    .join("\n");
    assert_eq!(actual, wanted);
}
