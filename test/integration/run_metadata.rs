//! The optional analysis fields of `pass start` (`--task-type`, `--risk`, `--implementer`) and
//! `qa run start` (`--implementer`, `--round`): stored in the target document, shown with the
//! runs, rejected when invalid or when a later pass contradicts its run, and absent from
//! documents written without them.

use std::fs;
use std::process::Output;

use serde_json::Value;

use crate::support::{Env, numbered};

fn changed_branch() -> Env {
    let env = Env::with_branch().offline();
    env.write("a.txt", &numbered(10, &[(3, "three")]));
    env.commit_all("change");
    env
}

fn qa_exec(env: &Env, args: &[&str]) -> Output {
    env.bin(&env.repo).arg("qa").args(args).output().unwrap()
}

fn qa(env: &Env, args: &[&str]) -> Value {
    let output = qa_exec(env, args);
    assert!(
        output.status.success(),
        "agt qa {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Rewrites the only stored target as `edit` changes its JSON.
fn edit_document(env: &Env, edit: impl FnOnce(&mut Value)) {
    let path = fs::read_dir(env.state())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("branch-") && name.ends_with(".json"))
        })
        .expect("a stored target");
    let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    edit(&mut document);
    fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
}

fn pass_start(env: &Env, extra: &[&str]) -> Output {
    let mut args = vec!["pass", "start", "--kind", "full"];
    args.extend(extra);
    env.exec(&args, None, &[])
}

fn json(output: &Output) -> Value {
    assert!(output.status.success(), "failed: {}", stderr(output));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn end_pass(env: &Env, run: &str, pass: &str) {
    env.run(&["pass", "end", "--run", run, "--pass", pass]);
}

#[test]
fn review_fields_are_stored_and_shown_where_runs_print() {
    let env = changed_branch();
    let started = json(&pass_start(
        &env,
        &[
            "--task-type",
            "bug",
            "--risk",
            "medium",
            "--implementer",
            "claude-sonnet-5-5",
        ],
    ));
    let run = started["run"].as_str().unwrap().to_owned();
    assert_eq!(started["pass"]["risk"], "medium");
    assert!(started["pass"].get("task_type").is_none());
    end_pass(&env, &run, "c1p0");

    // A later pass repeats the run-level fields and sets its own risk.
    let second = env.run(&[
        "pass",
        "start",
        "--run",
        &run,
        "--kind",
        "fix-check",
        "--task-type",
        "bug",
        "--implementer",
        "claude-sonnet-5-5",
        "--risk",
        "high",
    ]);
    assert_eq!(second["pass"]["risk"], "high");
    end_pass(&env, &run, "c1p1");

    let shown = env.run(&["show", &run]);
    assert_eq!(shown["run"]["task_type"], "bug");
    assert_eq!(shown["run"]["implementer"], "claude-sonnet-5-5");
    assert!(shown["run"].get("risk").is_none());
    let risks: Vec<&Value> = shown["run"]["passes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pass| &pass["risk"])
        .collect();
    assert_eq!(risks, ["medium", "high"]);

    let pass = env.run(&["show", &format!("{run}.c1p0")]);
    assert_eq!(pass["pass"]["risk"], "medium");

    let status = env.run(&["status", "--full"]);
    let listed = &status["runs"][0];
    assert_eq!(listed["task_type"], "bug");
    assert_eq!(listed["implementer"], "claude-sonnet-5-5");
}

#[test]
fn every_task_type_and_risk_is_accepted() {
    for (index, (task_type, risk)) in [
        ("design", "low"),
        ("bug", "medium"),
        ("feature", "high"),
        ("fix", "low"),
        ("other", "medium"),
    ]
    .into_iter()
    .enumerate()
    {
        let env = changed_branch();
        let started = json(&pass_start(
            &env,
            &["--task-type", task_type, "--risk", risk],
        ));
        let run = started["run"].as_str().unwrap();
        let shown = env.run(&["show", run]);
        assert_eq!(shown["run"]["task_type"], task_type, "case {index}");
        assert_eq!(shown["run"]["passes"][0]["risk"], risk, "case {index}");
    }
}

#[test]
fn absent_review_fields_print_nothing() {
    let env = changed_branch();
    let started = json(&pass_start(&env, &[]));
    let run = started["run"].as_str().unwrap().to_owned();
    for field in ["risk", "task_type", "implementer"] {
        assert!(started["pass"].get(field).is_none(), "{field}");
    }
    let shown = env.run(&["show", &run]);
    for field in ["task_type", "implementer"] {
        assert!(shown["run"].get(field).is_none(), "{field}");
    }
    assert!(shown["run"]["passes"][0].get("risk").is_none());
    let status = env.run(&["status", "--full"]);
    for field in ["task_type", "implementer"] {
        assert!(status["runs"][0].get(field).is_none(), "{field}");
    }
}

#[test]
fn invalid_review_values_are_rejected_and_start_nothing() {
    let env = changed_branch();
    for args in [
        ["--task-type", "chore"],
        ["--task-type", "Bug"],
        ["--risk", "critical"],
    ] {
        let output = pass_start(&env, &args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
        assert!(stderr(&output).contains("possible values"), "{args:?}");
    }
    for implementer in ["", "  ", "two\nlines"] {
        let output = pass_start(&env, &["--implementer", implementer]);
        assert_eq!(output.status.code(), Some(1), "{implementer:?}");
        assert!(stderr(&output).contains("--implementer"), "{implementer:?}");
    }
    assert!(
        env.run(&["status", "--full"])["runs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_later_pass_may_not_contradict_the_run_level_fields() {
    let env = changed_branch();
    let started = json(&pass_start(
        &env,
        &["--task-type", "feature", "--implementer", "model-a"],
    ));
    let run = started["run"].as_str().unwrap().to_owned();
    end_pass(&env, &run, "c1p0");

    let start = |extra: &[&str]| {
        let mut args = vec!["pass", "start", "--run", &run, "--kind", "fix-check"];
        args.extend(extra);
        env.exec(&args, None, &[])
    };
    let wrong_type = start(&["--task-type", "bug"]);
    assert_eq!(wrong_type.status.code(), Some(1));
    assert!(stderr(&wrong_type).contains("task type feature"));
    let wrong_model = start(&["--implementer", "model-b"]);
    assert_eq!(wrong_model.status.code(), Some(1));
    assert!(stderr(&wrong_model).contains("implementer model-a"));
    // Nothing was written by the failed starts.
    let shown = env.run(&["show", &run]);
    assert_eq!(shown["run"]["passes"].as_array().unwrap().len(), 1);

    // Matching and absent values both pass, and a changed risk is fine.
    assert!(
        start(&["--task-type", "feature", "--risk", "low"])
            .status
            .success()
    );
}

#[test]
fn a_later_pass_cannot_add_a_run_field_the_run_lacks() {
    let env = changed_branch();
    let started = json(&pass_start(&env, &[]));
    let run = started["run"].as_str().unwrap().to_owned();
    end_pass(&env, &run, "c1p0");
    let output = env.exec(
        &[
            "pass",
            "start",
            "--run",
            &run,
            "--kind",
            "fix-check",
            "--task-type",
            "bug",
        ],
        None,
        &[],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("has no task type"));
}

/// Every file under the state directory, by name, so a test can see that nothing was written.
fn stored_files(env: &Env) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = fs::read_dir(env.state())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .map(|path| {
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(&path).unwrap(),
            )
        })
        .collect();
    files.sort();
    files
}

#[test]
fn pass_start_dry_run_refuses_what_a_real_start_refuses() {
    let env = changed_branch();
    let started = json(&pass_start(
        &env,
        &["--task-type", "feature", "--implementer", "model-a"],
    ));
    let run = started["run"].as_str().unwrap().to_owned();
    end_pass(&env, &run, "c1p0");
    let before = stored_files(&env);

    for extra in [
        vec!["--task-type", "bug"],
        vec!["--implementer", "model-b"],
        vec!["--implementer", ""],
        vec!["--implementer", "two\nlines"],
    ] {
        let mut args = vec!["pass", "start", "--run", &run, "--kind", "fix-check"];
        args.extend(&extra);
        let real = env.exec(&args, None, &[]);
        assert_eq!(real.status.code(), Some(1), "{extra:?} real");
        args.push("--dry-run");
        let dry = env.exec(&args, None, &[]);
        assert_eq!(
            dry.status.code(),
            Some(1),
            "{extra:?} dry: {}",
            stderr(&dry)
        );
        assert_eq!(stderr(&dry), stderr(&real), "{extra:?}");
        assert!(dry.stdout.is_empty(), "{extra:?}");
    }
    // An invalid implementer is refused for a run-starting dry run too.
    let dry = env.exec(
        &[
            "pass",
            "start",
            "--kind",
            "full",
            "--implementer",
            " ",
            "--dry-run",
        ],
        None,
        &[],
    );
    assert_eq!(dry.status.code(), Some(1), "{}", stderr(&dry));
    assert!(stderr(&dry).contains("--implementer"));
    assert_eq!(stored_files(&env), before, "a refused dry run wrote");
}

#[test]
fn pass_start_dry_run_accepts_valid_fields_and_writes_nothing() {
    let env = changed_branch();
    let started = json(&pass_start(
        &env,
        &["--task-type", "feature", "--implementer", "model-a"],
    ));
    let run = started["run"].as_str().unwrap().to_owned();
    end_pass(&env, &run, "c1p0");
    let before = stored_files(&env);

    // Matching values, no fields, and a first pass with fields all pass.
    let matching = [
        "pass",
        "start",
        "--run",
        run.as_str(),
        "--kind",
        "fix-check",
        "--task-type",
        "feature",
        "--implementer",
        " model-a ",
        "--risk",
        "high",
        "--dry-run",
    ];
    let absent = [
        "pass",
        "start",
        "--run",
        run.as_str(),
        "--kind",
        "fix-check",
        "--dry-run",
    ];
    let first = [
        "pass",
        "start",
        "--kind",
        "full",
        "--task-type",
        "bug",
        "--implementer",
        "model-b",
        "--dry-run",
    ];
    for args in [&matching[..], &absent[..], &first[..]] {
        let output = env.exec(args, None, &[]);
        assert_eq!(json(&output)["dry_run"], true, "{args:?}");
    }
    assert_eq!(stored_files(&env), before, "a dry run wrote");
}

#[test]
fn a_run_field_the_run_lacks_is_described_as_missing() {
    let env = changed_branch();
    let started = json(&pass_start(&env, &[]));
    let run = started["run"].as_str().unwrap().to_owned();
    end_pass(&env, &run, "c1p0");
    let start = |extra: &[&str]| {
        let mut args = vec!["pass", "start", "--run", &run, "--kind", "fix-check"];
        args.extend(extra);
        env.exec(&args, None, &[])
    };
    for (flag, value, name) in [
        ("--task-type", "bug", "task type"),
        ("--implementer", "model-a", "implementer"),
    ] {
        let message = stderr(&start(&[flag, value]));
        assert!(
            message.contains(&format!("run {run} has no {name}")),
            "{message}"
        );
        assert!(
            message.contains(&format!("only the pass that starts the run can set {flag}")),
            "{message}"
        );
        assert!(!message.contains("none"), "{message}");
    }
}

#[test]
fn a_failed_first_pass_leaves_no_run_with_fields() {
    let env = changed_branch();
    let output = pass_start(
        &env,
        &[
            "--task-type",
            "bug",
            "--reviewers",
            "/nonexistent/reviewers.json",
        ],
    );
    assert!(!output.status.success());
    assert!(
        env.run(&["status", "--full"])["runs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_review_document_without_the_fields_still_loads_and_takes_them() {
    let env = changed_branch();
    let started = json(&pass_start(
        &env,
        &["--task-type", "bug", "--risk", "low", "--implementer", "m"],
    ));
    let run = started["run"].as_str().unwrap().to_owned();
    end_pass(&env, &run, "c1p0");
    // The document as an earlier build wrote it: none of the new keys.
    edit_document(&env, |document| {
        let run = &mut document["runs"][0];
        run.as_object_mut().unwrap().remove("task_type");
        run.as_object_mut().unwrap().remove("implementer");
        run["passes"][0].as_object_mut().unwrap().remove("risk");
    });
    let shown = env.run(&["show", &run]);
    assert!(shown["run"].get("task_type").is_none());
    assert!(shown["run"]["passes"][0].get("risk").is_none());
    assert_eq!(
        env.run(&["status", "--full"])["runs"][0]["id"],
        run.as_str()
    );
    // It takes writes, and a later pass without fields leaves it without.
    let next = env.run(&[
        "pass",
        "start",
        "--run",
        &run,
        "--kind",
        "fix-check",
        "--risk",
        "high",
    ]);
    assert_eq!(next["pass"]["risk"], "high");
    let shown = env.run(&["show", &run]);
    assert!(shown["run"].get("task_type").is_none());
    assert!(shown["run"]["passes"][0].get("risk").is_none());
    assert_eq!(shown["run"]["passes"][1]["risk"], "high");
}

#[test]
fn unset_review_fields_are_not_written_to_the_document() {
    let env = changed_branch();
    json(&pass_start(&env, &[]));
    edit_document(&env, |document| {
        let text = document.to_string();
        for key in ["task_type", "implementer", "risk", "round"] {
            assert!(!text.contains(&format!("\"{key}\"")), "{key} was written");
        }
    });
}

#[test]
fn qa_fields_are_stored_and_shown_where_qa_runs_print() {
    let env = changed_branch();
    let started = qa(
        &env,
        &[
            "run",
            "start",
            "--implementer",
            "claude-sonnet-5-5",
            "--round",
            "2",
        ],
    );
    let run = started["run"].as_str().unwrap().to_owned();
    assert_eq!(started["implementer"], "claude-sonnet-5-5");
    assert_eq!(started["round"], 2);
    qa(
        &env,
        &[
            "check",
            "--run",
            &run,
            "--name",
            "login",
            "--outcome",
            "pass",
        ],
    );

    let shown = qa(&env, &["show", &run]);
    assert_eq!(shown["run"]["implementer"], "claude-sonnet-5-5");
    assert_eq!(shown["run"]["round"], 2);

    let list = qa(&env, &["list"]);
    assert_eq!(list["runs"][0]["id"], run.as_str());
    assert_eq!(list["runs"][0]["implementer"], "claude-sonnet-5-5");
    assert_eq!(list["runs"][0]["round"], 2);

    let ended = qa(&env, &["run", "end", "--run", &run]);
    let listed = &ended["status"]["qa"]["runs"][0];
    assert_eq!(listed["round"], 2);
    assert_eq!(listed["implementer"], "claude-sonnet-5-5");
}

#[test]
fn absent_qa_fields_print_nothing() {
    let env = changed_branch();
    let started = qa(&env, &["run", "start"]);
    let run = started["run"].as_str().unwrap().to_owned();
    for field in ["implementer", "round"] {
        assert!(started.get(field).is_none(), "{field}");
    }
    let shown = qa(&env, &["show", &run]);
    let list = qa(&env, &["list"]);
    for field in ["implementer", "round"] {
        assert!(shown["run"].get(field).is_none(), "{field}");
        assert!(list["runs"][0].get(field).is_none(), "{field}");
    }
}

#[test]
fn invalid_qa_values_are_rejected() {
    let env = changed_branch();
    for round in ["0", "-1", "two", "1.5", ""] {
        let output = qa_exec(&env, &["run", "start", "--round", round]);
        assert_eq!(output.status.code(), Some(2), "round {round:?}");
    }
    for implementer in ["", "   ", "a\nb"] {
        let output = qa_exec(&env, &["run", "start", "--implementer", implementer]);
        assert_eq!(output.status.code(), Some(1), "{implementer:?}");
        assert!(stderr(&output).contains("--implementer"), "{implementer:?}");
    }
    assert!(qa(&env, &["list"])["runs"].as_array().unwrap().is_empty());
}

#[test]
fn a_qa_document_without_the_fields_still_loads() {
    let env = changed_branch();
    let started = qa(
        &env,
        &["run", "start", "--implementer", "m", "--round", "1"],
    );
    let run = started["run"].as_str().unwrap().to_owned();
    edit_document(&env, |document| {
        let run = document["qa_runs"][0].as_object_mut().unwrap();
        run.remove("implementer");
        run.remove("round");
    });
    let shown = qa(&env, &["show", &run]);
    assert!(shown["run"].get("round").is_none());
    qa(
        &env,
        &[
            "check",
            "--run",
            &run,
            "--name",
            "login",
            "--outcome",
            "pass",
        ],
    );
    let list = qa(&env, &["list"]);
    assert_eq!(list["runs"][0]["id"], run.as_str());
    assert!(list["runs"][0].get("implementer").is_none());
}

#[test]
fn help_describes_the_fields() {
    let env = changed_branch();
    let review = env.text(&["pass", "start", "--help"]);
    for text in ["--task-type", "--risk", "--implementer", "design", "high"] {
        assert!(review.contains(text), "{text}");
    }
    let output = qa_exec(&env, &["run", "start", "--help"]);
    let help = String::from_utf8_lossy(&output.stdout).into_owned();
    for text in ["--implementer", "--round"] {
        assert!(help.contains(text), "{text}");
    }
}

/// A target written without `issue` (any run, QA run or check started without the flag) loads,
/// takes more writes, and never gains the key.
#[test]
fn a_target_without_issue_fields_loads_and_stays_without_them() {
    let env = changed_branch();
    let started = json(&pass_start(&env, &[]));
    let run = started["run"].as_str().unwrap().to_owned();
    let qa_run = qa(&env, &["run", "start"])["run"]
        .as_str()
        .unwrap()
        .to_owned();
    qa(
        &env,
        &[
            "check",
            "--run",
            &qa_run,
            "--name",
            "one",
            "--outcome",
            "pass",
        ],
    );
    // Written by this build and read back by it, as a store from before the field would be.
    edit_document(&env, |document| {
        assert!(document["runs"][0].get("issue").is_none(), "{document}");
        assert!(document["qa_runs"][0].get("issue").is_none(), "{document}");
        assert!(
            document["qa_runs"][0]["checks"][0].get("issue").is_none(),
            "{document}"
        );
    });
    qa(
        &env,
        &[
            "check",
            "--run",
            &qa_run,
            "--name",
            "two",
            "--outcome",
            "fail",
        ],
    );
    end_pass(&env, &run, "c1p0");
    assert!(env.run(&["show", &run])["run"].get("issue").is_none());
    assert!(qa(&env, &["list"])["runs"][0].get("issue").is_none());
    edit_document(&env, |document| {
        assert!(document["runs"][0].get("issue").is_none(), "{document}");
        assert!(
            document["qa_runs"][0]["checks"][1].get("issue").is_none(),
            "{document}"
        );
    });
}
