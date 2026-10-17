//! `agt sessions` on fixture transcripts. The expected files are what the `agent-sessions`
//! script printed for the same fixtures, so Claude Code reports stay byte for byte the same.
//! `test/fixtures/sessions/generate.py` writes the fixtures; groups that span files have equal
//! examples and distinct counts, so the expected output holds in any directory order.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::support::Env;

const SINCE: &str = "2026-02-15";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test/fixtures/sessions")
}

/// `agt sessions` in the isolated environment, reading fixtures by relative path.
fn bare(env: &Env, args: &[&str]) -> Command {
    let mut command = env.command(env!("CARGO_BIN_EXE_agt"), &fixtures());
    command
        .env("AGT_PRICES", fixtures().join("prices.json"))
        .env("TZ", "UTC")
        .arg("sessions")
        .args(args);
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
fn claude_reports_match_the_script() {
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
        ("cost --top 2", "cost_--top_2"),
        ("agents", "agents"),
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
    // Counted whatever its age, as the script did: before the date check.
    let root = env.path("roots");
    std::fs::create_dir_all(root.join("-tmp-probe")).unwrap();
    std::fs::write(root.join("-tmp-probe/old.jsonl"), "{}\n").unwrap();
    let mut late = bare(&env, &["cost", "--since", "2099-01-01", "--root"]);
    late.arg(&root);
    let (code, _, err) = run(late);
    assert_eq!(code, Some(0));
    assert_eq!(
        err,
        "(skipped 1 scratch transcripts; --scratch includes them)\n"
    );
    // Outside `--project`, it is not counted at all.
    let mut other = bare(
        &env,
        &[
            "cost",
            "--since",
            "2099-01-01",
            "--project",
            "repo",
            "--root",
        ],
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
    let mut missing = sessions(&env, &["cost", "--root", "projects"]);
    missing.env("AGT_PRICES", "/nonexistent/prices.json");
    let (code, _, err) = run(missing);
    assert_eq!(code, Some(1));
    assert!(
        err.contains("cannot read prices from /nonexistent/prices.json"),
        "{err}"
    );
}
