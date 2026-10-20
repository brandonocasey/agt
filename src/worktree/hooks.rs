//! The `[worktree.hooks]` commands: each entry runs through `sh -c` in the worktree root, one
//! after another, stopping at the first failure.

use std::path::Path;

use serde_json::{Value, json};

use super::info::main_checkout;
use super::{port, prepare, trust};
use crate::core::config::Config;
use crate::core::error::Result;
use crate::core::shell::quote;
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

/// The hooks that apply to a repository: the global ones, plus the repository file's when it
/// is trusted. `untrusted` reports a repository file whose hooks were left out.
pub(super) struct Resolved {
    create: Vec<String>,
    remove: Vec<String>,
    pub(super) untrusted: Option<Value>,
}

impl Resolved {
    pub(super) fn commands(&self, kind: Kind) -> &[String] {
        match kind {
            Kind::Create => &self.create,
            Kind::Remove => &self.remove,
        }
    }

    /// Whether `create` has anything to run or to report.
    pub(super) fn is_empty(&self, kind: Kind) -> bool {
        self.commands(kind).is_empty() && self.untrusted.is_none()
    }
}

/// The hooks of the repository `main` belongs to.
pub(super) fn resolve(main: &Path) -> Result<Resolved> {
    let repo = Repo::at(main);
    // A bare main checkout has no `.config/agt.toml`, so only the global file applies.
    if repo.git(&["rev-parse", "--is-bare-repository"])? == "true" {
        let hooks = Config::load()?.worktree.hooks;
        return Ok(Resolved {
            create: hooks.create,
            remove: hooks.remove,
            untrusted: None,
        });
    }
    let merged = Config::load_for_repo(&repo)?.worktree.hooks;
    let file = trust::config_file(main);
    let bytes = std::fs::read(&file).unwrap_or_default();
    let symlink = trust::symlink_reason(main);
    if !trust::has_hooks(&bytes) || (symlink.is_none() && trust::is_trusted(main, &bytes)?) {
        return Ok(Resolved {
            create: merged.create,
            remove: merged.remove,
            untrusted: None,
        });
    }
    let own = Config::load_from(&file)?.worktree.hooks;
    let global = Config::load()?.worktree.hooks;
    let shown = file.canonicalize().unwrap_or(file);
    let mut untrusted = json!({
        "file": shown,
        "create": own.create,
        "remove": own.remove,
    });
    if let Some(reason) = symlink {
        untrusted["reason"] = json!(format!("{reason}; its hooks cannot be trusted"));
    } else {
        untrusted["next"] = json!(format!(
            "ask the user to review these commands; to trust them, they run: agt worktree trust {}",
            quote(&main.display().to_string())
        ));
    }
    Ok(Resolved {
        create: global.create,
        remove: global.remove,
        untrusted: Some(untrusted),
    })
}

/// Runs the `kind` hooks in the worktree `root`; the report is `{"create": [steps], "ok": bool}`
/// (keyed by the kind) and the flag says every step succeeded.
pub(super) fn run(root: &Path, kind: Kind) -> Result<(Value, bool)> {
    let main = main_checkout(&Repo::at(root))?.path;
    let resolved = resolve(&main)?;
    let commands = resolved.commands(kind);
    let branch = Repo::at(root).current_branch()?.unwrap_or_default();
    let mut env = vec![
        ("AGT_HOOK", kind.name().to_owned()),
        ("AGT_WORKTREE", root.to_string_lossy().into_owned()),
        ("AGT_MAIN_CHECKOUT", main.to_string_lossy().into_owned()),
        ("AGT_BRANCH", branch),
    ];
    // Read only when something runs, so a damaged ports file cannot block a hookless remove.
    if !commands.is_empty()
        && let Some(number) = port::recorded(root)?
    {
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
    let mut report = json!({kind.name(): steps, "ok": ok});
    if let Some(untrusted) = resolved.untrusted {
        report["untrusted"] = untrusted;
    }
    Ok((report, ok))
}
