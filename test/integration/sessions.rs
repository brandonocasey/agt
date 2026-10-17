//! `agt sessions` on fixture transcripts. The expected files are what the `agent-sessions`
//! script printed for the same fixtures, so Claude Code reports stay byte for byte the same.
//! `test/fixtures/sessions/generate.py` writes the fixtures; groups that span files have equal
//! examples and distinct counts, so the expected output holds in any directory order.

use std::path::{Path, PathBuf};

use assert_cmd::Command;

const SINCE: &str = "2026-02-15";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test/fixtures/sessions")
}

fn sessions(args: &[&str]) -> Command {
    let mut command = Command::cargo_bin("agt").expect("binary builds");
    command
        .current_dir(fixtures())
        .env("HOME", "/nonexistent")
        .env("AGT_PRICES", "prices.json")
        .env("TZ", "UTC")
        .arg("sessions")
        .args(args)
        .args(["--since", SINCE]);
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

fn expected(name: &str) -> String {
    std::fs::read_to_string(fixtures().join("expected").join(format!("{name}.txt")))
        .expect("expected output exists")
}

#[test]
fn claude_reports_match_the_script() {
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
        let output = sessions(&args).output().expect("agt runs");
        assert!(output.status.success(), "{name}: {output:?}");
        let mut actual = String::from_utf8_lossy(&output.stdout).into_owned();
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
    sessions(&["cost", "--root", "projects"])
        .assert()
        .success()
        .stderr("(skipped 1 scratch transcripts; --scratch includes them)\n");
}

#[test]
fn codex_rollouts_read_as_calls_tools_and_messages() {
    sessions(&["cost", "--by", "harness,agent,model", "--root", "codex"])
        .assert()
        .success()
        .stdout(concat!(
            "Total $0.01 since 2026-02-15\n",
            "harness  agent     model            calls    in  write  read  out   USD     %  write%  read%  out%\n",
            "-------  --------  ---------------  -----  ----  -----  ----  ---  ----  ----  ------  -----  ----\n",
            "codex    main      gpt-test-1           2  1.4k      0  1.6k  150  0.01  81.0       0     16    29\n",
            "codex    explorer  gpt-test-1-mini      1   500      0     0   20  0.00  19.0       0      0    17\n",
        ));
    let (code, out, _) = run(sessions(&["errors", "--root", "codex"]));
    assert_eq!(code, Some(0));
    assert!(
        out.contains("Bash: Script failed  cd /repo && cargo test"),
        "{out}"
    );
    sessions(&["friction", "--tsv", "--root", "codex"])
        .assert()
        .success()
        .stdout("2026-03-05\t-repo-app\t0199aaaa\tdid you run it\n");
}

#[test]
fn harness_picks_one_side_of_a_mixed_scan() {
    sessions(&[
        "cost",
        "--harness",
        "claude",
        "--root",
        "projects",
        "--root",
        "codex",
    ])
    .assert()
    .success()
    .stdout(expected("cost"));
    let (code, out, _) = run(sessions(&[
        "cost",
        "--by",
        "harness",
        "--harness",
        "codex",
        "--root",
        "projects",
        "--root",
        "codex",
    ]));
    assert_eq!(code, Some(0));
    assert!(
        out.contains("\ncodex        3") && !out.contains("claude"),
        "{out}"
    );
}

#[test]
fn bad_input_fails_with_a_reason() {
    let (code, _, err) = run(sessions(&["cost", "--by", "colour", "--root", "projects"]));
    assert_eq!(code, Some(1));
    assert!(err.contains("--by takes agent, model"), "{err}");
    let mut bad_date = Command::cargo_bin("agt").expect("binary builds");
    bad_date.args(["sessions", "cost", "--since", "2026-02-30"]);
    let (code, _, err) = run(bad_date);
    assert_eq!(code, Some(1));
    assert!(err.contains("--since needs YYYY-MM-DD"), "{err}");
    let mut missing = sessions(&["cost", "--root", "projects"]);
    missing.env("AGT_PRICES", "/nonexistent/prices.json");
    let (code, _, err) = run(missing);
    assert_eq!(code, Some(1));
    assert!(
        err.contains("cannot read prices from /nonexistent/prices.json"),
        "{err}"
    );
}
