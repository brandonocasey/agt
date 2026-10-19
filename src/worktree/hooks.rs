//! The `[worktree.hooks]` commands: each entry runs through `sh -c` in the worktree root, one
//! after another, stopping at the first failure.

use std::path::Path;

use serde_json::{Value, json};

use super::info::main_checkout;
use super::{port, prepare};
use crate::core::config::Config;
use crate::core::error::Result;
use crate::git::Repo;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Create,
    Remove,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Remove => "remove",
        }
    }
}

/// The configured commands of `kind` for the repository `main` belongs to.
pub(super) fn configured(main: &Path, kind: Kind) -> Result<Vec<String>> {
    let repo = Repo::at(main);
    // A bare main checkout has no `.config/agt.toml`, so only the global file applies.
    let config = if repo.git(&["rev-parse", "--is-bare-repository"])? == "true" {
        Config::load()?
    } else {
        Config::load_for_repo(&repo)?
    };
    let hooks = config.worktree.hooks;
    Ok(match kind {
        Kind::Create => hooks.create,
        Kind::Remove => hooks.remove,
    })
}

/// Runs the `kind` hooks in the worktree `root`; the report is `{"create": [steps], "ok": bool}`
/// (keyed by the kind) and the flag says every step succeeded.
pub(super) fn run(root: &Path, kind: Kind) -> Result<(Value, bool)> {
    let main = main_checkout(&Repo::at(root))?.path;
    let commands = configured(&main, kind)?;
    let branch = Repo::at(root)
        .git_opt(&["symbolic-ref", "--quiet", "--short", "HEAD"])?
        .unwrap_or_default();
    let mut env = vec![
        ("AGT_HOOK", kind.name().to_owned()),
        ("AGT_WORKTREE", root.to_string_lossy().into_owned()),
        ("AGT_MAIN_CHECKOUT", main.to_string_lossy().into_owned()),
        ("AGT_BRANCH", branch),
    ];
    if let Some(number) = port::recorded(root)? {
        env.push(("PORT", number.to_string()));
    }
    let mut steps = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let argv = ["sh".to_owned(), "-c".to_owned(), command.clone()];
        let mut step = prepare::run_step(root, &argv, index, &env)?;
        step["command"] = json!(command);
        let failed = step["ok"] != true;
        steps.push(step);
        if failed {
            break;
        }
    }
    let ok = steps.iter().all(|step| step["ok"] == true);
    Ok((json!({kind.name(): steps, "ok": ok}), ok))
}
